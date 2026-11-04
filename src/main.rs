mod config;
mod health;
mod lease;
mod panel;
mod proxy;
mod ssh;
mod state;
mod supervisor;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use state::App;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[derive(Parser)]
#[command(
    name = "linkbox",
    version,
    about = "Client-side reverse tunnel supervisor with a local control panel"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
    /// Path to the config file.
    #[arg(short, long, global = true, default_value = "linkbox.toml")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the supervisor and the control panel.
    Run {
        /// Override the panel bind address from the config.
        #[arg(long)]
        bind: Option<String>,
    },
    /// SOCKS5 stdio bridge (used as ssh's ProxyCommand). Not for humans.
    ProxyStdio {
        #[arg(long)]
        socks: String,
        #[arg(long)]
        target: String,
    },
    /// Parse the config and print the ssh command each tunnel would run.
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::ProxyStdio { socks, target } => proxy::bridge(&socks, &target),
        other => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            rt.block_on(async_main(cli.config, other))
        }
    }
}

async fn async_main(config_path: PathBuf, cmd: Cmd) -> Result<()> {
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(non_blocking)
        .init();

    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let cfg = config::Config::parse_text(&text)?;

    match cmd {
        Cmd::Check => {
            println!("config: {}", config_path.display());
            let defaults = &cfg.defaults;
            for t in &cfg.tunnels {
                let r = cfg.resolve(t);
                let lease_cmd = lease::lease_command(
                    &lease::fingerprint(&lease::instance_id(&config_path), &t.name),
                    r.remote_port,
                );
                let plan = ssh::forwarder_plan(
                    &r,
                    defaults,
                    &PathBuf::from("/tmp"),
                    &PathBuf::from("linkbox"),
                    lease_cmd,
                );
                println!(
                    "\n[{}] {} -> {} (enabled={})\n  ssh {}",
                    t.name,
                    r.forward_spec(),
                    r.local_target,
                    t.enabled,
                    plan.args.join(" ")
                );
            }
            println!("\n{} tunnel(s) configured.", cfg.tunnels.len());
            return Ok(());
        }
        Cmd::Run { bind } => {
            let self_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("linkbox"));
            let state_dir = state_dir();
            std::fs::create_dir_all(&state_dir).ok();

            let app = std::sync::Arc::new(App::new(
                config_path.clone(),
                self_exe,
                state_dir.clone(),
            ));
            {
                let mut t = app.config_text.write().unwrap();
                *t = text.clone();
            }
            app.apply_config(cfg, text);

            supervisor::start(app.clone());

            let cfg_bind = app.cfg.read().unwrap().panel.bind.clone();
            let cfg_token = app.cfg.read().unwrap().panel.token.clone();
            let bind = bind.unwrap_or(cfg_bind);
            let listener = tokio::net::TcpListener::bind(&bind)
                .await
                .with_context(|| format!("bind panel on {bind}"))?;
            tracing::info!(%bind, "linkbox panel listening");
            tracing::info!(dir = %state_dir.display(), "state dir");

            let app2 = app.clone();
            let server = axum::serve(listener, panel::router(app2, cfg_token));
            tokio::select! {
                r = server => { r.context("panel server")?; }
                _ = shutdown_signal() => {
                    tracing::info!("shutting down");
                    app.stopping.store(true, Ordering::Relaxed);
                    kill_all(&app);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
            Ok(())
        }
        Cmd::ProxyStdio { .. } => unreachable!(),
    }
}

fn state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LINKBOX_STATE_DIR") {
        return PathBuf::from(d);
    }
    // /run/linkbox when we are root and it exists or can be created, else /tmp
    let run = PathBuf::from("/run/linkbox");
    if std::fs::create_dir_all(&run).is_ok() {
        return run;
    }
    std::env::temp_dir().join("linkbox")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn kill_all(app: &App) {
    for (name, pid) in app.children.all() {
        tracing::info!(pid, %name, "killing forwarder");
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }
    // also reap anything already recorded in status but missed by the registry
    let st = app.status.read().unwrap();
    for s in st.values() {
        if let Some(pid) = s.pid {
            let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        }
    }
}
