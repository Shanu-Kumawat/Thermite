//! In-memory [`Backend`] for tests and `--dry-run` style development.

use std::sync::{Arc, Mutex};

use thermite_core::{
    Backend, BackendError, CapabilityLevel, CapabilityReport, FanDescriptor, FanId, FanTelemetry,
    HwFanMode, Telemetry, TempClass, TempSource, TempTelemetry,
};

/// A write operation recorded by [`FakeBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Fan mode change.
    Mode {
        /// Target fan.
        fan: FanId,
        /// Mode written.
        mode: HwFanMode,
    },
    /// Fan duty change (percent).
    Duty {
        /// Target fan.
        fan: FanId,
        /// Duty written.
        duty_pct: u8,
    },
    /// Platform profile change.
    Profile {
        /// Profile name written.
        profile: String,
    },
}

/// Shared handle to a [`FakeBackend`]'s recorded writes, usable from another
/// thread (e.g. a daemon-under-test).
#[derive(Debug, Clone, Default)]
pub struct WriteLog(Arc<Mutex<Vec<WriteOp>>>);

impl WriteLog {
    /// All writes so far.
    pub fn all(&self) -> Vec<WriteOp> {
        self.0.lock().map(|v| v.clone()).unwrap_or_default()
    }

    fn push(&self, op: WriteOp) {
        if let Ok(mut v) = self.0.lock() {
            v.push(op);
        }
    }
}

/// Scriptable backend: telemetry is settable, writes are recorded (and can
/// be made to fail).
pub struct FakeBackend {
    telemetry: Telemetry,
    capability: CapabilityReport,
    fans: Vec<FanDescriptor>,
    log: WriteLog,
    fail_writes: bool,
}

impl FakeBackend {
    /// A benign default: cpu+gpu fans spinning in auto mode, CPU 55°C, GPU
    /// 45°C, on AC, balanced profile.
    pub fn new() -> Self {
        Self::with_fans(&[
            ("cpu", "CPU Fan", TempSource::Cpu),
            ("gpu", "GPU Fan", TempSource::Gpu),
        ])
    }

    /// Build a fake with arbitrary fans (N-fan tests).
    pub fn with_fans(fans: &[(&str, &str, TempSource)]) -> Self {
        let descriptors = fans
            .iter()
            .map(|&(id, label, ref source)| FanDescriptor {
                id: FanId::new(id),
                label: label.to_string(),
                temp_source: source.clone(),
            })
            .collect::<Vec<_>>();

        let mut telemetry = Telemetry::default();
        for d in &descriptors {
            telemetry.fans.push(FanTelemetry {
                id: d.id.clone(),
                rpm: Some(2000),
                duty_pct: Some(40),
                hw_mode: Some(HwFanMode::Auto),
            });
        }
        telemetry.temps = vec![
            TempTelemetry {
                label: "CPU".into(),
                class: TempClass::Cpu,
                temp_c: 55.0,
            },
            TempTelemetry {
                label: "GPU".into(),
                class: TempClass::Gpu,
                temp_c: 45.0,
            },
        ];
        telemetry.on_ac = Some(true);
        telemetry.platform_profile = Some("balanced".into());

        let capability = CapabilityReport {
            level: CapabilityLevel::Full,
            driver: Some("fake".into()),
            driver_detail: Some("(fake)".into()),
            platform_profiles: vec![
                "low-power".into(),
                "quiet".into(),
                "balanced".into(),
                "balanced-performance".into(),
                "performance".into(),
            ],
            notes: vec!["fake backend".into()],
        };
        Self {
            telemetry,
            capability,
            fans: descriptors,
            log: WriteLog::default(),
            fail_writes: false,
        }
    }

    /// Replace the telemetry returned by reads.
    pub fn set_telemetry(&mut self, telemetry: Telemetry) {
        self.telemetry = telemetry;
    }

    /// Override class temperatures (CPU, GPU).
    pub fn set_temps(&mut self, cpu_c: f32, gpu_c: f32) {
        self.set_temp_by_class(TempClass::Cpu, cpu_c);
        self.set_temp_by_class(TempClass::Gpu, gpu_c);
    }

