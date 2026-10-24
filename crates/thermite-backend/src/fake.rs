//! In-memory [`Backend`] for tests and `--dry-run` style development.

use std::sync::{Arc, Mutex};

use thermite_core::{
    Backend, BackendError, CapabilityLevel, CapabilityReport, FanId, FanTelemetry, HwFanMode,
    Telemetry, TempClass, TempTelemetry,
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
    log: WriteLog,
    fail_writes: bool,
}

impl FakeBackend {
    /// A benign default: both fans spinning in auto mode, CPU 55°C, GPU
    /// 45°C, on AC, balanced profile.
    pub fn new() -> Self {
        let telemetry = Telemetry {
            fans: FanId::all()
                .map(|id| FanTelemetry {
                    id,
                    rpm: Some(2000),
                    duty_pct: Some(40),
                    hw_mode: Some(HwFanMode::Auto),
                })
                .to_vec(),
            temps: vec![
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
            ],
            gpu: None,
            on_ac: Some(true),
            platform_profile: Some("balanced".into()),
            tick_ms: 0,
        };
        let capability = CapabilityReport {
            level: CapabilityLevel::Full,
            acer_wmi_hwmon: Some("(fake)".into()),
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
            log: WriteLog::default(),
            fail_writes: false,
        }
    }

    /// Replace the telemetry returned by reads.
    pub fn set_telemetry(&mut self, telemetry: Telemetry) {
        self.telemetry = telemetry;
    }

    /// Override individual temperatures conveniently (CPU first, GPU second).
    pub fn set_temps(&mut self, cpu_c: f32, gpu_c: f32) {
        for t in &mut self.telemetry.temps {
            match t.class {
                TempClass::Cpu => t.temp_c = cpu_c,
                TempClass::Gpu => t.temp_c = gpu_c,
                _ => {}
            }
        }
        if !self
            .telemetry
            .temps
            .iter()
            .any(|t| t.class == TempClass::Cpu)
        {
            self.telemetry.temps.push(TempTelemetry {
                label: "CPU".into(),
                class: TempClass::Cpu,
                temp_c: cpu_c,
            });
        }
        if !self
            .telemetry
            .temps
            .iter()
            .any(|t| t.class == TempClass::Gpu)
        {
            self.telemetry.temps.push(TempTelemetry {
                label: "GPU".into(),
                class: TempClass::Gpu,
                temp_c: gpu_c,
            });
        }
    }

    /// Set the fan readback (RPM/duty/mode) the backend reports.
    pub fn set_fan_readback(
        &mut self,
        fan: FanId,
        rpm: Option<u32>,
        duty: Option<u8>,
        mode: Option<HwFanMode>,
    ) {
        if let Some(f) = self.telemetry.fans.iter_mut().find(|f| f.id == fan) {
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
    fn capability(&self) -> CapabilityReport {
        self.capability.clone()
    }

    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError> {
        Ok(self.telemetry.clone())
    }

    fn set_fan_hw_mode(&mut self, fan: FanId, mode: HwFanMode) -> Result<(), BackendError> {
        if self.fail_writes {
            return Err(BackendError::Invalid("fake failure".into()));
        }
        self.log.push(WriteOp::Mode { fan, mode });
        if let Some(f) = self.telemetry.fans.iter_mut().find(|f| f.id == fan) {
            f.hw_mode = Some(mode);
        }
        Ok(())
    }

    fn set_fan_duty(&mut self, fan: FanId, duty_pct: u8) -> Result<(), BackendError> {
        if self.fail_writes {
            return Err(BackendError::Invalid("fake failure".into()));
        }
        if duty_pct > 100 {
            return Err(BackendError::Invalid("duty out of range".into()));
        }
        self.log.push(WriteOp::Duty { fan, duty_pct });
        if let Some(f) = self.telemetry.fans.iter_mut().find(|f| f.id == fan) {
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
