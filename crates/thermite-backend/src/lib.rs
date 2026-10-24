//! # thermite-backend
//!
//! Hardware access for Thermite. Everything here is sysfs/NVML plumbing;
//! policy lives in `thermite-core`.
//!
//! ## Layout
//! - [`capability`] — probing what the acer-wmi gaming interface offers
//! - [`acer_wmi`] — fan/pwm/platform-profile control via the mainline driver
//! - [`hwmon`] — generic hwmon sensor scanning
//! - [`power`] — AC/battery state
//! - [`nvml`] — NVIDIA GPU telemetry
//! - [`fake`] — in-memory backend for tests

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod acer_wmi;
pub mod capability;
pub mod fake;
pub mod hwmon;
pub mod nvml;
pub mod power;
pub mod sysfs;

pub use acer_wmi::AcerWmiBackend;
pub use fake::{FakeBackend, WriteOp};

use thermite_core::{Backend, CapabilityReport, Telemetry};

/// Composite backend: acer-wmi fans/profiles + generic sensors + NVML,
/// layered so telemetry works even when the gaming interface is absent.
pub struct ThermiteBackend {
    acer: AcerWmiBackend,
    nvml: Option<nvml::NvmlSource>,
}

impl ThermiteBackend {
    /// Probe the machine and assemble the best available backend.
    pub fn probe() -> Self {
        Self {
            acer: AcerWmiBackend::probe(),
            nvml: Some(nvml::NvmlSource::new()),
        }
    }
}

impl Default for ThermiteBackend {
    fn default() -> Self {
        Self::probe()
    }
}

impl Backend for ThermiteBackend {
    fn capability(&self) -> CapabilityReport {
        let mut report = self.acer.capability();
        if let Some(n) = &self.nvml {
            for note in n.notes() {
                report.notes.push(note);
            }
        }
        report
    }

    fn read_telemetry(&mut self) -> Result<Telemetry, thermite_core::BackendError> {
        let mut telemetry = self.acer.read_telemetry()?;
        telemetry.temps.extend(hwmon::scan_temps()?);
        telemetry.gpu = self.nvml.as_mut().and_then(|n| n.read());
        telemetry.on_ac = power::on_ac();
        Ok(telemetry)
    }

    fn set_fan_hw_mode(
        &mut self,
        fan: thermite_core::FanId,
        mode: thermite_core::HwFanMode,
    ) -> Result<(), thermite_core::BackendError> {
        self.acer.set_fan_hw_mode(fan, mode)
    }

    fn set_fan_duty(
        &mut self,
        fan: thermite_core::FanId,
        duty_pct: u8,
    ) -> Result<(), thermite_core::BackendError> {
        self.acer.set_fan_duty(fan, duty_pct)
    }

    fn set_platform_profile(&mut self, profile: &str) -> Result<(), thermite_core::BackendError> {
        self.acer.set_platform_profile(profile)
    }
}