    /// Override the temperature of every sensor of one class, inserting one
    /// if none exists yet.
    pub fn set_temp_by_class(&mut self, class: TempClass, temp_c: f32) {
        for t in &mut self.telemetry.temps {
            if t.class == class {
                t.temp_c = temp_c;
            }
        }
        if !self.telemetry.temps.iter().any(|t| t.class == class) {
            self.telemetry.temps.push(TempTelemetry {
                label: format!("{class:?}"),
                class,
                temp_c,
            });
        }
    }

    /// Set the fan readback (RPM/duty/mode) the backend reports.
    pub fn set_fan_readback(
        &mut self,
        fan: &FanId,
        rpm: Option<u32>,
        duty: Option<u8>,
        mode: Option<HwFanMode>,
    ) {
        if let Some(f) = self.telemetry.fans.iter_mut().find(|f| &f.id == fan) {
            f.rpm = rpm;
            f.duty_pct = duty;
            f.hw_mode = mode;
        }
    }

    /// Downgrade capabilities.
    pub fn set_capability(&mut self, level: CapabilityLevel) {
        self.capability.level = level;
    }

    /// Make all writes fail with [`BackendError::Invalid`].
    pub fn fail_writes(&mut self, fail: bool) {
        self.fail_writes = fail;
    }

    /// Clone the write log handle (shares state with the backend).
    pub fn write_log(&self) -> WriteLog {
        self.log.clone()
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for FakeBackend {
    fn fans(&self) -> &[FanDescriptor] {
        &self.fans
    }

    fn capability(&self) -> CapabilityReport {
        self.capability.clone()
    }

    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError> {
        Ok(self.telemetry.clone())
    }

    fn set_fan_hw_mode(&mut self, fan: &FanId, mode: HwFanMode) -> Result<(), BackendError> {
        if self.fail_writes {
            return Err(BackendError::Invalid("fake failure".into()));
        }
        self.log.push(WriteOp::Mode {
            fan: fan.clone(),
            mode,
        });
        if let Some(f) = self.telemetry.fans.iter_mut().find(|f| &f.id == fan) {
            f.hw_mode = Some(mode);
        }
        Ok(())
    }

    fn set_fan_duty(&mut self, fan: &FanId, duty_pct: u8) -> Result<(), BackendError> {
        if self.fail_writes {
            return Err(BackendError::Invalid("fake failure".into()));
        }
        if duty_pct > 100 {
            return Err(BackendError::Invalid("duty out of range".into()));
        }
        self.log.push(WriteOp::Duty {
            fan: fan.clone(),
            duty_pct,
        });
        if let Some(f) = self.telemetry.fans.iter_mut().find(|f| &f.id == fan) {
            f.duty_pct = Some(duty_pct);
        }
        Ok(())
    }

    fn set_platform_profile(&mut self, profile: &str) -> Result<(), BackendError> {
        if self.fail_writes {
            return Err(BackendError::Invalid("fake failure".into()));
        }
        self.log.push(WriteOp::Profile {
            profile: profile.into(),
        });
        self.telemetry.platform_profile = Some(profile.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n_fan_fake_reports_all_fans() {
        let mut b = FakeBackend::with_fans(&[
            ("cpu", "CPU Fan", TempSource::Cpu),
            ("gpu", "GPU Fan", TempSource::Gpu),
            ("intake", "Intake Fan", TempSource::Named("Board".into())),
        ]);
        assert_eq!(b.fans().len(), 3);
        let t = b.read_telemetry().unwrap();
        assert_eq!(t.fans.len(), 3);
        assert_eq!(
            t.temp_for(&TempSource::Named("Board".into())),
            None,
            "no Board sensor in defaults"
        );
        b.set_fan_duty(&FanId::new("intake"), 60).unwrap();
        assert!(b.write_log().all().iter().any(
            |op| matches!(op, WriteOp::Duty { fan, duty_pct: 60 } if fan.as_str() == "intake")
        ));
    }
}
