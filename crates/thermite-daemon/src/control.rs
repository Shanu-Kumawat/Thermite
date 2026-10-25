//! The control loop: one tick of read → decide → write, with all failsafe
//! and re-assert behavior.
//!
//! Invariants enforced here:
//! - firmware-override resistance: mode+duty re-asserted at least every
//!   `reassert_interval_ms`, and immediately when readback drifts;
//! - thermal failsafe: warn → full duty, crit → hand control back to the
//!   firmware (`pwm_enable = auto`) with a recovery latch;
//! - suspend safety: fans to auto before sleep, no writes for a grace
//!   period after resume (the EC needs ~12-15s to reinitialize — see
//!   Div-Acer-Manager-Max issue #253);
//! - graceful shutdown: fans restored to auto.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thermite_core::config::{Config, FanModeKind};
use thermite_core::sensors::{Backend, FanId, HwFanMode};
use thermite_core::{FailsafeEngine, FailsafePhase, Hysteresis, PhaseDirective};
use thermite_proto::{AlertKind, AlertLevel};
use tracing::{debug, info, warn};

use crate::server::{Subscribers, broadcast_alert, tick_frame};
use crate::state::{DaemonPaths, SharedState};
use crate::suspend::SuspendEvent;

/// How long after resume to wait before touching hardware again.
pub const RESUME_GRACE: Duration = Duration::from_secs(20);
/// Minimum time between external-override alerts per fan.
const OVERRIDE_ALERT_COOLDOWN: Duration = Duration::from_secs(60);

/// Values shared by per-fan decisions within one tick.
#[derive(Debug, Clone, Copy)]
struct TickContext<'a> {
    telemetry: &'a thermite_core::Telemetry,
    now: Instant,
    reassert_ms: u64,
}

/// What we last commanded for one fan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LastWrite {
    mode: HwFanMode,
    duty: Option<u8>,
    at: Instant,
}

/// The daemon's control loop. `tick_once` is public for tests; `run` drives
/// it on a timer with suspend handling.
pub struct ControlLoop {
    backend: Box<dyn Backend>,
    shared: Arc<Mutex<SharedState>>,
    subs: Arc<Subscribers>,
    paths: DaemonPaths,
    /// Ping the systemd watchdog each tick (when NOTIFY_SOCKET is set).
    notify: bool,
    failsafe: FailsafeEngine,
    hysteresis: BTreeMap<String, Hysteresis>,
    last_written: BTreeMap<String, LastWrite>,
    last_drift_alert: BTreeMap<String, Option<Instant>>,
    last_telemetry_error: Option<Instant>,
    tick_ms: u64,
    restored_auto: bool,
}

impl ControlLoop {
    /// Build from current shared state (reads config for engine tuning).
    pub fn new(
        backend: Box<dyn Backend>,
        shared: Arc<Mutex<SharedState>>,
        subs: Arc<Subscribers>,
        paths: DaemonPaths,
        notify: bool,
    ) -> Self {
        let (thresholds, drop_c, hold_ticks) = {
            let Ok(guard) = shared.lock() else {
                unreachable!("shared state lock at startup");
            };
            (
                guard.config.failsafe,
                guard.config.hysteresis.drop_c,
                guard.config.hysteresis.hold_ticks,
            )
        };
        let hysteresis = ["cpu", "gpu"]
            .into_iter()
            .map(|fan| (fan.to_string(), Hysteresis::new(drop_c, hold_ticks)))
            .collect();
        Self {
            backend,
            shared,
            subs,
            paths,
            notify,
            failsafe: FailsafeEngine::new(thresholds),
            hysteresis,
            last_written: BTreeMap::new(),
            last_drift_alert: BTreeMap::new(),
            last_telemetry_error: None,
            tick_ms: 0,
            restored_auto: false,
        }
    }

    /// Re-read engine tuning after a config reload.
    pub fn on_config_reload(&mut self, config: &Config) {
        self.failsafe.set_thresholds(config.failsafe);
        for h in self.hysteresis.values_mut() {
            h.drop_c = config.hysteresis.drop_c;
            h.hold_ticks = config.hysteresis.hold_ticks;
        }
    }

