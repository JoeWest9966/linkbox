//! Per-tunnel supervisor: spawn the ssh forwarder, watch it die, put it back.
//!
//! Two things this gets right that most naive wrappers get wrong:
//!   * `ExitOnForwardFailure=yes` is set, so a stolen remote port is a hard
//!     failure instead of a silent, permanently "up" zombie.
//!   * reconnect backoff is jittered exponential, capped, and reset on success,
//!     so a flapping remote does not turn into a log-spamming thundering herd.

use crate::health;
use crate::lease;
use crate::ssh;
use crate::state::{App, Phase, now_epoch};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Why the forwarder died, in the only three flavours that change what we do
/// next. ssh gives us all of these on stderr and exit status 255 alike, which is
/// exactly why the naive version retried a permanent problem 113 times.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    /// `ExitOnForwardFailure` tripped: the remote port is taken. Maybe by our
    /// own previous session — which we can reclaim.
    PortBusy,
    /// Bad key, bad user, server refuses us. Nothing self-heals; park the loop.
    Auth,
    /// Timeout, refused, DNS, proxy hiccup. Back off and try again.
    Network,
    Other(String),
}

fn classify(exit: &str, tail: &[String]) -> Failure {
    let hay = format!("{exit}\n{}", tail.join("\n"));
    let has = |s: &str| hay.contains(s);
    if has("remote port forwarding failed") || has("forwarding failed for listen port") {
        return Failure::PortBusy;
    }
    if has("Permission denied")
        || has("Too many authentication failures")
        || has("no mutual signature algorithm")
        || has("Host key verification failed")
    {
        return Failure::Auth;
    }
    if has("Connection timed out")
        || has("banner exchange")
        || has("Connection refused")
        || has("Could not resolve hostname")
        || has("Network is unreachable")
        || has("kex_exchange_identification")
        || has("Connection closed by remote host")
        || has("Broken pipe")
    {
        return Failure::Network;
    }
    Failure::Other(exit.to_string())
}

impl Failure {
    fn describe(&self) -> String {
        match self {
            Failure::PortBusy => "远端端口被占用".into(),
            Failure::Auth => "认证失败：需要人处理（装密钥/改用户）".into(),
            Failure::Network => "网络不通，将自动重试".into(),
            Failure::Other(s) => s.clone(),
        }
    }
}

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

            let lease_cmd = lease::lease_command(&app.lease_fp(&name), t.remote_port);
            let plan =
                ssh::forwarder_plan(&t, &defaults, &app.state_dir, &app.self_exe, lease_cmd);
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

            let mut wait = 0u64;
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
                        s.up_since = None;
                        s.last_error = None;
                        // A spawned process is NOT a working tunnel: ssh can be
                        // mid-handshake, or about to die on authentication. Only
                        // the end-to-end probe is allowed to declare `up`.
                        s.phase = Phase::Dialing;
                    });
                    let (long_run, err, class) =
                        drive_child(app.clone(), name.clone(), child).await;
                    if long_run {
                        backoff = 1;
                    }
                    app.set(&name, |s| {
                        s.pid = None;
                        s.up_since = None;
                        s.phase = Phase::Down;
                        s.last_error = Some(format!("{} — {err}", class.describe()));
                    });

                    match class {
                        Failure::PortBusy => {
                            app.log(&name, "远端端口被占，先核对租约再决定是否回收");
                            let rec = reclaim_port(&app, &t, &defaults).await;
                            app.log(&name, &format!("租约核对：{}", rec.describe()));
                            // Only an outcome we can actually act on earns a
                            // fast retry. "no lease / can't tell" means the port
                            // is held by something we did not create, and a 1s
                            // hot loop against it is just noise.
                            let fast = matches!(rec, lease::Reclaim::Free | lease::Reclaim::Killed(_));
                            if fast {
                                app.set(&name, |s| s.phase = Phase::Dialing);
                                wait = 1;
                                backoff = 1;
                            } else {
                                tracing::warn!(name = %name, phase = Phase::Conflict.as_str(), "parked: remote port unavailable");
                                app.set(&name, |s| {
                                    s.phase = Phase::Conflict;
                                    s.last_error = Some(format!(
                                        "远端端口不可用：{}",
                                        rec.describe()
                                    ));
                                });
                                wait = 30;
                            }
                        }
                        Failure::Auth => {
                            // No amount of retrying installs a key. Park until
                            // the config changes or a human hits restart.
                            tracing::warn!(name = %name, phase = Phase::AuthFailed.as_str(), "parked: authentication failed");
                            app.set(&name, |s| s.phase = Phase::AuthFailed);
                            let epoch_at = app
                                .control_for(&name)
                                .config_epoch
                                .load(Ordering::Relaxed);
                            loop {
                                if app.stopping.load(Ordering::Relaxed) {
                                    return;
                                }
                                if app.resolve_named(&name).is_none() {
                                    return;
                                }
                                let c = app.control_for(&name);
                                if c.config_epoch.load(Ordering::Relaxed) != epoch_at
                                    || c.restart.swap(false, Ordering::Relaxed)
                                {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                            app.set(&name, |s| s.phase = Phase::Dialing);
                            backoff = 1;
                            wait = 1;
                        }
                        Failure::Network | Failure::Other(_) => {}
                    }
                }
            }

            let cap = defaults.backoff_max_secs.max(1);
            let base = backoff.min(cap);
            backoff = (backoff * 2).min(cap);
            // ±25% jitter so N tunnels never reconnect in lockstep
            let jitter = 1.0 + (fastrand_jitter(&name) * 0.5) - 0.25;
            let wait = if wait > 0 {
                wait
            } else {
                ((base as f64) * jitter).max(1.0) as u64
            };
            backoff_wait(&app, &name, epoch, wait).await;
        }
    });
}

