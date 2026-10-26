//! # thermite-backend
//!
//! Hardware access for Thermite. Everything here is sysfs/NVML plumbing;
//! policy lives in `thermite-core`.
//!
//! ## Layout
//! - [`drivers`] — vendor device drivers (acer-wmi today) behind a
//!   registry that picks the first matching driver
//! - [`capability`] — probing what the acer-wmi driver offers
//! - [`hwmon`] — generic hwmon sensor scanning
//! - [`power`] — AC/battery state
//! - [`nvml`] — NVIDIA GPU telemetry
//! - [`fake`] — in-memory backend for tests

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod capability;
pub mod drivers;
pub mod fake;
pub mod hwmon;
pub mod nvml;
pub mod power;
pub mod sysfs;

pub use drivers::acer_wmi::AcerWmiBackend;
pub use drivers::{Driver, DriverMatch};
pub use fake::{FakeBackend, WriteOp};

use std::time::{Duration, Instant};

use thermite_core::{
    Backend, BackendError, CapabilityReport, FanDescriptor, FanId, HwFanMode, Telemetry,
    TempTelemetry,
};

/// How long the display-only generic sensor scan (NVMe, board, wifi,
/// per-core temps) is trusted. NVMe reads cost ~5-7 ms each and per-core
/// temps ~2 ms; none of them feed fan control (the driver reports fresh
/// CPU/GPU-class temps every tick), so they refresh slowly.
pub const SCAN_TTL: Duration = Duration::from_secs(5);

/// Composite backend: the matched device driver + generic sensors + NVML,
/// layered so telemetry works even when no driver matches.
pub struct ThermiteBackend {
    driver: Option<Box<dyn Backend>>,
    nvml: Option<nvml::NvmlSource>,
    scan_cache: Option<(Instant, Vec<TempTelemetry>)>,
    scan_ttl: Duration,
}

impl ThermiteBackend {
    /// Probe the machine and assemble the best available backend.
    pub fn probe() -> Self {
        Self {
            driver: drivers::probe_drivers(std::path::Path::new("/")).map(|m| m.backend),
            nvml: Some(nvml::NvmlSource::new()),
            scan_cache: None,
            scan_ttl: SCAN_TTL,
        }
    }

    /// Probe under an alternative filesystem root (tests).
    pub fn with_root(root: &std::path::Path) -> Self {
        Self {
            driver: drivers::probe_drivers(root).map(|m| m.backend),
            nvml: None,
            scan_cache: None,
            scan_ttl: SCAN_TTL,
        }
    }

    /// Display-only sensor scan with a TTL cache; on a scan error the last
    /// good scan is served (these sensors are not control inputs).
    fn scan_temps_cached(&mut self) -> Vec<TempTelemetry> {
        let fresh = self
            .scan_cache
            .take()
            .filter(|(at, _)| at.elapsed() < self.scan_ttl);
        if let Some((at, temps)) = fresh {
            self.scan_cache = Some((at, temps.clone()));
            return temps;
        }
        match hwmon::scan_temps() {
            Ok(temps) => {
                self.scan_cache = Some((Instant::now(), temps.clone()));
                temps
            }
            Err(_) => self.scan_cache.take().map(|(_, t)| t).unwrap_or_default(),
        }
    }
}

impl Default for ThermiteBackend {
    fn default() -> Self {
        Self::probe()
    }
}

impl Backend for ThermiteBackend {
    fn fans(&self) -> &[FanDescriptor] {
        self.driver.as_ref().map(|d| d.fans()).unwrap_or(&[])
    }

    fn capability(&self) -> CapabilityReport {
        let mut report = match &self.driver {
            Some(d) => d.capability(),
            None => CapabilityReport::default(),
        };
        if let Some(n) = &self.nvml {
            for note in n.notes() {
                report.notes.push(note);
            }
        }
        report
    }

    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError> {
        let mut telemetry = match &mut self.driver {
            Some(d) => d.read_telemetry()?,
            None => Telemetry::default(),
        };
        telemetry.temps.extend(self.scan_temps_cached());
        telemetry.gpu = self.nvml.as_mut().and_then(|n| n.read());
        telemetry.on_ac = power::on_ac();
        Ok(telemetry)
    }

    fn set_fan_hw_mode(&mut self, fan: &FanId, mode: HwFanMode) -> Result<(), BackendError> {
        match &mut self.driver {
            Some(d) => d.set_fan_hw_mode(fan, mode),
            None => Err(BackendError::Unsupported(
                "no device driver matched this machine".to_string(),
            )),
        }
    }

    fn set_fan_duty(&mut self, fan: &FanId, duty_pct: u8) -> Result<(), BackendError> {
        match &mut self.driver {
            Some(d) => d.set_fan_duty(fan, duty_pct),
            None => Err(BackendError::Unsupported(
                "no device driver matched this machine".to_string(),
            )),
        }
    }

    fn set_platform_profile(&mut self, profile: &str) -> Result<(), BackendError> {
        match &mut self.driver {
            Some(d) => d.set_platform_profile(profile),
            None => Err(BackendError::Unsupported(
                "no device driver matched this machine".to_string(),
            )),
        }
    }
}
