use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub panel: Panel,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub tunnels: Vec<Tunnel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for Panel {
    fn default() -> Self {
        Self { bind: default_bind(), token: None }
    }
}

fn default_bind() -> String {
    "127.0.0.1:8085".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default = "d_ssh_user")]
    pub ssh_user: String,
    #[serde(default = "d_ssh_port")]
    pub ssh_port: u16,
    #[serde(default = "d_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "d_alive_interval")]
    pub server_alive_interval: u64,
    #[serde(default = "d_alive_count")]
    pub server_alive_count_max: u32,
    #[serde(default = "d_health_interval")]
    pub health_interval_secs: u64,
    #[serde(default = "d_health_cmd")]
    pub health_cmd: String,
    #[serde(default = "d_health_expect")]
    pub health_expect: String,
    #[serde(default = "d_health_path")]
    pub health_path: String,
    #[serde(default = "d_strict")]
    pub strict_host_key_checking: String,
    #[serde(default = "d_backoff_min")]
    pub backoff_min_secs: u64,
    #[serde(default = "d_backoff_max")]
    pub backoff_max_secs: u64,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            ssh_user: d_ssh_user(),
            ssh_port: d_ssh_port(),
            connect_timeout_secs: d_connect_timeout(),
            server_alive_interval: d_alive_interval(),
            server_alive_count_max: d_alive_count(),
            health_interval_secs: d_health_interval(),
            health_cmd: d_health_cmd(),
            health_expect: d_health_expect(),
            health_path: d_health_path(),
            strict_host_key_checking: d_strict(),
            backoff_min_secs: d_backoff_min(),
            backoff_max_secs: d_backoff_max(),
        }
    }
}

fn d_ssh_user() -> String { "root".into() }
fn d_ssh_port() -> u16 { 22 }
fn d_connect_timeout() -> u64 { 10 }
fn d_alive_interval() -> u64 { 20 }
fn d_alive_count() -> u32 { 3 }
fn d_health_interval() -> u64 { 15 }
fn d_health_expect() -> String { "200".into() }
fn d_health_path() -> String { "/".into() }
fn d_strict() -> String { "accept-new".into() }
fn d_backoff_min() -> u64 { 1 }
fn d_backoff_max() -> u64 { 60 }

