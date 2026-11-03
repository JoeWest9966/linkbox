//! Per-tunnel supervisor: spawn the ssh forwarder, watch it die, put it back.
//!
//! Two things this gets right that most naive wrappers get wrong:
//!   * `ExitOnForwardFailure=yes` is set, so a stolen remote port is a hard
//!     failure instead of a silent, permanently "up" zombie.
//!   * reconnect backoff is jittered exponential, capped, and reset on success,
//!     so a flapping remote does not turn into a log-spamming thundering herd.

use crate::health;
use crate::ssh;
use crate::state::{App, Phase, now_epoch};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub fn start(app: Arc<App>) {
    tokio::spawn(async move {
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            if app.stopping.load(Ordering::Relaxed) {
                return;
            }
            let names = app.tunnel_names();
            for n in &names {
                if live.insert(n.clone()) {
                    spawn_tunnel(app.clone(), n.clone());
                    health::spawn_health_loop(app.clone(), n.clone());
                }
            }
            live.retain(|n| names.contains(n));
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}

fn spawn_tunnel(app: Arc<App>, name: String) {
    tokio::spawn(async move {
        let mut backoff = 1u64;
        let mut seen_epoch = 0u64;

        loop {
            if app.stopping.load(Ordering::Relaxed) {
                return;
            }
            let Some(t) = app.resolve_named(&name) else {
                // tunnel removed from config: exit so the registry can forget us
                return;
            };
            let defaults = app.cfg.read().unwrap().defaults.clone();
            let ctrl = app.control_for(&name);
            let epoch = ctrl.config_epoch.load(Ordering::Relaxed);
            if epoch != seen_epoch {
                seen_epoch = epoch;
                backoff = 1;
            }

            // honour an explicit restart request
            if ctrl.restart.swap(false, Ordering::Relaxed) {
                backoff = 1;
                app.set(&name, |s| {
                    s.restarts += 1;
                    s.last_error = Some("manual restart".into());
                });
            }

            if !tunnel_enabled(&app, &name) {
                app.set(&name, |s| {
                    s.phase = Phase::Disabled;
                    s.pid = None;
                    s.up_since = None;
                });
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            app.set(&name, |s| {
                s.phase = Phase::Dialing;
                s.attempts += 1;
            });

            let plan = ssh::forwarder_plan(&t, &defaults, &app.state_dir, &app.self_exe);
            app.set(&name, |s| {
                s.remote_spec = plan.remote_spec.clone();
                s.target = format!("{}@{}:{}", t.user, t.host, t.port);
                s.via_socks = t.via_socks.clone();
            });

            // never run two forwarders for one tunnel: atomically claim the
            // slot. If another task already holds it (and the child is alive),
            // back off; a dead child frees the slot automatically.
            if app.children.try_claim(&name).is_some() {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            // We own the slot, so any leftover control socket is stale by
            // definition (its owner is gone) and must not be reused.
            ssh::clear_control_socket(&app.state_dir, &name);

            let mut cmd = Command::new("ssh");
            cmd.args(&plan.args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            match cmd.spawn() {
                Err(e) => {
                    app.children.clear(&name);
                    app.set(&name, |s| {
                        s.phase = Phase::Down;
                        s.last_error = Some(format!("spawn ssh: {e}"));
                    });
                }
                Ok(child) => {
                    let pid = child.id();
                    if let Some(p) = pid {
                        app.children.set(&name, p);
                    }
                    tracing::info!(name = %name, pid = ?pid, spec = %plan.remote_spec, "forwarder started");
                    app.set(&name, |s| {
                        s.pid = pid;
                        s.up_since = Some(now_epoch());
                        s.last_error = None;
                        // ssh is up: the health loop only downgrades from here
                        s.phase = Phase::Up;
                    });
                    let (exit, err) = drive_child(app.clone(), name.clone(), child).await;
                    app.set(&name, |s| {
                        s.pid = None;
                        s.up_since = None;
                        s.phase = Phase::Down;
                        s.last_error = Some(err.clone());
                    });
                    // healthy long run → reset backoff for the next flap
                    if exit {
                        backoff = 1;
                    }
                }
            }

            let cap = defaults.backoff_max_secs.max(1);
            let wait = backoff.min(cap);
            backoff = (backoff * 2).min(cap);
            // ±25% jitter so N tunnels never reconnect in lockstep
            let jitter = 1.0 + (fastrand_jitter(&name) * 0.5) - 0.25;
            let wait = ((wait as f64) * jitter).max(1.0) as u64;
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    });
}

fn tunnel_enabled(app: &Arc<App>, name: &str) -> bool {
    app.cfg
        .read()
        .unwrap()
        .tunnel(name)
        .map(|t| t.enabled)
        .unwrap_or(false)
}

/// Kills and reaps its child on drop. `kill_on_drop` is not enough when the
/// owning future is cancelled mid-`wait()`: the ssh process escapes and is
/// reparented to init, then collides with the next forwarder for the same port.
struct ChildGuard {
    child: Option<Child>,
    name: String,
    app: Arc<App>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
            // best-effort reap; do not block on a hung ssh forever
            let _ = c.try_wait();
        }
        self.app.children.clear(&self.name);
        // NOTE: deliberately NOT clearing the control socket here. A live
        // forwarder owns that socket; unlinking it makes the still-running ssh
        // exit 0 and the next spawn races the corpse. The socket is cleaned at
        // the top of the supervisor loop instead, where we know no child is up.
    }
}

/// Returns (ran_long_enough_to_reset_backoff, last_error).
async fn drive_child(
    app: Arc<App>,
    name: String,
    child: Child,
) -> (bool, String) {
    let mut guard = ChildGuard {
        child: Some(child),
        name: name.clone(),
        app: app.clone(),
    };
    // drain stderr into the log tail
    if let Some(err) = guard.child.as_mut().and_then(|c| c.stderr.take()) {
        let app2 = app.clone();
        let name2 = name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                app2.log(&name2, &l);
            }
        });
    }

    let started = now_epoch();
    let mut marker = app.get(&name).map(|s| s.attempts).unwrap_or(0);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                let ctrl = app.control_for(&name);
                let wants_restart = ctrl.restart.load(Ordering::Relaxed);
                let removed = app.resolve_named(&name).is_none();
                let epoch = ctrl.config_epoch.load(Ordering::Relaxed);
                let mut fingerprint_changed = false;
                if let Some(t) = app.resolve_named(&name) {
                    let d = app.cfg.read().unwrap().defaults.clone();
                    let plan = ssh::forwarder_plan(&t, &d, &app.state_dir, &app.self_exe);
                    fingerprint_changed = plan.remote_spec
                        != app.get(&name).map(|s| s.remote_spec).unwrap_or_default();
                    let _ = epoch;
                }
                if wants_restart || removed || fingerprint_changed {
                    app.log(&name, "stopping forwarder (restart/config change)");
                    if let Some(c) = guard.child.as_mut() {
                        let _ = c.start_kill();
                        let _ = c.wait().await;
                    }
                    let msg = if removed { "removed from config" } else { "restarted" };
                    return (false, msg.to_string());
                }
                // if someone bumped attempts manually, ignore; marker is informational
                marker = marker.wrapping_add(0);
            }
            res = async {
                match guard.child.as_mut() {
                    Some(c) => Some(c.wait().await),
                    None => None,
                }
            } => {
                let long_run = now_epoch().saturating_sub(started) >= 30;
                let detail = match res {
                    None => "child already reaped".to_string(),
                    Some(Ok(status)) => format!("ssh exited: {status}"),
                    Some(Err(e)) => format!("ssh wait failed: {e}"),
                };
                return (long_run, detail);
            }
        }
    }
}

/// Deterministic per-name jitter in [0,1) — no rand dependency needed.
fn fastrand_jitter(name: &str) -> f64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    now_epoch().hash(&mut h);
    let v = h.finish() % 1000;
    v as f64 / 1000.0
}
