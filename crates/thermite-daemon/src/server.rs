//! Unix-socket IPC server: connection acceptance, peer authentication,
//! request dispatch, and event fan-out to subscribers.
//!
//! Threading model: one reader thread per connection parses requests and
//! computes responses; a single writer thread per connection serializes all
//! outbound frames (responses + subscribed events) in order.

use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::{self, Group, Uid, User};
use thermite_core::config::{Config, FanModeKind};
use thermite_core::curve::CurveDef;
use thermite_proto::{
    Alert, AlertKind, AlertLevel, ControlSnapshot, DaemonInfo, Event, Frame, PROTOCOL_VERSION,
    Request, Response, StatusSnapshot, read_frame, write_frame,
};
use tracing::{info, warn};

use crate::state::{DaemonPaths, PersistedState, SharedState, save_persisted};

/// Broadcast registry: subscribed connections' outbound senders, pruned when
/// their writer dies.
#[derive(Default)]
pub struct Subscribers {
    inner: Mutex<Vec<(u64, mpsc::Sender<Frame>)>>,
    next_id: AtomicU64,
}

impl Subscribers {
    /// Register a sender; returns its id for later removal.
    pub fn add(&self, tx: mpsc::Sender<Frame>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut v) = self.inner.lock() {
            v.push((id, tx));
        }
        id
    }

    /// Remove a registration (writer thread teardown).
    pub fn remove(&self, id: u64) {
        if let Ok(mut v) = self.inner.lock() {
            v.retain(|(i, _)| *i != id);
        }
    }

    /// Send a frame to every subscriber; unreachable ones are pruned by
    /// their writer threads via [`Subscribers::remove`].
    pub fn broadcast(&self, frame: &Frame) {
        if let Ok(v) = self.inner.lock() {
            for (_, tx) in v.iter() {
                let _ = tx.send(frame.clone());
            }
        }
    }

    /// Number of live subscribers.
    pub fn count(&self) -> usize {
        self.inner.lock().map(|v| v.len()).unwrap_or(0)
    }
}

/// Bind the daemon socket: stale file removed, parent dir created, mode
/// 0660, group `thermite` when it exists and we are root.
pub fn bind_socket(path: &Path, group: Option<&str>) -> std::io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => info!("removed stale socket {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o660);
    std::fs::set_permissions(path, perms)?;
    if unistd::geteuid().is_root()
        && let Some(group) = group
        && let Ok(Some(g)) = Group::from_name(group)
    {
        let uid = Some(unistd::getuid());
        if let Err(e) = unistd::chown(path, uid, Some(g.gid)) {
            warn!("chown socket to {group} failed: {e}");
        } else {
            info!("socket {} owned by group {group}", path.display());
        }
    }
    Ok(listener)
}

/// Peer credential check: root, our own uid, or members of `group` may
/// connect.
fn peer_allowed(stream: &UnixStream, group: Option<&str>) -> bool {
    let Ok(cred) = getsockopt(stream, PeerCredentials) else {
        return false;
    };
    let uid = Uid::from_raw(cred.uid());
    if uid.is_root() || uid == unistd::geteuid() {
        return true;
    }
    let Some(group) = group else {
        return false;
    };
    let Ok(Some(g)) = Group::from_name(group) else {
        return false;
    };
    if g.gid.as_raw() == cred.gid() {
        return true;
    }
    // Supplementary-group membership via the user's name.
    User::from_uid(uid)
        .ok()
        .flatten()
        .is_some_and(|user| g.mem.contains(&user.name))
}

/// Accept loop; runs on its own thread until the listener dies.
pub fn serve(
    listener: UnixListener,
    shared: Arc<Mutex<SharedState>>,
    subs: Arc<Subscribers>,
    paths: DaemonPaths,
    allow_group: Option<String>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = peer_allowed(&stream, allow_group.as_deref());
                if peer {
                    let shared = Arc::clone(&shared);
                    let subs = Arc::clone(&subs);
                    let paths = paths.clone();
                    std::thread::spawn(move || {
                        handle_connection(stream, shared, subs, paths);
                    });
                } else {
                    warn!("rejected IPC connection (credentials)");
                }
            }
            Err(e) => warn!("accept failed: {e}"),
        }
    }
}