/// The end-to-end probe runs *on the remote host* against the forwarded port.
/// Placeholders: `{remote_port}`, `{local_target}`, `{health_path}`.
fn d_health_cmd() -> String {
    "curl -sS -o /dev/null -w \"%{http_code} %{time_total}\" --max-time 8 \
     http://127.0.0.1:{remote_port}{health_path}"
        .into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tunnel {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub ssh_port: Option<u16>,
    #[serde(default)]
    pub ssh_user: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub via_socks: Option<String>,
    #[serde(default)]
    pub remote_bind: Option<String>,
    pub remote_port: u16,
    #[serde(default)]
    pub local_target: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub health_cmd: Option<String>,
    #[serde(default)]
    pub health_path: Option<String>,
    #[serde(default)]
    pub health_expect: Option<String>,
}

fn yes() -> bool {
    true
}

/// A tunnel with every optional field resolved against `[defaults]`.
#[derive(Debug, Clone)]
pub struct ResolvedTunnel {
    pub name: String,
    pub user: String,
    pub host: String,
    pub port: u16,
    pub identity: Option<PathBuf>,
    pub via_socks: Option<String>,
    pub remote_bind: Option<String>,
    pub remote_port: u16,
    pub local_target: String,
    pub health_cmd: String,
    pub health_expect: String,
}

impl ResolvedTunnel {
    /// The `-R` argument value handed to ssh.
    pub fn forward_spec(&self) -> String {
        match &self.remote_bind {
            Some(rb) if rb.starts_with('/') => format!("{}:{}", rb, self.local_target),
            Some(rb) => format!("{}:{}:{}", rb, self.remote_port, self.local_target),
            None => format!("127.0.0.1:{}:{}", self.remote_port, self.local_target),
        }
    }

    pub fn health_command(&self) -> String {
        self.health_cmd
            .replace("{remote_port}", &self.remote_port.to_string())
            .replace("{local_target}", &self.local_target)
    }
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

impl Config {
    pub fn parse_text(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("parse config toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for t in &self.tunnels {
            if !valid_name(&t.name) {
                bail!(
                    "tunnel name {:?} is invalid: use lowercase letters, digits, '_' and '-' (max 32)",
                    t.name
                );
            }
            if !seen.insert(t.name.clone()) {
                bail!("duplicate tunnel name {:?}", t.name);
            }
            if t.host.trim().is_empty() {
                bail!("tunnel {:?} has an empty host", t.name);
            }
        }
        Ok(())
    }

    pub fn tunnel(&self, name: &str) -> Option<&Tunnel> {
        self.tunnels.iter().find(|t| t.name == name)
    }

    pub fn resolve(&self, t: &Tunnel) -> ResolvedTunnel {
        let d = &self.defaults;
        let local_target = t
            .local_target
            .clone()
            .unwrap_or_else(|| format!("127.0.0.1:{}", t.remote_port));
        let health_path = t.health_path.clone().unwrap_or_else(|| d.health_path.clone());
        ResolvedTunnel {
            name: t.name.clone(),
            user: t.ssh_user.clone().unwrap_or_else(|| d.ssh_user.clone()),
            host: t.host.clone(),
            port: t.ssh_port.unwrap_or(d.ssh_port),
            identity: t.identity.as_deref().map(expand_tilde),
            via_socks: t.via_socks.clone(),
            remote_bind: t.remote_bind.clone(),
            remote_port: t.remote_port,
            local_target,
            health_cmd: t.health_cmd.clone().unwrap_or_else(|| d.health_cmd.clone()),
            health_expect: t.health_expect.clone().unwrap_or_else(|| d.health_expect.clone()),
        }
        .with_health_path(&health_path)
    }
}

impl ResolvedTunnel {
    fn with_health_path(mut self, path: &str) -> Self {
        self.health_cmd = self.health_cmd.replace("{health_path}", path);
        self
    }
}

pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Flip `enabled = ...` for one tunnel without disturbing the rest of the file
/// (comments, ordering and formatting survive). If the key is absent it is
/// inserted right after the `name = ...` line of that tunnel.
pub fn set_enabled_in_text(text: &str, name: &str, on: bool) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 1);

    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == "[[tunnels]]" {
            // collect this block
            let start = i;
            let mut end = i + 1;
            while end < lines.len() && lines[end].trim() != "[[tunnels]]" {
                end += 1;
            }
            let block = &lines[start..end];
            let is_target = block.iter().any(|l| {
                l.trim_start()
                    .strip_prefix("name")
                    .map(|r| r.trim_start().starts_with('='))
                    .unwrap_or(false)
                    && l.split('=').nth(1).map(|v| v.trim().trim_matches('"') == name) == Some(true)
            });
            let mut new_block: Vec<String> = block.iter().map(|l| l.to_string()).collect();
            if is_target {
                let mut replaced = false;
                for l in new_block.iter_mut() {
                    let t = l.trim_start();
                    if t.starts_with("enabled") {
                        *l = format!("enabled = {}", on);
                        replaced = true;
                        break;
                    }
                }
                if !replaced {
                    // insert after the name line, else at the top of the block
                    let mut idx = 0usize;
                    for (k, l) in block.iter().enumerate() {
                        if l.trim_start().starts_with("name") {
                            idx = k + 1;
                            break;
                        }
                    }
                    new_block.insert(idx, format!("enabled = {}", on));
                }
            }
            out.extend(new_block);
            i = end;
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    Ok(out.join("\n"))
}

/// Atomic write: same directory, fsync-free rename (config is small).
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("cfg")
    ));
    std::fs::write(&tmp, contents).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
