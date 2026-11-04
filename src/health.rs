//! End-to-end health probing.
//!
//! A tunnel is not healthy because an `ssh` process exists. It is healthy when
//! a request made *from the remote host* through the forwarded port answers.
//! The probe rides the forwarder's ControlMaster socket, so each check is a
//! cheap multiplexed channel instead of a second handshake through the SOCKS hop.
//!
//! That trick has a cost this module has to own: the probe channel shares ONE
//! TCP connection — and therefore one congestion window — with whatever the user
//! is pushing through the tunnel. On a 250 ms RTT cross-border link a big upload
//! fills the send queue and the probe's few KB sit behind it. Reporting that as
//! `degraded` on the first timeout is a lie: the tunnel is working, it is just
//! busy. Hence the strike counter.

use crate::config::ResolvedTunnel;
use crate::ssh;
use crate::state::{App, Health, Phase, ProbeKind};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Consecutive timeouts before we admit something is wrong. One slow probe on a
/// congested evening is normal; this many in a row is not.
const TIMEOUT_STRIKES: u32 = 3;
/// A refusal or a wrong answer is unambiguous, so it needs fewer strikes.
const HARD_STRIKES: u32 = 2;

fn strikes_needed(kind: ProbeKind) -> u32 {
    match kind {
        ProbeKind::Timeout => TIMEOUT_STRIKES,
        _ => HARD_STRIKES,
    }
}

fn failed(kind: ProbeKind, detail: impl Into<String>) -> Health {
    Health {
        ok: false,
        kind,
        fail_streak: 0,
        rtt_ms: None,
        detail: detail.into(),
        at: crate::state::now_epoch(),
    }
}

pub async fn run_probe(app: &Arc<App>, t: &ResolvedTunnel) -> Health {
    let cm = ssh::control_socket(&app.state_dir, &t.name);
    if !cm.exists() {
        return failed(ProbeKind::NoForwarder, "no control socket (forwarder not up)");
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
        // The channel never got its turn: queueing, almost always.
        Err(_) => failed(ProbeKind::Timeout, "probe timed out"),
        Ok(Err(e)) => failed(ProbeKind::Failed, format!("probe spawn failed: {e}")),
        Ok(Ok(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !o.status.success() {
                let detail = first_line(&stderr, &stdout);
                // curl's own `--max-time` firing is still a timeout, not a
                // refusal: the remote end was reachable enough to start.
                let kind = if detail.contains("timed out") {
                    ProbeKind::Timeout
                } else {
                    ProbeKind::Failed
                };
                return failed(kind, detail);
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
                kind: if ok { ProbeKind::Ok } else { ProbeKind::Failed },
                fail_streak: 0,
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
        let mut fail_streak: u32 = 0;
        loop {
            if app.stopping.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let Some(t) = app.resolve_named(&name) else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            // Probing only makes sense while a forwarder could be up. In
            // `conflict`/`authfailed`/`down` the panel already knows the reason
            // and "no control socket" would just bury it.
            if !app
                .get(&name)
                .map(|s| s.phase.probe_worthwhile())
                .unwrap_or(false)
            {
                fail_streak = 0;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
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

            let mut h = run_probe(&app, &t).await;
            if h.ok {
                fail_streak = 0;
            } else {
                fail_streak = fail_streak.saturating_add(1);
            }
            h.fail_streak = fail_streak;
            let strikes = strikes_needed(h.kind);
            if !h.ok {
                // Show the count so a flaky-but-working tunnel does not look
                // identical to a dead one.
                h.detail = format!(
                    "{} · strike {}/{}",
                    h.detail,
                    fail_streak.min(strikes),
                    strikes
                );
            }

            let before = app.get(&name).map(|s| s.phase).unwrap_or(Phase::Down);
            let ok = h.ok;
            let detail = h.detail.clone();
            let stored = h.clone();
            app.set(&name, move |s| {
                if s.phase == Phase::Disabled {
                    return;
                }
                s.health = Some(stored);
                if ok {
                    if s.phase != Phase::Up {
                        s.up_since = Some(crate::state::now_epoch());
                    }
                    s.phase = Phase::Up;
                } else if s.phase == Phase::Up && fail_streak >= strikes {
                    // It was working and the failures have now outnumbered the
                    // benefit of the doubt: that is `degraded`. Below the
                    // threshold we deliberately stay `up` — the probe lost a
                    // race with real traffic, not with reality.
                    s.phase = Phase::Degraded;
                    s.up_since = None;
                }
            });

            let after = app.get(&name).map(|s| s.phase).unwrap_or(before);
            match (before, after) {
                (Phase::Up, Phase::Degraded) => app.log(
                    &name,
                    &format!("探测连续 {fail_streak} 次失败，降级：{detail}"),
                ),
                (Phase::Degraded, Phase::Up) => app.log(&name, "探测恢复，回到 up"),
                _ => {}
            }

            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_need_more_strikes_than_refusals() {
        // The whole point: a congested link must not be called dead as fast as
        // an actually refused connection.
        assert!(strikes_needed(ProbeKind::Timeout) > strikes_needed(ProbeKind::Failed));
        assert_eq!(strikes_needed(ProbeKind::Timeout), TIMEOUT_STRIKES);
        assert!(TIMEOUT_STRIKES >= 3);
    }

    #[test]
    fn a_failed_probe_carries_its_kind_and_a_fresh_streak() {
        let h = failed(ProbeKind::Timeout, "probe timed out");
        assert!(!h.ok);
        assert_eq!(h.kind, ProbeKind::Timeout);
        assert_eq!(h.fail_streak, 0, "streak is owned by the loop, not the probe");
        assert_eq!(h.detail, "probe timed out");
    }
}