fn handle_connection(
    stream: UnixStream,
    shared: Arc<Mutex<SharedState>>,
    subs: Arc<Subscribers>,
    paths: DaemonPaths,
) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            warn!("stream clone failed: {e}");
            return;
        }
    };
    let (tx, rx) = mpsc::channel::<Frame>();
    // Subscription id, set when the client subscribes, used by the writer
    // thread to deregister from `subs` when the connection dies.
    let sub_slot: Arc<Mutex<Option<u64>>> = Arc::default();
    let writer_subs = Arc::clone(&subs);
    let writer_slot = Arc::clone(&sub_slot);
    std::thread::spawn(move || {
        for frame in rx {
            if write_frame(&mut writer, &frame).is_err() {
                break;
            }
        }
        if let Ok(slot) = writer_slot.lock()
            && let Some(id) = *slot
        {
            writer_subs.remove(id);
        }
    });

    let mut reader = BufReader::new(stream);
    let mut hello_done = false;
    while let Ok(frame) = read_frame(&mut reader) {
        let Frame::Request { id, request } = frame else {
            continue; // clients must not send events/responses; ignore
        };
        if !hello_done && !matches!(request, Request::Hello { .. }) {
            let _ = tx.send(Frame::Response {
                id,
                result: Err("send Hello first".to_string()),
            });
            continue;
        }
        let result = if matches!(request, Request::Subscribe) {
            if let Ok(mut slot) = sub_slot.lock()
                && slot.is_none()
            {
                *slot = Some(subs.add(tx.clone()));
            }
            Ok(Response::Ok)
        } else {
            handle_request(&request, &shared, &subs, &paths)
        };
        if matches!(request, Request::Hello { .. }) && result.is_ok() {
            hello_done = true;
        }
        if tx
            .send(Frame::Response {
                id,
                result: result.map_err(|e: String| e),
            })
            .is_err()
        {
            break; // writer gone
        }
    }
}

fn handle_request(
    request: &Request,
    shared: &Arc<Mutex<SharedState>>,
    subs: &Arc<Subscribers>,
    paths: &DaemonPaths,
) -> Result<Response, String> {
    match request {
        Request::Hello {
            protocol_version, ..
        } => {
            if *protocol_version != PROTOCOL_VERSION {
                return Err(format!(
                    "protocol mismatch: server {PROTOCOL_VERSION}, client {protocol_version}"
                ));
            }
            let guard = shared.lock().map_err(|_| "state poisoned")?;
            Ok(Response::Hello {
                protocol_version: PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                capability: guard.capability.clone(),
            })
        }
        Request::Subscribe => unreachable!("handled inline in handle_connection"),
        Request::GetStatus => {
            let guard = shared.lock().map_err(|_| "state poisoned")?;
            Ok(Response::Status {
                status: snapshot(&guard),
            })
        }
        Request::GetConfig => {
            let guard = shared.lock().map_err(|_| "state poisoned")?;
            Ok(Response::Config {
                config: guard.config.clone(),
            })
        }
        Request::SetFanConfig { fan, config } => {
            let mut guard = shared.lock().map_err(|_| "state poisoned")?;
            let name = fan.to_string();
            validate_fan_config(config, &guard.config).map_err(|e| format!("fans.{name}: {e}"))?;
            if let Some(rt) = guard.fans.get_mut(&name) {
                rt.config = config.clone();
                rt.commanded_duty = None;
            } else {
                return Err(format!("unknown fan {name}"));
            }
            persist_state(&guard, &paths.state_dir).map_err(|e| e.to_string())?;
            Ok(Response::Ok)
        }
        Request::SetPlatformProfile { profile } => {
            let mut guard = shared.lock().map_err(|_| "state poisoned")?;
            if guard.config.platform.active.is_none() {
                return Err(
                    "platform profiles are managed by another tool (config has [platform] \
                     active = \"none\"); set an explicit profile in /etc/thermite/thermite.toml \
                     to let Thermite manage them"
                        .to_string(),
                );
            }
            if guard.capability.platform_profiles.is_empty() {
                return Err("platform_profile interface absent; profiles unavailable".to_string());
            }
            if !guard.capability.platform_profiles.contains(profile) {
                return Err(format!(
                    "profile \"{profile}\" not offered by firmware (available: {})",
                    guard.capability.platform_profiles.join(", ")
                ));
            }
            guard.desired_profile = Some(profile.clone());
            guard.profile_applied = false;
            guard.profile_error = None;
            persist_state(&guard, &paths.state_dir).map_err(|e| e.to_string())?;
            Ok(Response::Ok)
        }
        Request::PutCurve { name, points } => {
            let mut guard = shared.lock().map_err(|_| "state poisoned")?;
            let def =
                CurveDef::validated(points.clone()).map_err(|e| format!("curve {name}: {e}"))?;
            let mut new_config = guard.config.clone();
            new_config.curves.insert(name.clone(), def);
            new_config
                .validate()
                .map_err(|e| format!("config would become invalid: {e}"))?;
            validate_runtime_fans(&new_config, &guard.fans)?;
            write_config(&new_config, &paths.config).map_err(|e| e.to_string())?;
            guard.config = new_config;
            guard.config_gen += 1;
            let _ = persist_state(&guard, &paths.state_dir);
            subs.broadcast(&Frame::Event(Event::ConfigChanged {
                config: guard.config.clone(),
            }));
            Ok(Response::Ok)
        }
        Request::DeleteCurve { name } => {
            let mut guard = shared.lock().map_err(|_| "state poisoned")?;
            if !guard.config.curves.contains_key(name) {
                return Err(format!("curve {name} does not exist"));
            }
            let mut new_config = guard.config.clone();
            new_config.curves.remove(name);
            new_config
                .validate()
                .map_err(|e| format!("config would become invalid: {e}"))?;
            validate_runtime_fans(&new_config, &guard.fans)?;
            write_config(&new_config, &paths.config).map_err(|e| e.to_string())?;
            guard.config = new_config;
            guard.config_gen += 1;
            let _ = persist_state(&guard, &paths.state_dir);
            subs.broadcast(&Frame::Event(Event::ConfigChanged {
                config: guard.config.clone(),
            }));
            Ok(Response::Ok)
        }
        Request::Ping => Ok(Response::Pong),
    }
}