    /// Keep running until `stop`; sleeps in slices so suspend events,
    /// config reloads and shutdown are handled promptly.
    pub fn run(
        &mut self,
        stop: &AtomicBool,
        suspend_rx: &Receiver<SuspendEvent>,
        reload: &AtomicBool,
    ) {
        let watchdog = self.notify && std::env::var("NOTIFY_SOCKET").is_ok();
        loop {
            let now = Instant::now();
            while let Ok(event) = suspend_rx.try_recv() {
                self.handle_suspend(event, now);
            }
            if reload.swap(false, Ordering::Relaxed) {
                self.reload_config();
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            self.tick_once(now);
            if watchdog {
                let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let poll = self
                .shared
                .lock()
                .map(|g| Duration::from_millis(g.config.general.poll_interval_ms))
                .unwrap_or(Duration::from_secs(1));
            let deadline = now + poll;
            while Instant::now() < deadline {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                while let Ok(event) = suspend_rx.try_recv() {
                    self.handle_suspend(event, Instant::now());
                }
                if reload.swap(false, Ordering::Relaxed) {
                    self.reload_config();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// Reload the configuration file; on validation failure the previous
    /// configuration stays active and an alert is raised.
    pub fn reload_config(&mut self) {
        match Config::load(&self.paths.config) {
            Ok(config) => {
                self.on_config_reload(&config);
                if let Ok(mut guard) = self.shared.lock() {
                    guard.config = config.clone();
                }
                info!(
                    "configuration reloaded from {}",
                    self.paths.config.display()
                );
                self.subs.broadcast(&thermite_proto::Frame::Event(
                    thermite_proto::Event::ConfigChanged { config },
                ));
                self.alert(
                    AlertLevel::Warning,
                    AlertKind::ConfigReloaded,
                    "configuration reloaded".to_string(),
                );
            }
            Err(e) => {
                warn!("config reload rejected, keeping previous config: {e}");
                self.alert(
                    AlertLevel::Critical,
                    AlertKind::ConfigRejected,
                    format!("config reload rejected, previous config kept: {e}"),
                );
            }
        }
    }

    /// One control tick at time `now` (injected for testability).
    pub fn tick_once(&mut self, now: Instant) {
        let poll_ms = self
            .shared
            .lock()
            .map(|g| g.config.general.poll_interval_ms)
            .unwrap_or(1000);
        let reassert_ms = self
            .shared
            .lock()
            .map(|g| g.config.general.reassert_interval_ms)
            .unwrap_or(2000);
        self.tick_ms = self.tick_ms.saturating_add(poll_ms);

        // 1. Telemetry.
        let telemetry = match self.backend.read_telemetry() {
            Ok(t) => {
                self.last_telemetry_error = None;
                t
            }
            Err(e) => {
                warn!("telemetry read failed: {e}");
                let throttled = self
                    .last_telemetry_error
                    .is_none_or(|t| now.duration_since(t) > Duration::from_secs(60));
                if throttled {
                    self.last_telemetry_error = Some(now);
                    self.alert(
                        AlertLevel::Warning,
                        AlertKind::ExternalOverride,
                        format!("telemetry unavailable: {e}"),
                    );
                }
                return;
            }
        };
        let cpu_t = telemetry.cpu_temp_c();
        let gpu_t = telemetry.gpu_temp_c();
        let on_battery = telemetry.on_ac == Some(false);

        // 2. Shutdown: restore auto once and stop writing.
        if self.shared.lock().is_ok_and(|g| g.shutting_down) {
            if !self.restored_auto {
                self.restore_auto();
            }
            self.publish(telemetry);
            return;
        }

        // 3. Post-resume grace: observe, never write.
        {
            let mut guard = match self.shared.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if let Some(until) = guard.grace_until {
                if now < until {
                    guard.telemetry = telemetry.clone();
                    drop(guard);
                    self.publish(telemetry);
                    return;
                }
                // Grace expired: re-apply everything from a clean slate.
                guard.grace_until = None;
                guard.profile_applied = false;
                drop(guard);
                for h in self.hysteresis.values_mut() {
                    h.reset();
                }
                self.last_written.clear();
                self.alert(
                    AlertLevel::Warning,
                    AlertKind::ResumeReapply,
                    "resume grace period over; re-applying fan state".to_string(),
                );
            }
        }

        // 4. Failsafe state machine.
        let before = self.failsafe.phase();
        let directive = self
            .failsafe
            .step(cpu_t.unwrap_or(0.0), gpu_t.unwrap_or(0.0));
        let after = self.failsafe.phase();
        if after != before {
            let (level, kind, msg) = match after {
                FailsafePhase::ForceFull => (
                    AlertLevel::Warning,
                    AlertKind::FailsafeFull,
                    format!(
                        "warn threshold crossed (cpu {}°C, gpu {}°C): forcing full duty",
                        cpu_t.unwrap_or(0.0),
                        gpu_t.unwrap_or(0.0)
                    ),
                ),
                FailsafePhase::ReleasedAuto => (
                    AlertLevel::Critical,
                    AlertKind::FailsafeReleased,
                    format!(
                        "crit threshold crossed (cpu {}°C, gpu {}°C): control returned to firmware",
                        cpu_t.unwrap_or(0.0),
                        gpu_t.unwrap_or(0.0)
                    ),
                ),
                FailsafePhase::Normal => (
                    AlertLevel::Warning,
                    AlertKind::FailsafeRecovered,
                    "temperatures recovered; resuming manual control".to_string(),
                ),
            };
            self.alert(level, kind, msg);
        }

        // 5. Platform profile application (startup, change, post-resume).
        let desired_profile = self
            .shared
            .lock()
            .ok()
            .and_then(|g| g.desired_profile.clone());
        if let Some(profile) = desired_profile {
            let applied = self.shared.lock().is_ok_and(|g| g.profile_applied);
            if !applied {
                match self.backend.set_platform_profile(&profile) {
                    Ok(()) => {
                        info!("platform profile applied: {profile}");
                        if let Ok(mut g) = self.shared.lock() {
                            g.profile_applied = true;
                            g.profile_error = None;
                        }
                    }
                    Err(e) => {
                        warn!("platform profile {profile} rejected: {e}");
                        if let Ok(mut g) = self.shared.lock() {
                            g.profile_error = Some(e.to_string());
                        }
                        self.alert(
                            AlertLevel::Warning,
                            AlertKind::ProfileRejected,
                            format!("platform profile \"{profile}\" rejected: {e}"),
                        );
                    }
                }
            }
        }

        // 6. Fan decisions.
        let fan_configs: Vec<(String, thermite_core::config::FanConfig)> = self
            .shared
            .lock()
            .map(|g| {
                g.fans
                    .iter()
                    .map(|(name, rt)| (name.clone(), rt.config.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let ctx = TickContext {
            telemetry: &telemetry,
            now,
            reassert_ms,
        };
        for fan_id in FanId::all() {
            let name = fan_id.to_string();
            let Some(config) = fan_configs
                .iter()
                .find(|(n, _)| n == &name)
                .map(|(_, c)| c.clone())
            else {
                continue;
            };
            let driving_temp = match fan_id {
                FanId::Cpu => cpu_t.unwrap_or(0.0),
                FanId::Gpu => gpu_t.unwrap_or(0.0),
            };

            match directive {
                PhaseDirective::ReleaseToAuto => {
                    self.ensure_auto(&name, fan_id, ctx);
                }
                PhaseDirective::ForceFull => {
                    self.apply_fan(&name, fan_id, HwFanMode::Custom, 100, ctx);
                }
                PhaseDirective::Normal => match config.mode {
                    FanModeKind::Auto => {
                        self.ensure_auto(&name, fan_id, ctx);
                    }
                    FanModeKind::Manual => {
                        let duty = config.manual_duty_pct.unwrap_or(40);
                        let duty = self.failsafe.apply_floors(duty, driving_temp);
                        self.apply_fan(&name, fan_id, HwFanMode::Custom, duty, ctx);
                    }
                    FanModeKind::Curve => {
                        let curve_name = config.curve.clone().unwrap_or_default();
                        let curve = {
                            let Ok(guard) = self.shared.lock() else {
                                continue;
                            };
                            guard
                                .config
                                .effective_curve(&curve_name, on_battery)
                                .cloned()
                        };
                        match curve {
                            Some(curve) => {
                                let raw = self
                                    .hysteresis
                                    .get_mut(&name)
                                    .map(|h| h.update(driving_temp, &curve))
                                    .unwrap_or(0);
                                let duty = self.failsafe.apply_floors(raw, driving_temp);
                                self.apply_fan(&name, fan_id, HwFanMode::Custom, duty, ctx);
                            }
                            None => {
                                self.set_fan_error(
                                    &name,
                                    format!("curve \"{curve_name}\" missing"),
                                );
                                self.ensure_auto(&name, fan_id, ctx);
                            }
                        }
                    }
                },
            }
        }

        self.publish(telemetry);
    }

    /// Write mode + duty when needed (change, re-assert due, or readback
    /// drift) and track drift for external-override alerts.
    fn apply_fan(
        &mut self,
        name: &str,
        fan: FanId,
        mode: HwFanMode,
        duty: u8,
        ctx: TickContext<'_>,
    ) {
        let TickContext {
            telemetry,
            now,
            reassert_ms,
        } = ctx;
        let last = self.last_written.get(name).copied();
        let reassert_due =
            last.is_none_or(|l| now.duration_since(l.at) >= Duration::from_millis(reassert_ms));
        let hw = telemetry.fan(fan);
        let drifting = hw.is_some_and(|h| {
            h.hw_mode != Some(mode) || h.duty_pct.is_some_and(|d| d.abs_diff(duty) > 2)
        });

        // External-override detection: the readback disagrees with what we
        // last wrote (firmware reverting settings, or another tool). Our own
        // corrective write below resets the readback, so this must be
        // edge-triggered with a cooldown, not counted across consecutive
        // ticks.
        if let (Some(last), Some(h)) = (last, hw) {
            let mode_mismatch = h.hw_mode.is_some_and(|m| Some(m) != Some(last.mode));
            let duty_mismatch = last
                .duty
                .is_some_and(|d| h.duty_pct.is_some_and(|r| r.abs_diff(d) > 2));
            if (mode_mismatch || duty_mismatch)
                && self
                    .last_drift_alert
                    .get(name)
                    .copied()
                    .flatten()
                    .is_none_or(|t| now.duration_since(t) >= OVERRIDE_ALERT_COOLDOWN)
            {
                self.last_drift_alert.insert(name.to_string(), Some(now));
                self.alert(
                    AlertLevel::Warning,
                    AlertKind::ExternalOverride,
                    format!("fan {name} is not following commands (firmware or another tool interfering?)"),
                );
            }
        }

        let mode_changed = last.is_none_or(|l| l.mode != mode);
        let duty_changed = last.is_none_or(|l| l.duty != Some(duty));
        if !(mode_changed || duty_changed || reassert_due || drifting) {
            return;
        }

        let changed = mode_changed || duty_changed;
        debug!(
            "fan {name}: mode={mode:?} duty={duty}% (changed={changed} reassert={reassert_due} drift={drifting})"
        );
        let mut error = None;
        if (mode_changed || reassert_due || hw.and_then(|h| h.hw_mode) != Some(mode))
            && let Err(e) = self.backend.set_fan_hw_mode(fan, mode)
        {
            error = Some(format!("set mode: {e}"));
        }
        if error.is_none()
            && (duty_changed
                || reassert_due
                || hw
                    .and_then(|h| h.duty_pct)
                    .is_none_or(|d| d.abs_diff(duty) > 2))
            && let Err(e) = self.backend.set_fan_duty(fan, duty)
        {
            error = Some(format!("set duty: {e}"));
        }
        match error {
            Some(e) => self.set_fan_error(name, e),
            None => {
                self.set_fan_error(name, String::new());
                if let Ok(mut g) = self.shared.lock()
                    && let Some(rt) = g.fans.get_mut(name)
                {
                    rt.commanded_duty = Some(duty);
                }
            }
        }
        self.last_written.insert(
            name.to_string(),
            LastWrite {
                mode,
                duty: Some(duty),
                at: now,
            },
        );
    }

    /// Ensure a fan is in firmware-auto mode.
    fn ensure_auto(&mut self, name: &str, fan: FanId, ctx: TickContext<'_>) {
        let TickContext {
            telemetry,
            now,
            reassert_ms,
        } = ctx;
        let last = self.last_written.get(name).copied();
        let hw_mode = telemetry.fan(fan).and_then(|f| f.hw_mode);
        let needs_write = match last {
            None => hw_mode != Some(HwFanMode::Auto),
            Some(l) => {
                l.mode != HwFanMode::Auto
                    || hw_mode != Some(HwFanMode::Auto)
                    || now.duration_since(l.at) >= Duration::from_millis(reassert_ms)
            }
        };
        if needs_write && let Err(e) = self.backend.set_fan_hw_mode(fan, HwFanMode::Auto) {
            self.set_fan_error(name, format!("set auto mode: {e}"));
            return;
        }
        if needs_write {
            self.set_fan_error(name, String::new());
            self.last_written.insert(
                name.to_string(),
                LastWrite {
                    mode: HwFanMode::Auto,
                    duty: None,
                    at: now,
                },
            );
        }
        if let Ok(mut g) = self.shared.lock()
            && let Some(rt) = g.fans.get_mut(name)
        {
            rt.commanded_duty = None;
        }
    }

    /// Handle logind PrepareForSleep.
    pub fn handle_suspend(&mut self, event: SuspendEvent, now: Instant) {
        match event {
            SuspendEvent::Sleeping => {
                info!("system going to sleep: restoring firmware auto mode");
                self.restore_auto();
                if let Ok(mut g) = self.shared.lock() {
                    g.grace_until = Some(now + Duration::from_secs(3600));
                }
            }
            SuspendEvent::Resumed => {
                info!("system resumed: holding writes for {RESUME_GRACE:?}");
                if let Ok(mut g) = self.shared.lock() {
                    g.grace_until = Some(now + RESUME_GRACE);
                    g.profile_applied = false;
                }
            }
        }
    }

    /// Restore firmware-auto on every fan we may have touched.
    pub fn restore_auto(&mut self) {
        for fan in FanId::all() {
            if let Err(e) = self.backend.set_fan_hw_mode(fan, HwFanMode::Auto) {
                warn!("could not restore auto for {fan} fan: {e}");
            } else {
                info!("restored firmware auto for {fan} fan");
            }
        }
        self.restored_auto = true;
        self.last_written.clear();
    }

    /// Record (or clear) a per-fan write error in shared state.
    fn set_fan_error(&self, name: &str, error: String) {
        if let Ok(mut g) = self.shared.lock()
            && let Some(rt) = g.fans.get_mut(name)
        {
            if error.is_empty() {
                rt.last_write_error = None;
            } else {
                warn!("fan {name} write error: {error}");
                rt.last_write_error = Some(error);
            }
        }
    }

    /// Update shared telemetry/resolved curves and broadcast the tick.
    fn publish(&mut self, telemetry: thermite_core::Telemetry) {
        let Ok(mut guard) = self.shared.lock() else {
            return;
        };
        guard.telemetry = telemetry.clone();
        let on_battery = telemetry.on_ac == Some(false);
        let suffix = guard.config.general.battery_curve_suffix.clone();
        let curve_names: Vec<String> = guard.config.curves.keys().cloned().collect();
        for rt in guard.fans.values_mut() {
            rt.resolved_curve = match &rt.config.mode {
                FanModeKind::Curve => {
                    let base = rt.config.curve.clone().unwrap_or_default();
                    let battery_name = suffix.as_ref().map(|s| format!("{base}{s}"));
                    if on_battery
                        && battery_name
                            .as_ref()
                            .is_some_and(|n| curve_names.contains(n))
                    {
                        battery_name
                    } else if curve_names.contains(&base) {
                        Some(base)
                    } else {
                        None
                    }
                }
                _ => None,
            };
        }
        let control = guard.control_snapshot();
        let frame = tick_frame(telemetry, control);
        drop(guard);
        self.subs.broadcast(&frame);
    }

    fn alert(&mut self, level: AlertLevel, kind: AlertKind, message: String) {
        let tick = self.tick_ms;
        if let Ok(mut guard) = self.shared.lock() {
            broadcast_alert(&mut guard, &self.subs, level, kind, message, tick);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use thermite_backend::{FakeBackend, WriteOp};
    use thermite_core::config::{FanConfig, FanModeKind};
    use thermite_core::curve::{CurveDef, CurvePoint};

    /// FakeBackend behind Arc<Mutex<>> with settable temperatures, so tests
    /// can drive the loop tick by tick while it lives inside `Box<dyn
    /// Backend>`.
    struct ScriptedBackend {
        fake: Arc<Mutex<FakeBackend>>,
        cpu: Arc<Mutex<f32>>,
        gpu: Arc<Mutex<f32>>,
    }

    impl ScriptedBackend {
        fn new() -> (Self, FakeHandle, TempHandle, TempHandle) {
            let fake = Arc::new(Mutex::new(FakeBackend::new()));
            let cpu = Arc::new(Mutex::new(55.0));
            let gpu = Arc::new(Mutex::new(45.0));
            (
                Self {
                    fake: Arc::clone(&fake),
                    cpu: Arc::clone(&cpu),
                    gpu: Arc::clone(&gpu),
                },
                fake,
                cpu,
                gpu,
            )
        }
    }

    impl Backend for ScriptedBackend {
        fn capability(&self) -> thermite_core::CapabilityReport {
            self.fake.lock().map(|f| f.capability()).unwrap_or_default()
        }
        fn read_telemetry(
            &mut self,
        ) -> Result<thermite_core::Telemetry, thermite_core::BackendError> {
            let cpu = self.cpu.lock().map(|t| *t).unwrap_or(0.0);
            let gpu = self.gpu.lock().map(|t| *t).unwrap_or(0.0);
            let mut fake = self
                .fake
                .lock()
                .map_err(|_| thermite_core::BackendError::Invalid("poisoned".into()))?;
            fake.set_temps(cpu, gpu);
            fake.read_telemetry()
        }
        fn set_fan_hw_mode(
            &mut self,
            fan: FanId,
            mode: HwFanMode,
        ) -> Result<(), thermite_core::BackendError> {
            self.fake
                .lock()
                .map_err(|_| thermite_core::BackendError::Invalid("poisoned".into()))?
                .set_fan_hw_mode(fan, mode)
        }
        fn set_fan_duty(
            &mut self,
            fan: FanId,
            duty: u8,
        ) -> Result<(), thermite_core::BackendError> {
            self.fake
                .lock()
                .map_err(|_| thermite_core::BackendError::Invalid("poisoned".into()))?
                .set_fan_duty(fan, duty)
        }
        fn set_platform_profile(
            &mut self,
            profile: &str,
        ) -> Result<(), thermite_core::BackendError> {
            self.fake
                .lock()
                .map_err(|_| thermite_core::BackendError::Invalid("poisoned".into()))?
                .set_platform_profile(profile)
        }
    }

    type TempHandle = Arc<Mutex<f32>>;
    type FakeHandle = Arc<Mutex<FakeBackend>>;

    fn setup() -> (
        ControlLoop,
        Arc<Mutex<SharedState>>,
        Arc<Subscribers>,
        FakeHandle,
        TempHandle,
        TempHandle,
    ) {
        let (backend, fake, cpu, gpu) = ScriptedBackend::new();
        let config = Config::with_default_curves();
        let shared = Arc::new(Mutex::new(SharedState::new(
            config,
            &crate::state::PersistedState::default(),
            false,
        )));
        let subs = Arc::new(Subscribers::default());
        let paths = DaemonPaths {
            config: PathBuf::from("/nonexistent/thermite.toml"),
            socket: PathBuf::from("/nonexistent/t.sock"),
            state_dir: std::env::temp_dir(),
        };
        let control = ControlLoop::new(
            Box::new(backend),
            Arc::clone(&shared),
            Arc::clone(&subs),
            paths,
            false,
        );
        (control, shared, subs, fake, cpu, gpu)
    }

    fn set_fan_mode(shared: &Arc<Mutex<SharedState>>, fan: &str, config: FanConfig) {
        if let Ok(mut g) = shared.lock()
            && let Some(rt) = g.fans.get_mut(fan)
        {
            rt.config = config;
            rt.commanded_duty = None;
        }
    }

    fn curve_mode(name: &str) -> FanConfig {
        FanConfig {
            mode: FanModeKind::Curve,
            curve: Some(name.to_string()),
            manual_duty_pct: None,
        }
    }

    fn ops(fake: &Arc<Mutex<FakeBackend>>, fan: FanId) -> Vec<WriteOp> {
        let fan_name = fan.to_string();
        fake.lock()
            .map(|f| f.write_log().all())
            .unwrap_or_default()
            .into_iter()
            .filter(|op| match op {
                WriteOp::Mode { fan, .. } | WriteOp::Duty { fan, .. } => {
                    fan.to_string() == fan_name
                }
                WriteOp::Profile { .. } => false,
            })
            .collect()
    }

    #[test]
    fn curve_following_with_reassert() {
        let (mut control, shared, _subs, fake, _cpu_t, _gpu_t) = setup();
        set_fan_mode(&shared, "cpu", curve_mode("balanced"));
        let t0 = Instant::now();

        control.tick_once(t0);
        // FakeBackend defaults: cpu 55°C -> balanced curve duty 55%.
        let writes = ops(&fake, FanId::Cpu);
        assert_eq!(
            writes,
            vec![
                WriteOp::Mode {
                    fan: FanId::Cpu,
                    mode: HwFanMode::Custom
                },
                WriteOp::Duty {
                    fan: FanId::Cpu,
                    duty_pct: 55
                },
            ]
        );
        // GPU stays in firmware auto: no writes.
        assert!(ops(&fake, FanId::Gpu).is_empty());

        // Unchanged state: no writes before the re-assert deadline.
        control.tick_once(t0 + Duration::from_millis(1500));
        assert_eq!(ops(&fake, FanId::Cpu).len(), 2);

        // Re-assert deadline passes (2000ms): mode+duty written again.
        control.tick_once(t0 + Duration::from_millis(3100));
        assert_eq!(ops(&fake, FanId::Cpu).len(), 4);
    }

    #[test]
    fn manual_duty_gets_floors() {
        let (mut control, shared, _subs, fake, cpu_t, _gpu_t) = setup();
        // Fake cpu temp is 70°C -> duty floor 30 applies.
        *cpu_t.lock().unwrap() = 70.0;
        set_fan_mode(
            &shared,
            "cpu",
            FanConfig {
                mode: FanModeKind::Manual,
                curve: None,
                manual_duty_pct: Some(20),
            },
        );
        control.tick_once(Instant::now());
        let writes = ops(&fake, FanId::Cpu);
        assert!(writes.contains(&WriteOp::Duty {
            fan: FanId::Cpu,
            duty_pct: 30
        }));
    }

    #[test]
    fn zero_duty_not_allowed_above_zero_threshold() {
        let (mut control, shared, _subs, fake, _cpu_t, _gpu_t) = setup();
        // Curve that wants 0% everywhere; fake cpu temp 55°C > 45°C cap.
        {
            let mut g = shared.lock().unwrap();
            g.config.curves.insert(
                "flat-zero".into(),
                CurveDef::from_points(vec![CurvePoint(45.0, 0), CurvePoint(46.0, 0)]),
            );
        }
        set_fan_mode(&shared, "cpu", curve_mode("flat-zero"));
        control.tick_once(Instant::now());
        assert!(
            ops(&fake, FanId::Cpu)
                .iter()
                .any(|op| matches!(op, WriteOp::Duty { duty_pct: 15, .. })),
            "expected min-spin duty 15, got {:?}",
            ops(&fake, FanId::Cpu)
        );
    }

    #[test]
    fn failsafe_warn_forces_full_then_crit_releases_and_recovers() {
        let (mut control, shared, _subs, fake, cpu_t, gpu_t) = setup();
        set_fan_mode(&shared, "cpu", curve_mode("balanced"));
        let t = Instant::now();

        control.tick_once(t); // normal: Custom + 40

        // Warn threshold (cpu 87): force full duty.
        *cpu_t.lock().unwrap() = 88.0;
        *gpu_t.lock().unwrap() = 50.0;
        control.tick_once(t + Duration::from_secs(1));
        let writes = ops(&fake, FanId::Cpu);
        assert!(
            writes.contains(&WriteOp::Duty {
                fan: FanId::Cpu,
                duty_pct: 100
            }),
            "expected full duty at warn, got {writes:?}"
        );
        assert!(matches!(control.failsafe.phase(), FailsafePhase::ForceFull));

        // Crit threshold (cpu 92): release to firmware auto.
        *cpu_t.lock().unwrap() = 93.0;
        control.tick_once(t + Duration::from_secs(2));
        let writes = ops(&fake, FanId::Cpu);
        assert!(
            writes.contains(&WriteOp::Mode {
                fan: FanId::Cpu,
                mode: HwFanMode::Auto
            }),
            "expected auto write at crit, got {writes:?}"
        );
        assert!(matches!(
            control.failsafe.phase(),
            FailsafePhase::ReleasedAuto
        ));

        // Still latched at 90 (crit 92 - recovery 5 = 87).
        let before = ops(&fake, FanId::Cpu).len();
        *cpu_t.lock().unwrap() = 90.0;
        control.tick_once(t + Duration::from_secs(3));
        assert_eq!(
            ops(&fake, FanId::Cpu).len(),
            before,
            "no duty writes while latched"
        );

        // Recovered at 85: manual control resumes.
        *cpu_t.lock().unwrap() = 85.0;
        control.tick_once(t + Duration::from_secs(4));
        assert!(matches!(control.failsafe.phase(), FailsafePhase::Normal));
        let tail = ops(&fake, FanId::Cpu);
        assert!(
            tail.contains(&WriteOp::Mode {
                fan: FanId::Cpu,
                mode: HwFanMode::Custom
            }),
            "expected manual control restored, got {tail:?}"
        );

        // Alerts were recorded.
        let alerts = shared
            .lock()
            .unwrap()
            .alerts
            .iter()
            .map(|a| a.kind)
            .collect::<Vec<_>>();
        assert!(alerts.contains(&thermite_proto::AlertKind::FailsafeFull));
        assert!(alerts.contains(&thermite_proto::AlertKind::FailsafeReleased));
        assert!(alerts.contains(&thermite_proto::AlertKind::FailsafeRecovered));
    }

    #[test]
    fn suspend_restores_auto_and_grace_blocks_writes() {
        let (mut control, shared, _subs, fake, _cpu_t, _gpu_t) = setup();
        set_fan_mode(&shared, "cpu", curve_mode("balanced"));
        let t = Instant::now();
        control.tick_once(t);
        assert_eq!(ops(&fake, FanId::Cpu).len(), 2);

        control.handle_suspend(SuspendEvent::Sleeping, t + Duration::from_secs(10));
        let writes = ops(&fake, FanId::Cpu);
        assert!(writes.contains(&WriteOp::Mode {
            fan: FanId::Cpu,
            mode: HwFanMode::Auto
        }));

        control.handle_suspend(SuspendEvent::Resumed, t + Duration::from_secs(30));
        let baseline = ops(&fake, FanId::Cpu).len();

        // Inside the 20s grace: telemetry flows, writes do not.
        control.tick_once(t + Duration::from_secs(35));
        assert_eq!(ops(&fake, FanId::Cpu).len(), baseline);

        // Grace over: state re-applied.
        control.tick_once(t + Duration::from_secs(55));
        assert!(ops(&fake, FanId::Cpu).len() > baseline);
        let tail = ops(&fake, FanId::Cpu);
        assert!(tail.contains(&WriteOp::Mode {
            fan: FanId::Cpu,
            mode: HwFanMode::Custom
        }));
    }

    #[test]
    fn external_override_is_detected_and_alerted() {
        let (mut control, shared, _subs, fake, _cpu_t, _gpu_t) = setup();
        set_fan_mode(&shared, "cpu", curve_mode("balanced"));
        let t = Instant::now();
        control.tick_once(t);

        // Someone else forces the fan back to auto at low duty. Re-apply the
        // override every tick to simulate a persistent interferer.
        let hijack = |fake: &Arc<Mutex<FakeBackend>>| {
            fake.lock().unwrap().set_fan_readback(
                FanId::Cpu,
                Some(800),
                Some(0),
                Some(HwFanMode::Auto),
            );
        };
        hijack(&fake);
        control.tick_once(t + Duration::from_secs(1));
        let alerts = shared
            .lock()
            .unwrap()
            .alerts
            .iter()
            .map(|a| a.kind)
            .collect::<Vec<_>>();
        assert!(
            alerts.contains(&thermite_proto::AlertKind::ExternalOverride),
            "alerts: {alerts:?}"
        );

        // Cooldown: a second override tick within 60s does not re-alert.
        hijack(&fake);
        control.tick_once(t + Duration::from_secs(2));
        let count = shared
            .lock()
            .unwrap()
            .alerts
            .iter()
            .filter(|a| a.kind == thermite_proto::AlertKind::ExternalOverride)
            .count();
        assert_eq!(count, 1, "cooldown should suppress duplicate alerts");

        // And the control loop wrote the correction (mode back to custom).
        let writes = ops(&fake, FanId::Cpu);
        assert!(writes.contains(&WriteOp::Mode {
            fan: FanId::Cpu,
            mode: HwFanMode::Custom
        }));
    }

    #[test]
    fn profile_applied_once_and_rejection_recorded() {
        let (mut control, shared, _subs, fake, _cpu_t, _gpu_t) = setup();
        if let Ok(mut g) = shared.lock() {
            g.desired_profile = Some("quiet".to_string());
        }
        let t = Instant::now();
        control.tick_once(t);
        let profiles = fake
            .lock()
            .unwrap()
            .write_log()
            .all()
            .into_iter()
            .filter(|op| matches!(op, WriteOp::Profile { .. }))
            .count();
        assert_eq!(profiles, 1, "profile applied exactly once");
        assert!(shared.lock().unwrap().profile_applied);

        // Not re-applied on the next tick.
        control.tick_once(t + Duration::from_secs(1));
        let profiles = fake
            .lock()
            .unwrap()
            .write_log()
            .all()
            .into_iter()
            .filter(|op| matches!(op, WriteOp::Profile { .. }))
            .count();
        assert_eq!(profiles, 1);

        // Firmware rejection path: new profile + failing writes.
        if let Ok(mut g) = shared.lock() {
            g.desired_profile = Some("performance".to_string());
            g.profile_applied = false;
        }
        fake.lock().unwrap().fail_writes(true);
        control.tick_once(t + Duration::from_secs(2));
        assert!(shared.lock().unwrap().profile_error.is_some());
    }

    #[test]
    fn shutdown_restores_auto_and_stops_writing() {
        let (mut control, shared, _subs, fake, _cpu_t, _gpu_t) = setup();
        set_fan_mode(&shared, "cpu", curve_mode("balanced"));
        control.tick_once(Instant::now());
        shared.lock().unwrap().shutting_down = true;
        control.tick_once(Instant::now() + Duration::from_secs(1));
        let writes = ops(&fake, FanId::Cpu);
        assert!(writes.contains(&WriteOp::Mode {
            fan: FanId::Cpu,
            mode: HwFanMode::Auto
        }));
        let baseline = writes.len();
        control.tick_once(Instant::now() + Duration::from_secs(2));
        assert_eq!(
            ops(&fake, FanId::Cpu).len(),
            baseline,
            "no further writes after shutdown restore"
        );
    }

    #[test]
    fn reload_flag_is_honored_by_run() {
        // Smoke: run() exits promptly on stop; reload of a nonexistent file
        // keeps the old config and alerts.
        let (mut control, shared, _subs, _fake, _cpu_t, _gpu_t) = setup();
        let reload = AtomicBool::new(true);
        control.reload_config(); // /nonexistent path -> rejection alert
        let alerts = shared
            .lock()
            .unwrap()
            .alerts
            .iter()
            .map(|a| a.kind)
            .collect::<Vec<_>>();
        assert!(alerts.contains(&thermite_proto::AlertKind::ConfigRejected));
        let stop = AtomicBool::new(true); // already stopping
        let (_tx, rx) = std::sync::mpsc::channel();
        control.run(&stop, &rx, &reload);
    }
}
