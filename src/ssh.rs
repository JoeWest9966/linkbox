//! ssh command assembly.
//!
//! Two flavours are built here:
//!   * the long-lived forwarder (`ssh -N -R ...`) owned by the supervisor
//!   * the probe (`ssh ... remote-cmd`) which reuses the forwarder's
//!     ControlMaster socket so a health check costs a few ms instead of a
//!     fresh handshake through a SOCKS hop.

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

pub fn forwarder_plan(
    t: &ResolvedTunnel,
    d: &Defaults,
    state_dir: &Path,
    self_exe: &Path,
) -> Plan {
    let cm = cm_path(state_dir, &t.name);
    let mut args: Vec<String> = vec![
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "ControlMaster=yes".into(),
        "-o".into(),
        format!("ControlPath={}", cm.display()),
        // ControlPersist=yes makes the master daemonize itself and the
        // foreground `ssh -N` exits 0 — the port stays bound by an orphan we
        // no longer own, and every respawn then collides with it. The socket
        // must live and die with THIS process.
        "-o".into(),
        "ControlPersist=no".into(),
        // THE load-bearing flag: without it, ssh logs in fine when the remote
        // port is taken and then forwards nothing while the panel shows green.
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ServerAliveInterval=0".into(),
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
    args.push("-p".into());
    args.push(t.port.to_string());
    args.push("-R".into());
    args.push(t.forward_spec());
    args.push(format!("{}@{}", t.user, t.host));

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