fn validate_fan_config(
    config: &thermite_core::config::FanConfig,
    cfg: &Config,
) -> Result<(), String> {
    match config.mode {
        FanModeKind::Auto => Ok(()),
        FanModeKind::Manual => config
            .manual_duty_pct
            .is_some_and(|d| d <= 100)
            .then_some(())
            .ok_or_else(|| "manual mode needs manual_duty_pct in 0-100".to_string()),
        FanModeKind::Curve => {
            let name = config.curve.clone().unwrap_or_default();
            cfg.curves
                .contains_key(&name)
                .then_some(())
                .ok_or_else(|| format!("curve mode references undefined curve \"{name}\""))
        }
    }
}

/// Runtime fan modes reference curves; check those (config.validate only
/// checks the config-file fan entries).
fn validate_runtime_fans(
    config: &Config,
    fans: &std::collections::BTreeMap<String, crate::state::FanRuntime>,
) -> Result<(), String> {
    for (fan, rt) in fans {
        if rt.config.mode == FanModeKind::Curve
            && !rt
                .config
                .curve
                .as_deref()
                .is_some_and(|name| config.curves.contains_key(name))
        {
            return Err(format!(
                "fan {fan} uses curve \"{}\" which would no longer exist",
                rt.config.curve.clone().unwrap_or_default()
            ));
        }
    }
    Ok(())
}

fn persist_state(guard: &SharedState, state_dir: &Path) -> anyhow::Result<()> {
    let mut fans = std::collections::BTreeMap::new();
    for (fan, rt) in &guard.fans {
        fans.insert(fan.clone(), rt.config.clone());
    }
    save_persisted(
        state_dir,
        &PersistedState {
            version: 1,
            fans,
            platform_profile: guard.desired_profile.clone(),
        },
    )
}

fn write_config(config: &Config, path: &Path) -> anyhow::Result<()> {
    let s = config.to_toml_string()?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, s)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn snapshot(guard: &SharedState) -> StatusSnapshot {
    StatusSnapshot {
        telemetry: guard.telemetry.clone(),
        control: guard.control_snapshot(),
        daemon: DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            uptime_ms: guard.started.elapsed().as_millis() as u64,
            dry_run: guard.dry_run,
            capability: guard.capability.clone(),
        },
        recent_alerts: guard.alerts.iter().rev().take(10).rev().cloned().collect(),
    }
}

