//! SOCKS5 stdio bridge used as ssh's `ProxyCommand`.
//!
//! Why this exists: Debian ships GNU netcat (`nc` without `-X`), and machines
//! that run a local SOCKS inbound (sing-box, xray, v2ray...) rarely have a
//! working SOCKS client on PATH. Rather than depend on one, linkbox *is* the
//! client: ssh spawns `linkbox proxy-stdio --socks 127.0.0.1:31002 --target %h:%p`
//! and we bridge stdin/stdout to the target through the SOCKS hop.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};

const SOCKS5: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const CMD_CONNECT: u8 = 0x01;
const ATYP_V4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_V6: u8 = 0x04;

pub fn bridge(socks: &str, target: &str) -> Result<()> {
    let socks_addr: SocketAddr = socks
        .to_socket_addrs()
        .with_context(|| format!("resolve socks {socks}"))?
        .next()
        .context("socks address resolved to nothing")?;

    let mut s = TcpStream::connect_timeout(&socks_addr, std::time::Duration::from_secs(15))
        .with_context(|| format!("connect socks {socks_addr}"))?;
    s.set_nodelay(true).ok();

    // greeting: version 5, one method: no-auth
    s.write_all(&[SOCKS5, 0x01, AUTH_NONE])?;
    s.flush()?;
    let mut resp = [0u8; 2];
    s.read_exact(&mut resp).context("socks greeting reply")?;
    if resp[0] != SOCKS5 {
        bail!("socks server replied version {:#04x}, expected 5", resp[0]);
    }
    if resp[1] != AUTH_NONE {
        bail!("socks server requires auth method {:#04x}; only no-auth is supported", resp[1]);
    }

    let (host, port) = split_host_port(target)?;
    let mut req = Vec::with_capacity(4 + 260 + 2);
    req.extend_from_slice(&[SOCKS5, CMD_CONNECT, 0x00]);
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            req.push(ATYP_V4);
            req.extend_from_slice(&ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            req.push(ATYP_V6);
            req.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > 255 {
                bail!("target host too long for socks domain name");
            }
            req.push(ATYP_DOMAIN);
            req.push(host.len() as u8);
            req.extend_from_slice(host.as_bytes());
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req)?;
    s.flush()?;

    // reply header is 4 bytes, then bind addr whose length depends on atyp
    let mut head = [0u8; 4];
    s.read_exact(&mut head).context("socks connect reply")?;
    if head[1] != 0x00 {
        bail!("socks connect failed: rep={:#04x} ({})", head[1], socks_reply_name(head[1]));
    }
    let skip = match head[3] {
        ATYP_V4 => 4,
        ATYP_V6 => 16,
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len)?;
            len[0] as usize
        }
        other => bail!("socks reply has unknown address type {:#04x}", other),
    };
    let mut pad = vec![0u8; skip + 2];
    s.read_exact(&mut pad).context("socks reply bind address")?;

    // bridge both directions until the *socket* hangs up. stdin EOF is normal
    // (ssh gives ProxyCommand a closed stdin under -N); it must only half-close
    // the socket, never tear down the tunnel.
    let mut writer = s.try_clone()?;
    let mut reader = s;
    let up = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    writer.flush().ok();
                }
            }
        }
        // Do NOT shutdown(SHUT_WR) here. ssh may still send data on this
        // channel, and half-closing makes the remote sshd close the session,
        // which shows up as a silent `ssh exited: exit status: 0`.
    });
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                stdout.flush().ok();
            }
        }
    }
    let _ = up.join();
    Ok(())
}

fn split_host_port(target: &str) -> Result<(String, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        // [v6]:port
        let (h, p) = rest.split_once("]:").context("bad [v6]:port target")?;
        let port: u16 = p.parse().context("bad target port")?;
        return Ok((h.to_string(), port));
    }
    let (h, p) = target
        .rsplit_once(':')
        .context("target must be host:port")?;
    let port: u16 = p.parse().context("bad target port")?;
    Ok((h.to_string(), port))
}

fn socks_reply_name(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "ttl expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown",
    }
}
