//! End-to-end health probing.
//!
//! A tunnel is not healthy because an `ssh` process exists. It is healthy when
//! a request made *from the remote host* through the forwarded port answers.
//! The probe rides the forwarder's ControlMaster socket, so each check is a
//! cheap multiplexed channel instead of a second handshake through the SOCKS hop.

use crate::config::ResolvedTunnel;
use crate::ssh;
use crate::state::{App, Health, Phase};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

pub async fn run_probe(app: &Arc<App>, t: &ResolvedTunnel) -> Health {
    let cm = ssh::control_socket(&app.state_dir, &t.name);
    if !cm.exists() {
        return Health {
            ok: false,
            rtt_ms: None,
            detail: "no control socket (forwarder not up)".into(),
            at: crate::state::now_epoch(),
        };
    }
    let args = ssh::probe_args(t, &app.state_dir);
    let started = Instant::now();
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let elapsed = started.elapsed();

    match out {
        Err(_) => Health {
            ok: false,
            rtt_ms: None,
            detail: "probe timed out".into(),
            at: crate::state::now_epoch(),
        },
        Ok(Err(e)) => Health {
            ok: false,
            rtt_ms: None,
            detail: format!("probe spawn failed: {e}"),
            at: crate::state::now_epoch(),
        },
        Ok(Ok(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !o.status.success() {
                return Health {
                    ok: false,
                    rtt_ms: None,
                    detail: first_line(&stderr, &stdout),
                    at: crate::state::now_epoch(),
                };
            }
            // stdout is `"<code> <seconds>"` from curl -w, or arbitrary text
            // if the user supplied their own command.
            let stdout_trim = stdout.trim();
            let ok = stdout_trim.contains(&t.health_expect);
            let rtt_ms = stdout_trim
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<f64>().ok())
                .map(|s| (s * 1000.0).round() as u64)
                .or(Some(elapsed.as_millis() as u64));
            Health {
                ok,
                rtt_ms,
                detail: if stdout_trim.is_empty() {
                    "ok".into()
                } else {
                    stdout_trim.to_string()
                },
                at: crate::state::now_epoch(),
            }
        }
    }
}

fn first_line(a: &str, b: &str) -> String {
    let s = if a.trim().is_empty() { b } else { a };
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(no output)")
        .to_string()
}

pub fn spawn_health_loop(app: Arc<App>, name: String) {
    tokio::spawn(async move {
        loop {
            if app.stopping.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let Some(t) = app.resolve_named(&name) else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            let interval = app
                .cfg
                .read()
                .unwrap()
                .defaults
                .health_interval_secs
                .max(5);
            if t.health_cmd.trim().is_empty() {
                // No probe configured: fall back to plain liveness. Give ssh a
                // grace window to finish the handshake, then promote
                // dialing -> up; the tunnel task drops it back to `down` if the
                // process dies.
                tokio::time::sleep(Duration::from_secs(5)).await;
                app.set(&name, |s| {
                    if s.phase == Phase::Dialing && s.pid.is_some() {
                        s.phase = Phase::Up;
                        s.up_since = Some(crate::state::now_epoch());
                    }
                });
                tokio::time::sleep(Duration::from_secs(interval)).await;
                continue;
            }
            let h = run_probe(&app, &t).await;
            app.set(&name, |s| {
                if s.phase == Phase::Disabled {
                    return;
                }
                s.health = Some(h.clone());
                if h.ok {
                    if s.phase != Phase::Up {
                        s.up_since = Some(crate::state::now_epoch());
                    }
                    s.phase = Phase::Up;
                } else if s.phase == Phase::Up {
                    // it was working and now it is not: that is `degraded`.
                    // A tunnel still in `dialing` is just not there yet.
                    s.phase = Phase::Degraded;
                    s.up_since = None;
                }
            });
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}
