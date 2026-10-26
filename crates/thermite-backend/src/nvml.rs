//! NVIDIA GPU telemetry via NVML.
//!
//! The 615.x proprietary driver exposes no hwmon interface on this machine,
//! so NVML supplements the GPU hwmon sensor with name + power draw.
//! NVML calls can occasionally block for ~1 s (driver memory-pressure
//! windows), so reads are throttled to [`READ_TTL`] and back off
//! exponentially after errors — a stalled call must never happen every
//! control tick.

use std::time::{Duration, Instant};

use thermite_core::GpuTelemetry;

/// How long one NVML reading is trusted (GPU temp itself comes from hwmon
/// every tick; NVML adds the display-only name + power draw).
pub const READ_TTL: Duration = Duration::from_secs(5);
/// First backoff step after a failed read.
const BACKOFF_START: Duration = Duration::from_secs(1);
/// Backoff ceiling.
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Generic throttle + exponential-backoff wrapper, testable without NVML.
#[derive(Debug, Default)]
pub struct Throttle {
    last_ok: Option<Instant>,
    next_allowed: Option<Instant>,
    backoff: Duration,
}

impl Throttle {
    /// Whether a read should happen now.
    fn due(&self, now: Instant) -> bool {
        match (self.last_ok, self.next_allowed) {
            (_, Some(t)) => now >= t,
            (None, None) => true,
            (Some(t), None) => now.duration_since(t) >= READ_TTL,
        }
    }

    fn record_ok(&mut self, now: Instant) {
        self.last_ok = Some(now);
        self.next_allowed = None;
        self.backoff = BACKOFF_START;
    }

    fn record_err(&mut self, now: Instant) {
        self.backoff = (self.backoff.max(BACKOFF_START) * 2).min(BACKOFF_MAX);
        self.next_allowed = Some(now + self.backoff);
    }

    /// True while in an error backoff window.
    fn backing_off(&self) -> bool {
        self.next_allowed.is_some()
    }
}

/// Lazily-initialized NVML handle; `None` inside when unavailable.
pub struct NvmlSource {
    #[cfg(feature = "nvml")]
    nvml: Option<nvml_wrapper::Nvml>,
    /// Device name, cached from the first successful access.
    #[cfg(feature = "nvml")]
    name: Option<String>,
    /// Most recent successful reading, served within the cache window.
    #[cfg(feature = "nvml")]
    last: Option<GpuTelemetry>,
    throttle: Throttle,
    note: Option<String>,
}

impl NvmlSource {
    /// Try to initialize NVML; failures degrade to an empty source with a
    /// diagnostic note (surfaced by `thermite doctor` and status).
    pub fn new() -> Self {
        let mut note = None;
        #[cfg(feature = "nvml")]
        let nvml = match nvml_wrapper::Nvml::init() {
            Ok(n) => Some(n),
            Err(e) => {
                note = Some(format!("NVML unavailable ({e}); GPU temp via hwmon only"));
                None
            }
        };
        Self {
            #[cfg(feature = "nvml")]
            nvml,
            #[cfg(feature = "nvml")]
            name: None,
            #[cfg(feature = "nvml")]
            last: None,
            throttle: Throttle::default(),
            note,
        }
    }

    /// Read GPU telemetry, refreshed at most every [`READ_TTL`] (cached
    /// readings are served in between) with exponential error backoff;
    /// `None` only when NVML is unavailable or has never succeeded.
    #[cfg(feature = "nvml")]
    pub fn read(&mut self) -> Option<GpuTelemetry> {
        self.read_at(Instant::now())
    }

    /// `read()` with an injectable clock (tests).
    #[cfg(feature = "nvml")]
    fn read_at(&mut self, now: Instant) -> Option<GpuTelemetry> {
        if self.throttle.due(now) {
            // Creating the device handle is a cheap in-process lookup; the
            // blocking risk lives in the temperature/power calls below,
            // which now run at most once per READ_TTL (or per backoff).
            let reading = self.nvml.as_ref().and_then(|nvml| {
                let device = nvml.device_by_index(0).ok()?;
                if self.name.is_none() {
                    self.name = Some(
                        device
                            .name()
                            .ok()
                            .unwrap_or_else(|| "NVIDIA GPU".to_string()),
                    );
                }
                let temp = device
                    .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                    .ok()?;
                let power = device.power_usage().unwrap_or(0);
                Some((temp, power))
            });
            match reading {
                Some((temp_c, mw)) => {
                    self.throttle.record_ok(now);
                    self.last = Some(GpuTelemetry {
                        name: self.name.clone().unwrap_or_else(|| "NVIDIA GPU".into()),
                        temp_c: temp_c as f32,
                        power_w: mw as f32 / 1000.0,
                    });
                }
                None => self.throttle.record_err(now),
            }
        }
        self.last.clone()
    }

    #[cfg(not(feature = "nvml"))]
    pub fn read(&mut self) -> Option<GpuTelemetry> {
        None
    }

    /// True while the source is in an error backoff window (diagnostics).
    pub fn backing_off(&self) -> bool {
        self.throttle.backing_off()
    }

    /// Diagnostic notes for capability reports.
    pub fn notes(&self) -> Vec<String> {
        match (&self.note, cfg!(feature = "nvml")) {
            (Some(n), _) => vec![n.clone()],
            (None, true) => Vec::new(),
            (None, false) => vec!["built without NVML support".to_string()],
        }
    }
}

impl Default for NvmlSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_caches_then_backs_off() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        assert!(t.due(t0));

        t.record_ok(t0);
        assert!(!t.due(t0 + Duration::from_secs(4)), "within cache window");
        assert!(t.due(t0 + READ_TTL), "after READ_TTL");

        t.record_err(t0 + READ_TTL);
        assert!(!t.due(t0 + READ_TTL + Duration::from_millis(500)));
        assert!(t.due(t0 + READ_TTL + Duration::from_secs(2)));
        assert!(t.backing_off());

        // Backoff doubles (1->2->4->...) and caps at 60s.
        for s in [2, 6, 20, 60, 130, 260] {
            t.record_err(t0 + READ_TTL + Duration::from_secs(s));
        }
        assert_eq!(t.backoff, BACKOFF_MAX);

        // A success clears the backoff state.
        t.record_ok(t0 + READ_TTL + Duration::from_secs(20));
        assert!(!t.backing_off());
    }
}
