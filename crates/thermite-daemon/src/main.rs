//! Daemon entry point: wiring only — all logic lives in the library.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use thermite_backend::ThermiteBackend;
use thermite_core::config::Config;
use thermite_core::sensors::Backend;
use thermite_daemon::control::ControlLoop;
use thermite_daemon::dryrun::DryRunBackend;
use thermite_daemon::server::{Subscribers, bind_socket, serve};
use thermite_daemon::state::{
    DaemonPaths, PersistedState, SharedState, load_persisted, save_persisted,
};
use thermite_daemon::suspend::{SuspendEvent, spawn_watcher};
use thermite_proto::DEFAULT_SOCKET_PATH;
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "thermite-daemon",
    version,
    about = "Thermite thermal management daemon"
)]
struct Args {
    /// Path to the configuration file
    #[arg(long, default_value = "/etc/thermite/thermite.toml")]
    config: PathBuf,
    /// Path to the IPC socket
    #[arg(long, default_value = DEFAULT_SOCKET_PATH)]
    socket: PathBuf,
    /// Directory for persistent runtime state
    #[arg(long, default_value = "/var/lib/thermite")]
    state_dir: PathBuf,
    /// Log intended writes without touching hardware
    #[arg(long)]
    dry_run: bool,
    /// Use an in-memory backend (development/testing)
    #[arg(long)]
    fake_backend: bool,
    /// Unix group allowed to talk to the IPC socket
    #[arg(long, default_value = "thermite")]
    ipc_group: String,
    /// Log filter (or set RUST_LOG)
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&args.log_level)),
        )
        .init();

    let paths = DaemonPaths {
        config: args.config.clone(),
        socket: args.socket.clone(),
        state_dir: args.state_dir.clone(),
    };

    // Config: a present-but-invalid file is fatal (fail closed); a missing
    // file falls back to safe defaults.
    let config = if paths.config.exists() {
        Config::load(&paths.config)
            .with_context(|| format!("loading {}", paths.config.display()))?
    } else {
        warn!(
            "config {} not found; using defaults (fans stay in firmware auto until configured)",
            paths.config.display()
        );
        Config::with_default_curves()
    };

    let persisted = match load_persisted(&paths.state_dir) {
        Ok(p) => p,
        Err(e) => {
            warn!("state load failed, starting fresh: {e}");
            PersistedState::default()
        }
    };

    let backend: Box<dyn Backend> = if args.fake_backend {
        info!("using fake backend");
        Box::new(thermite_backend::FakeBackend::new())
    } else {
        Box::new(ThermiteBackend::probe())
    };
    let capability = backend.capability();
    info!(
        "hardware capability: {:?} (driver: {:?}, profiles: {:?})",
        capability.level, capability.driver, capability.platform_profiles
    );
    for note in &capability.notes {
        info!("capability: {note}");
    }

    let backend: Box<dyn Backend> = if args.dry_run {
        Box::new(DryRunBackend::new(backend))
    } else {
        backend
    };

    let shared = Arc::new(Mutex::new(SharedState::new(
        config,
        &persisted,
        args.dry_run,
        backend.fans().to_vec(),
    )));
    shared
        .lock()
        .map_err(|_| anyhow::anyhow!("state lock"))?
        .capability = capability;

    let subs = Arc::new(Subscribers::default());
    let listener = bind_socket(&paths.socket, Some(&args.ipc_group))
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    {
        let shared = Arc::clone(&shared);
        let subs = Arc::clone(&subs);
        let paths = paths.clone();
        let group = args.ipc_group.clone();
        std::thread::Builder::new()
            .name("ipc-accept".into())
            .spawn(move || serve(listener, shared, subs, paths, Some(group)))
            .context("spawning IPC thread")?;
    }

    let (suspend_tx, suspend_rx) = mpsc::channel::<SuspendEvent>();
    spawn_watcher(suspend_tx);

    let stop = Arc::new(AtomicBool::new(false));
    let reload = Arc::new(AtomicBool::new(false));
    {
        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP]).context("signal setup")?;
        let stop = Arc::clone(&stop);
        let reload = Arc::clone(&reload);
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                for sig in signals.forever() {
                    match sig {
                        SIGHUP => reload.store(true, Ordering::Relaxed),
                        SIGINT | SIGTERM => {
                            info!("signal {sig}: shutting down");
                            if let Ok(mut g) = shared.lock() {
                                g.shutting_down = true;
                            }
                            stop.store(true, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            })
            .context("spawning signal thread")?;
    }

    let notify = std::env::var("NOTIFY_SOCKET").is_ok();
    if notify {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
    }

    let mut control = ControlLoop::new(
        backend,
        Arc::clone(&shared),
        Arc::clone(&subs),
        paths.clone(),
        notify,
    );
    info!(
        "thermite-daemon {} starting (dry_run={}, pid={})",
        env!("CARGO_PKG_VERSION"),
        args.dry_run,
        std::process::id()
    );
    control.run(&stop, &suspend_rx, &reload);

    // Graceful exit: fans back to firmware auto, state persisted, socket
    // removed, systemd told we're stopping.
    info!("shutting down: restoring firmware auto mode");
    control.restore_auto();
    if let Ok(guard) = shared.lock() {
        let mut fans = std::collections::BTreeMap::new();
        for (fan, rt) in &guard.fans {
            fans.insert(fan.clone(), rt.config.clone());
        }
        let state = PersistedState {
            version: 1,
            fans,
            platform_profile: guard.desired_profile.clone(),
        };
        if let Err(e) = save_persisted(&paths.state_dir, &state) {
            error!("persisting state on shutdown: {e}");
        }
    }
    let _ = std::fs::remove_file(&paths.socket);
    if notify {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
    }
    info!("bye");
    Ok(())
}
