//! Remote port leases.
//!
//! A reverse forward is not ours to take — it is a resource sitting on somebody
//! else's machine, and it outlives our local process. Kill the client hard and
//! the remote sshd keeps `127.0.0.1:<port>` bound until its `ClientAliveInterval`
//! gives up (30 minutes on a stock Debian). Every redial in that window fails
//! with `remote port forwarding failed`, which looks like a network problem and
//! is not.
//!
//! So the port is *leased*, not grabbed:
//!
//!   * the forwarder runs a tiny remote command instead of `ssh -N`; that
//!     command records `port + owning pid + timestamp` under a fingerprint made
//!     of this instance's id and the tunnel name;
//!   * before blaming the network, linkbox reads the lease back and reclaims
//!     only ports whose owner pid matches the lease *and* is still an sshd.
//!
//! Anything that does not match the lease belongs to someone else and is left
//! strictly alone — reporting a conflict is always better than killing a
//! process we cannot identify.

use std::path::Path;

/// Where leases live on the remote. `/tmp` is fine: it is writable, and if it
/// is wiped on reboot the sshd that held the port is gone too.
pub const REMOTE_DIR: &str = "/tmp/linkbox";

/// Stable per-installation identity that survives reboots.
///
/// Derived from `/etc/machine-id` plus the config path, so it needs no state
/// file (a tmpfs state dir would change on reboot and orphan every lease) and
/// two linkbox instances on one host still get distinct ids.
pub fn instance_id(config_path: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let machine = std::fs::read_to_string("/etc/machine-id")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        });
    let mut h = std::collections::hash_map::DefaultHasher::new();
    machine.hash(&mut h);
    config_path.hash(&mut h);
    format!("{:012x}", h.finish() & 0xffff_ffff_ffff)
}

/// `{instance}-{tunnel}`. Tunnel names are validated to `[a-z0-9_-]{1,32}`, so
/// this is safe as a filename.
pub fn fingerprint(instance: &str, tunnel: &str) -> String {
    format!("{instance}-{tunnel}")
}

fn lease_file(fp: &str) -> String {
    format!("{REMOTE_DIR}/{fp}.lease")
}

/// Shell that finds the pid currently owning `:<port>`.
///
/// `ss` is in iproute2, present on anything modern; when it is missing we fall
/// back to `$PPID` and the reclaim step will simply refuse to kill (it demands
/// an exact match), so a wrong guess degrades to `conflict`, never to a
/// misfire.
fn owner_lookup(port: u16) -> String {
    format!(
        "o=$(ss -lntp 2>/dev/null | grep ':{port} ' | grep -o 'pid=[0-9]*' | head -n1 | cut -d= -f2); [ -z \"$o\" ] && o=$PPID; "
    )
}

/// What the forwarder runs on the remote instead of `-N`.
///
/// It writes the lease and then stays alive; the session living is what keeps
/// the forward bound, exactly like `-N` did.
pub fn lease_command(fp: &str, port: u16) -> String {
    format!(
        "umask 077; mkdir -p {REMOTE_DIR}; {lookup}p={port}; printf '%s %s %s\\n' \"$p\" \"$o\" \"$(date +%s)\" > {file}; while :; do sleep 3600; done",
        lookup = owner_lookup(port),
        file = lease_file(fp),
    )
}

/// Runs over a short, throwaway ssh (no `-R`): read our lease, and kill the
/// holder only if it is provably the pid we leased.
///
/// Prints one of: `FREE`, `MISS`, `KILLED <pid>`, `FOREIGN <pid>`.
pub fn reclaim_command(fp: &str) -> String {
    format!(
        "f={file}; [ -f \"$f\" ] || {{ echo MISS; exit 0; }}; \
         read p o t < \"$f\"; \
         n=$(ss -lntp 2>/dev/null | grep \":$p \" | grep -o 'pid=[0-9]*' | head -n1 | cut -d= -f2); \
         if [ -z \"$n\" ]; then echo FREE; rm -f \"$f\"; exit 0; fi; \
         if [ \"$n\" = \"$o\" ] && grep -qa sshd /proc/$n/cmdline 2>/dev/null; then \
           kill -9 \"$n\" 2>/dev/null; echo \"KILLED $n\"; \
         else echo \"FOREIGN $n\"; fi; \
         rm -f \"$f\"",
        file = lease_file(fp),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reclaim {
    /// Nothing holds the port any more.
    Free,
    /// We leased it and killed our own leftover session.
    Killed(u32),
    /// Somebody else holds it. Never touched.
    Foreign(u32),
    /// No lease on the remote (first run, or /tmp was wiped).
    Missing,
    Unknown(String),
}

impl Reclaim {
    pub fn describe(&self) -> String {
        match self {
            Reclaim::Free => "远端端口空闲".into(),
            Reclaim::Killed(p) => format!("回收了上次连接遗留的会话 (pid {p})"),
            Reclaim::Foreign(p) => format!("远端端口被另一个进程占用 (pid {p})，不处理"),
            Reclaim::Missing => "远端没有本实例的租约".into(),
            Reclaim::Unknown(s) => format!("回收结果未识别: {s}"),
        }
    }

}

pub fn parse_reclaim(out: &str) -> Reclaim {
    let line = out.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("");
    let mut it = line.split_whitespace();
    match (it.next(), it.next()) {
        (Some("FREE"), _) => Reclaim::Free,
        (Some("MISS"), _) => Reclaim::Missing,
        (Some("KILLED"), Some(p)) => p.parse().map(Reclaim::Killed).unwrap_or(Reclaim::Unknown(line.into())),
        (Some("FOREIGN"), Some(p)) => p.parse().map(Reclaim::Foreign).unwrap_or(Reclaim::Unknown(line.into())),
        _ => Reclaim::Unknown(line.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reclaim_output() {
        assert_eq!(parse_reclaim("FREE\n"), Reclaim::Free);
        assert_eq!(parse_reclaim("MISS"), Reclaim::Missing);
        assert_eq!(parse_reclaim("KILLED 705"), Reclaim::Killed(705));
        assert_eq!(parse_reclaim("FOREIGN 42\n"), Reclaim::Foreign(42));
        assert!(matches!(parse_reclaim("garbage"), Reclaim::Unknown(_)));
        assert!(matches!(parse_reclaim(""), Reclaim::Unknown(_)));
    }

    #[test]
    fn fingerprint_is_path_safe() {
        let fp = fingerprint(&instance_id(Path::new("/etc/linkbox/linkbox.toml")), "workbuddy-api");
        assert!(fp.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn lease_command_mentions_port_and_file() {
        let c = lease_command("abc-workbuddy-api", 7864);
        assert!(c.contains("/tmp/linkbox/abc-workbuddy-api.lease"));
        assert!(c.contains("p=7864"));
        assert!(c.contains("while :"));
    }
}
