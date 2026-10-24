//! logind suspend/resume monitoring.
//!
//! Subscribes to `PrepareForSleep` on the system bus and forwards events to
//! the control loop, which restores firmware-auto before sleep and holds off
//! writes for a grace period after resume (the EC needs ~12-15s to
//! reinitialize; eager writes cause the profile-bouncing bugs seen in
//! Div-Acer-Manager-Max #253).

use std::sync::mpsc::Sender;

use tracing::{info, warn};

/// A sleep-cycle event from logind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendEvent {
    /// About to suspend (`PrepareForSleep(true)`).
    Sleeping,
    /// Just resumed (`PrepareForSleep(false)`).
    Resumed,
}

/// Spawn the watcher thread. Fails softly: without a system bus (containers,
/// minimal test environments) the daemon still runs, just without suspend
/// awareness — logged once.
pub fn spawn_watcher(tx: Sender<SuspendEvent>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("suspend-watch".into())
        .spawn(move || {
            let conn = match zbus::blocking::Connection::system() {
                Ok(c) => c,
                Err(e) => {
                    warn!("suspend watcher disabled (no system bus): {e}");
                    return;
                }
            };
            let proxy = match zbus::blocking::Proxy::new(
                &conn,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("suspend watcher: login1 proxy failed: {e}");
                    return;
                }
            };
            let signals = match proxy.receive_signal("PrepareForSleep") {
                Ok(s) => s,
                Err(e) => {
                    warn!("suspend watcher: signal subscription failed: {e}");
                    return;
                }
            };
            info!("watching logind PrepareForSleep");
            for message in signals {
                let Ok(start) = message.body().deserialize::<bool>() else {
                    continue;
                };
                let event = if start {
                    SuspendEvent::Sleeping
                } else {
                    SuspendEvent::Resumed
                };
                if tx.send(event).is_err() {
                    return; // control loop is gone
                }
            }
        })
        .unwrap_or_else(|e| {
            warn!("suspend watcher thread spawn failed: {e}");
            std::thread::spawn(|| ())
        })
}
