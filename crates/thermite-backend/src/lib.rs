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

use thermite_core::{
    Backend, BackendError, CapabilityReport, FanDescriptor, FanId, HwFanMode, Telemetry,
};

/// Composite backend: the matched device driver + generic sensors + NVML,
/// layered so telemetry works even when no driver matches.
pub struct ThermiteBackend {
    driver: Option<Box<dyn Backend>>,
    nvml: Option<nvml::NvmlSource>,
}

impl ThermiteBackend {
    /// Probe the machine and assemble the best available backend.
    pub fn probe() -> Self {
        Self {
            driver: drivers::probe_drivers(std::path::Path::new("/")).map(|m| m.backend),
            nvml: Some(nvml::NvmlSource::new()),
        }
    }

    /// Probe under an alternative filesystem root (tests).
    pub fn with_root(root: &std::path::Path) -> Self {
        Self {
            driver: drivers::probe_drivers(root).map(|m| m.backend),
            nvml: None,
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
        telemetry.temps.extend(hwmon::scan_temps()?);
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
