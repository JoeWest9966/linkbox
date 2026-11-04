//! ssh command assembly.
//!
//! Three flavours are built here:
//!   * the long-lived forwarder (`ssh -R ... <lease cmd>`) owned by the
//!     supervisor — it runs the remote lease shell instead of `-N` so the port
//!     is leased, not just grabbed
//!   * the probe (`ssh ... remote-cmd`) which reuses the forwarder's
//!     ControlMaster socket so a health check costs a few ms instead of a
//!     fresh handshake through a SOCKS hop
//!   * the reclaim (`ssh ... remote-cmd`, no `-R`) which frees a port left
//!     bound by our own previous session

use crate::config::{Defaults, ResolvedTunnel};
use std::path::{Path, PathBuf};

pub struct Plan {
    pub args: Vec<String>,
    /// Human-readable summary shown in the panel.
    pub remote_spec: String,
}

fn cm_path(state_dir: &Path, name: &str) -> PathBuf {
    // ControlPath has a length limit (~108 bytes) and names are validated to
    // [a-z0-9_-]{1,32}, so a flat path here stays well under it.
    state_dir.join(format!("cm-{name}.sock"))
}

pub fn control_socket(state_dir: &Path, name: &str) -> PathBuf {
    cm_path(state_dir, name)
}

/// Remove a stale mux socket. `ControlPersist=yes` leaves the master socket
/// behind after the process dies; if we let the next ssh see it, it reports
/// "already exists, disabling multiplexing" and then `ExitOnForwardFailure`
/// turns a healthy tunnel into a restart loop.
pub fn clear_control_socket(state_dir: &Path, name: &str) {
    let cm = cm_path(state_dir, name);
    let _ = std::fs::remove_file(&cm);
    // ssh also writes a small perm file alongside the socket
    let perm = cm.with_extension("sock-perm");
    let _ = std::fs::remove_file(perm);
}

/// Options shared by every ssh we spawn. Everything here is about *how to
/// reach and authenticate to* the remote; the caller adds the per-purpose bits
/// (control socket, `-R`, the command).
fn common_args(
    t: &ResolvedTunnel,
    d: &Defaults,
    state_dir: &Path,
    self_exe: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-T".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        // NOTE: ssh keeps the FIRST value it sees for an option, so never emit
        // a ServerAliveInterval=0 ahead of the real one or keepalive is dead.
        "-o".into(),
        format!("ServerAliveInterval={}", d.server_alive_interval),
        "-o".into(),
        format!("ServerAliveCountMax={}", d.server_alive_count_max),
        "-o".into(),
        format!("ConnectTimeout={}", d.connect_timeout_secs),
        "-o".into(),
        format!("StrictHostKeyChecking={}", d.strict_host_key_checking),
        "-o".into(),
        format!("UserKnownHostsFile={}", state_dir.join("known_hosts").display()),
        "-o".into(),
        "PasswordAuthentication=no".into(),
    ];
    if let Some(id) = &t.identity {
        args.push("-o".into());
        args.push("IdentitiesOnly=yes".into());
        args.push("-i".into());
        args.push(id.display().to_string());
    }
    if let Some(socks) = &t.via_socks {
        args.push("-o".into());
        args.push(format!(
            "ProxyCommand='{}' proxy-stdio --socks {} --target %h:%p",
            self_exe.display(),
            socks
        ));
    }
    args
}

/// A short-lived ssh that runs `remote_cmd` and exits. Used for lease reclaim:
/// it must never share the forwarder's ControlPath, or a dead mux would make the
/// cleanup fail exactly when it is needed.
pub fn remote_command_args(
    t: &ResolvedTunnel,
    d: &Defaults,
    state_dir: &Path,
    self_exe: &Path,
    remote_cmd: String,
) -> Vec<String> {
    let mut args = common_args(t, d, state_dir, self_exe);
    args.push("-o".into());
    args.push("ControlMaster=no".into());
    args.push("-o".into());
    args.push("ControlPersist=no".into());
    args.push("-o".into());
    args.push(format!(
        "ControlPath={}",
        state_dir.join(format!("cm-{}-r.sock", t.name)).display()
    ));
    args.push("-p".into());
    args.push(t.port.to_string());
    args.push(format!("{}@{}", t.user, t.host));
    args.push(remote_cmd);
    args
}

pub fn forwarder_plan(
    t: &ResolvedTunnel,
    d: &Defaults,
    state_dir: &Path,
    self_exe: &Path,
    lease_cmd: String,
) -> Plan {
    let cm = cm_path(state_dir, &t.name);
    let mut args = common_args(t, d, state_dir, self_exe);
    // ControlPersist=yes makes the master daemonize itself and the foreground
    // ssh exits 0 — the port stays bound by an orphan we no longer own, and
    // every respawn then collides with it. The socket must live and die with
    // THIS process.
    args.push("-o".into());
    args.push("ControlMaster=yes".into());
    args.push("-o".into());
    args.push(format!("ControlPath={}", cm.display()));
    args.push("-o".into());
    args.push("ControlPersist=no".into());
    // THE load-bearing flag: without it, ssh logs in fine when the remote port
    // is taken and then forwards nothing while the panel shows green.
    args.push("-o".into());
    args.push("ExitOnForwardFailure=yes".into());
    args.push("-p".into());
    args.push(t.port.to_string());
    args.push("-R".into());
    args.push(t.forward_spec());
    args.push(format!("{}@{}", t.user, t.host));
    // Not `-N`: the remote side writes the port lease and then parks, which
    // keeps the session (and therefore the forward) alive just the same.
    args.push(lease_cmd);

    Plan {
        remote_spec: t.forward_spec(),
        args,
    }
}

pub fn probe_args(t: &ResolvedTunnel, state_dir: &Path) -> Vec<String> {
    let cm = cm_path(state_dir, &t.name);
    vec![
        "-o".into(),
        format!("ControlPath={}", cm.display()),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "ControlPersist=no".into(),
        "-o".into(),
        format!("ConnectTimeout={}", t.port.max(5).min(10)),
        "-p".into(),
        t.port.to_string(),
        format!("{}@{}", t.user, t.host),
        t.health_command(),
    ]
}
