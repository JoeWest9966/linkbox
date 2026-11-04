//! Shared runtime state: config snapshot + per-tunnel status.

use crate::config::{Config, ResolvedTunnel};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Disabled,
    Dialing,
    Up,
    Degraded,
    Down,
    /// ssh is fine, but the remote port is held by something that is not us.
    /// Retrying is pointless until that changes, so we stop and say so.
    Conflict,
    /// Authentication failed: a human has to install a key. Retry loop parks.
    AuthFailed,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Disabled => "disabled",
            Phase::Dialing => "dialing",
            Phase::Up => "up",
            Phase::Degraded => "degraded",
            Phase::Down => "down",
            Phase::Conflict => "conflict",
            Phase::AuthFailed => "authfailed",
        }
    }

    /// Phases where probing the remote makes no sense: there is no forwarder
    /// to probe through, and reporting "no control socket" would bury the real
    /// reason the panel already knows.
    pub fn probe_worthwhile(&self) -> bool {
        matches!(self, Phase::Dialing | Phase::Up | Phase::Degraded)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub ok: bool,
    pub rtt_ms: Option<u64>,
    pub detail: String,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub name: String,
    pub phase: Phase,
    pub remote_spec: String,
    pub target: String,
    pub via_socks: Option<String>,
    pub pid: Option<u32>,
    pub up_since: Option<u64>,
    pub attempts: u64,
    pub restarts: u64,
    pub last_error: Option<String>,
    pub health: Option<Health>,
    pub log_tail: Vec<String>,
}

impl Status {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            phase: Phase::Down,
            remote_spec: String::new(),
            target: String::new(),
            via_socks: None,
            pid: None,
            up_since: None,
            attempts: 0,
            restarts: 0,
            last_error: None,
            health: None,
            log_tail: Vec::new(),
        }
    }
}

/// Per-tunnel control flags poked by the panel.
pub struct Control {
    pub restart: AtomicBool,
    /// Bumped whenever config for this tunnel changed, so the supervisor can
    /// notice (and rebuild the ssh args) without a full process restart.
    pub config_epoch: AtomicU64,
}

impl Control {
    pub fn new() -> Self {
        Self {
            restart: AtomicBool::new(false),
            config_epoch: AtomicU64::new(0),
        }
    }
}

/// The live forwarder PID, so shutdown/restart can kill it explicitly instead
/// of relying on `kill_on_drop` (which does not fire if the future is simply
/// leaked, e.g. on SIGTERM). Also the spawn guard: exactly one forwarder per
/// tunnel name at a time.
#[derive(Default)]
pub struct ChildPids(pub Mutex<HashMap<String, u32>>);

impl ChildPids {
    /// Atomically claim the right to run a forwarder for `name`.
    /// Returns the PID of an existing live child, or `None` if we won the claim.
    pub fn try_claim(&self, name: &str) -> Option<u32> {
        let mut m = self.0.lock().unwrap();
        if let Some(pid) = m.get(name).copied() {
            if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return Some(pid);
            }
            // stale entry (child died without clearing) — free the slot
            m.remove(name);
        }
        m.insert(name.to_string(), u32::MAX); // placeholder: "claimed, no pid yet"
        None
    }

    /// Publish the real PID after a successful spawn.
    pub fn set(&self, name: &str, pid: u32) {
        self.0.lock().unwrap().insert(name.to_string(), pid);
    }
    pub fn clear(&self, name: &str) {
        self.0.lock().unwrap().remove(name);
    }
    #[allow(dead_code)]
    pub fn pid_of(&self, name: &str) -> Option<u32> {
        self.0.lock().unwrap().get(name).copied().filter(|p| *p != u32::MAX)
    }
    pub fn all(&self) -> Vec<(String, u32)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, v)| **v != u32::MAX)
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

pub struct App {
    pub config_path: PathBuf,
    pub config_text: RwLock<String>,
    pub cfg: RwLock<Config>,
    pub status: RwLock<BTreeMap<String, Status>>,
    pub controls: Mutex<HashMap<String, Arc<Control>>>,
    pub children: ChildPids,
    pub state_dir: PathBuf,
    pub self_exe: PathBuf,
    pub started_at: u64,
    pub stopping: AtomicBool,
    /// Stable identity of this installation, used to build remote lease names.
    pub instance: String,
}

impl App {
    pub fn new(config_path: PathBuf, self_exe: PathBuf, state_dir: PathBuf) -> Self {
        let instance = crate::lease::instance_id(&config_path);
        Self {
            config_path,
            config_text: RwLock::new(String::new()),
            cfg: RwLock::new(Config::default()),
            status: RwLock::new(BTreeMap::new()),
            controls: Mutex::new(HashMap::new()),
            children: ChildPids::default(),
            state_dir,
            self_exe,
            started_at: now_epoch(),
            stopping: AtomicBool::new(false),
            instance,
        }
    }

    /// Remote lease name for a tunnel: distinguishes this installation from any
    /// other linkbox that happens to use the same ssh key and port.
    pub fn lease_fp(&self, name: &str) -> String {
        crate::lease::fingerprint(&self.instance, name)
    }

    pub fn control_for(&self, name: &str) -> Arc<Control> {
        let mut m = self.controls.lock().unwrap();
        m.entry(name.to_string())
            .or_insert_with(|| Arc::new(Control::new()))
            .clone()
    }

    pub fn tunnel_names(&self) -> Vec<String> {
        self.cfg
            .read()
            .unwrap()
            .tunnels
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    pub fn resolve_named(&self, name: &str) -> Option<ResolvedTunnel> {
        let cfg = self.cfg.read().unwrap();
        cfg.tunnel(name).map(|t| cfg.resolve(t))
    }

    pub fn snapshot(&self) -> Vec<Status> {
        self.status.read().unwrap().values().cloned().collect()
    }

    pub fn set(&self, name: &str, f: impl FnOnce(&mut Status)) {
        let mut st = self.status.write().unwrap();
        if let Some(s) = st.get_mut(name) {
            f(s);
        }
    }

    pub fn get(&self, name: &str) -> Option<Status> {
        self.status.read().unwrap().get(name).cloned()
    }

    /// Append a log line, keeping the tail bounded.
    pub fn log(&self, name: &str, line: &str) {
        let mut st = self.status.write().unwrap();
        if let Some(s) = st.get_mut(name) {
            s.log_tail.push(line.to_string());
            if s.log_tail.len() > 24 {
                let excess = s.log_tail.len() - 24;
                s.log_tail.drain(0..excess);
            }
        }
    }

    pub fn apply_config(&self, cfg: Config, text: String) {
        let names: std::collections::HashSet<String> =
            cfg.tunnels.iter().map(|t| t.name.clone()).collect();
        {
            let mut st = self.status.write().unwrap();
            st.retain(|k, _| names.contains(k));
            for n in &names {
                st.entry(n.clone()).or_insert_with(|| Status::new(n));
            }
        }
        {
            let mut m = self.controls.lock().unwrap();
            m.retain(|k, _| names.contains(k));
        }
        *self.cfg.write().unwrap() = cfg;
        *self.config_text.write().unwrap() = text;
    }
}

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