/// Sleep out a backoff, but wake within half a second if the config changed,
/// the tunnel was removed, or a human pressed restart. Without this a config
/// fix can sit idle for the full 60s cap before it is even tried.
async fn backoff_wait(app: &Arc<App>, name: &str, epoch: u64, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if app.stopping.load(Ordering::Relaxed) {
            return;
        }
        let c = app.control_for(name);
        if c.restart.load(Ordering::Relaxed)
            || c.config_epoch.load(Ordering::Relaxed) != epoch
            || app.resolve_named(name).is_none()
            || std::time::Instant::now() >= deadline
        {
            return;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        tokio::time::sleep(remaining.min(Duration::from_millis(500))).await;
    }
}

/// Ask the remote whether the port is still bound, and kill the holder only if
/// it is provably our own leftover session.
async fn reclaim_port(
    app: &Arc<App>,
    t: &crate::config::ResolvedTunnel,
    d: &crate::config::Defaults,
) -> lease::Reclaim {
    let cmd = lease::reclaim_command(&app.lease_fp(&t.name));
    let args = ssh::remote_command_args(t, d, &app.state_dir, &app.self_exe, cmd);
    let fut = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(25), fut).await {
        Ok(Ok(out)) => lease::parse_reclaim(&String::from_utf8_lossy(&out.stdout)),
        Ok(Err(e)) => lease::Reclaim::Unknown(format!("spawn ssh: {e}")),
        Err(_) => lease::Reclaim::Unknown("回收 ssh 超时".into()),
    }
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

/// Ask the child to die, politely first.
///
/// SIGKILL is what turns one restart into thirty minutes of dead ports: the
/// process vanishes without a FIN, the remote sshd keeps the forwarded port
/// bound until `ClientAliveInterval` expires, and every redial in that window
/// fails. SIGTERM makes ssh close its channels and shut the connection down
/// properly, which is what actually releases the port.
async fn terminate(child: &mut Child, grace: Duration) {
    if let Some(pid) = child.id() {
        // SAFETY: plain syscall on a pid we own; ESRCH is fine.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            tracing::warn!(pid = ?child.id(), "forwarder ignored SIGTERM, killing");
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            if let Some(pid) = c.id() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            // synchronous grace: a clean exit is what frees the remote port
            for _ in 0..40 {
                match c.try_wait() {
                    Ok(Some(_)) => break,
                    _ => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            let _ = c.start_kill();
            let _ = c.try_wait();
        }
        self.app.children.clear(&self.name);
        // NOTE: deliberately NOT clearing the control socket here. A live
        // forwarder owns that socket; unlinking it makes the still-running ssh
        // exit 0 and the next spawn races the corpse. The socket is cleaned at
        // the top of the supervisor loop instead, where we know no child is up.
    }
}

/// Returns (ran_long_enough_to_reset_backoff, last_error, failure class).
async fn drive_child(
    app: Arc<App>,
    name: String,
    child: Child,
) -> (bool, String, Failure) {
    let mut guard = ChildGuard {
        child: Some(child),
        name: name.clone(),
        app: app.clone(),
    };
    // drain stderr into the log tail and keep a copy for classification
    let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(err) = guard.child.as_mut().and_then(|c| c.stderr.take()) {
        let app2 = app.clone();
        let name2 = name.clone();
        let tail2 = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                app2.log(&name2, &l);
                let mut t = tail2.lock().unwrap();
                t.push(l);
                if t.len() > 40 {
                    t.remove(0);
                }
            }
        });
    }

    let started = now_epoch();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                let ctrl = app.control_for(&name);
                let wants_restart = ctrl.restart.load(Ordering::Relaxed);
                let removed = app.resolve_named(&name).is_none();
                let mut fingerprint_changed = false;
                if let Some(t) = app.resolve_named(&name) {
                    let d = app.cfg.read().unwrap().defaults.clone();
                    let lease_cmd = lease::lease_command(&app.lease_fp(&name), t.remote_port);
                    let plan = ssh::forwarder_plan(
                        &t,
                        &d,
                        &app.state_dir,
                        &app.self_exe,
                        lease_cmd,
                    );
                    fingerprint_changed = plan.remote_spec
                        != app.get(&name).map(|s| s.remote_spec).unwrap_or_default();
                }
                if wants_restart || removed || fingerprint_changed {
                    app.log(&name, "stopping forwarder (restart/config change)");
                    if let Some(mut c) = guard.child.take() {
                        // graceful: the remote must see the FIN so it releases
                        // the forwarded port before we redial it
                        terminate(&mut c, Duration::from_secs(3)).await;
                    }
                    app.children.clear(&name);
                    let msg = if removed { "removed from config" } else { "restarted" };
                    return (false, msg.to_string(), Failure::Other(msg.to_string()));
                }
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
                // let the stderr reader drain the pipe before we classify
                tokio::time::sleep(Duration::from_millis(200)).await;
                let lines = tail.lock().unwrap().clone();
                let class = classify(&detail, &lines);
                return (long_run, detail, class);
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