/// Convenience: broadcast an alert from anywhere.
pub fn broadcast_alert(
    shared: &mut SharedState,
    subs: &Subscribers,
    level: AlertLevel,
    kind: AlertKind,
    message: String,
    tick_ms: u64,
) {
    let alert = shared.push_alert(Alert {
        level,
        kind,
        message,
        tick_ms,
    });
    subs.broadcast(&Frame::Event(Event::Alert(alert)));
}

/// Build a tick event frame (used by the control loop and tests).
pub fn tick_frame(telemetry: thermite_core::Telemetry, control: ControlSnapshot) -> Frame {
    Frame::Event(Event::Tick { telemetry, control })
}

#[cfg(test)]
mod tests {
    use super::*;
    use thermite_core::CapabilityReport;
    use thermite_core::curve::CurvePoint;
    use thermite_core::sensors::CapabilityLevel;
    use thermite_proto::Client;

    fn test_state() -> SharedState {
        let mut st = SharedState::new(
            thermite_core::Config::with_default_curves(),
            &PersistedState::default(),
            false,
        );
        st.config.platform.active = Some(thermite_core::PlatformProfile::Balanced);
        st.capability = CapabilityReport {
            level: CapabilityLevel::Full,
            platform_profiles: vec!["quiet".into(), "balanced".into()],
            ..CapabilityReport::default()
        };
        st
    }

    #[test]
    fn end_to_end_over_real_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("t.sock");
        let listener = bind_socket(&sock, None).unwrap();
        let shared = Arc::new(Mutex::new(test_state()));
        let subs = Arc::new(Subscribers::default());
        let paths = DaemonPaths {
            config: dir.path().join("thermite.toml"),
            socket: sock.clone(),
            state_dir: dir.path().join("state"),
        };
        // Seed a config file so PutCurve persistence has somewhere to go.
        std::fs::write(&paths.config, "").unwrap();

        {
            let shared = Arc::clone(&shared);
            let subs = Arc::clone(&subs);
            let paths = paths.clone();
            std::thread::spawn(move || serve(listener, shared, subs, paths, None));
        }

        let mut client = Client::connect(&sock, "test").unwrap();
        let Response::Status { status } = client.call(Request::GetStatus).unwrap() else {
            panic!("expected status");
        };
        assert_eq!(status.daemon.pid, std::process::id());
        assert_eq!(status.control.fans.len(), 2);

        // Set fan mode to a curve; state must persist.
        client
            .call(Request::SetFanConfig {
                fan: thermite_core::FanId::Cpu,
                config: thermite_core::config::FanConfig {
                    mode: FanModeKind::Curve,
                    curve: Some("balanced".into()),
                    manual_duty_pct: None,
                },
            })
            .unwrap();
        let persisted = crate::state::load_persisted(&paths.state_dir).unwrap();
        assert_eq!(
            persisted.fans.get("cpu").unwrap().curve.as_deref(),
            Some("balanced")
        );

        // Bad curve reference rejected.
        let err = client
            .call(Request::SetFanConfig {
                fan: thermite_core::FanId::Cpu,
                config: thermite_core::config::FanConfig {
                    mode: FanModeKind::Curve,
                    curve: Some("nope".into()),
                    manual_duty_pct: None,
                },
            })
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");

        // Profile validation against firmware choices.
        client
            .call(Request::SetPlatformProfile {
                profile: "quiet".into(),
            })
            .unwrap();
        assert!(
            client
                .call(Request::SetPlatformProfile {
                    profile: "turbo".into()
                })
                .is_err()
        );

        // Curve round-trip with persistence.
        client
            .call(Request::PutCurve {
                name: "custom".into(),
                points: vec![CurvePoint(40.0, 30), CurvePoint(80.0, 100)],
            })
            .unwrap();
        let Response::Config { config } = client.call(Request::GetConfig).unwrap() else {
            panic!("expected config");
        };
        assert!(config.curves.contains_key("custom"));
        assert!(paths.config.exists(), "config persisted to disk");

        // Subscription receives a broadcast event.
        client.subscribe().unwrap();
        subs.broadcast(&tick_frame(Default::default(), Default::default()));
        assert!(matches!(client.read_event().unwrap(), Event::Tick { .. }));
    }
}
