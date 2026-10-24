//! Sensor/control telemetry types and the hardware [`Backend`] trait.
//!
//! These types are shared by the daemon, the IPC protocol, the CLI and the
//! GUI, so they carry serde derives and stable field semantics.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Which physical fan a setting or reading refers to.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum FanId {
    /// CPU fan (fan1 on the acer-wmi hwmon).
    #[default]
    Cpu,
    /// GPU fan (fan2 on the acer-wmi hwmon).
    Gpu,
}

impl FanId {
    /// Both fans in fixed order.
    pub fn all() -> [FanId; 2] {
        [FanId::Cpu, FanId::Gpu]
    }

    /// Lowercase name used in config keys and IPC.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        }
    }
}

impl std::fmt::Display for FanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Hardware fan mode as expressed by the acer-wmi `pwmX_enable` values.
///
/// NOTE: the ANV15-51 driver convention is *inverted* relative to the usual
/// hwmon convention — `0` = turbo (full), `1` = custom/manual, `2` = auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HwFanMode {
    /// `pwmX_enable = 0` — maximum speed.
    Turbo,
    /// `pwmX_enable = 1` — duty from `pwmX`.
    Custom,
    /// `pwmX_enable = 2` — firmware auto control.
    Auto,
}

impl HwFanMode {
    /// sysfs value for `pwmX_enable` (acer-wmi convention).
    pub fn enable_value(self) -> u8 {
        match self {
            Self::Turbo => 0,
            Self::Custom => 1,
            Self::Auto => 2,
        }
    }

    /// Parse a `pwmX_enable` readback; unknown values map to `None`.
    pub fn from_enable_value(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Turbo,
            1 => Self::Custom,
            2 => Self::Auto,
            _ => return None,
        })
    }
}

/// One fan's current hardware state.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct FanTelemetry {
    /// Which fan.
    pub id: FanId,
    /// RPM if reported.
    pub rpm: Option<u32>,
    /// Commanded duty percent readback (`pwmX`) if present.
    pub duty_pct: Option<u8>,
    /// `pwmX_enable` readback.
    pub hw_mode: Option<HwFanMode>,
}

/// Broad classification of a temperature sensor, used to pick control inputs
/// and dashboard grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TempClass {
    /// CPU package/cores (drives the CPU fan).
    Cpu,
    /// GPU (drives the GPU fan).
    Gpu,
    /// NVMe SSD.
    Nvme,
    /// Board/ACPI zones.
    Board,
    /// Wireless card.
    Wifi,
    /// Anything else.
    #[default]
    Other,
}

/// One temperature reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TempTelemetry {
    /// Human-readable label, e.g. `"Package id 0"` or `"Composite"`.
    pub label: String,
    /// Classification.
    pub class: TempClass,
    /// Temperature in °C.
    pub temp_c: f32,
}

/// NVIDIA GPU state via NVML (the 615.x driver exposes no hwmon).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuTelemetry {
    /// Device name, e.g. `"NVIDIA GeForce RTX 4050 Laptop GPU"`.
    pub name: String,
    /// GPU temperature in °C.
    pub temp_c: f32,
    /// Power draw in watts.
    pub power_w: f32,
}

/// One full telemetry snapshot, produced once per control tick and pushed to
/// IPC subscribers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Telemetry {
    /// Per-fan hardware state.
    pub fans: Vec<FanTelemetry>,
    /// All temperature sensors found.
    pub temps: Vec<TempTelemetry>,
    /// NVIDIA GPU via NVML, when available.
    pub gpu: Option<GpuTelemetry>,
    /// `true` on AC, `false` on battery, `None` unknown.
    pub on_ac: Option<bool>,
    /// Currently applied platform profile, when the interface exists.
    pub platform_profile: Option<String>,
    /// Milliseconds since the daemon started (monotonic-ish tick counter).
    pub tick_ms: u64,
}

impl Telemetry {
    /// Peak CPU temperature across CPU-class sensors (falls back to board
    /// zones). This is what drives the CPU fan and CPU failsafes.
    pub fn cpu_temp_c(&self) -> Option<f32> {
        let cpu = self
            .temps
            .iter()
            .filter(|t| t.class == TempClass::Cpu)
            .map(|t| t.temp_c)
            .fold(None, |acc: Option<f32>, t| {
                Some(acc.map_or(t, |a| a.max(t)))
            });
        cpu.or_else(|| self.board_max())
    }

    /// Peak GPU temperature (NVML preferred, then GPU-class sensors).
    pub fn gpu_temp_c(&self) -> Option<f32> {
        if let Some(g) = &self.gpu {
            return Some(g.temp_c);
        }
        self.temps
            .iter()
            .filter(|t| t.class == TempClass::Gpu)
            .map(|t| t.temp_c)
            .fold(None, |acc: Option<f32>, t| {
                Some(acc.map_or(t, |a| a.max(t)))
            })
            .or_else(|| self.board_max())
    }

    fn board_max(&self) -> Option<f32> {
        self.temps
            .iter()
            .filter(|t| t.class == TempClass::Board)
            .map(|t| t.temp_c)
            .fold(None, |acc: Option<f32>, t| {
                Some(acc.map_or(t, |a| a.max(t)))
            })
    }

    /// Telemetry for one fan.
    pub fn fan(&self, id: FanId) -> Option<&FanTelemetry> {
        self.fans.iter().find(|f| f.id == id)
    }
}

/// How much the acer-wmi gaming interface currently offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityLevel {
    /// No fan/profile interface at all (telemetry still available from
    /// generic hwmon sensors).
    #[default]
    None,
    /// Fan RPM/temps readable and platform profiles settable, but no manual
    /// pwm (`acer_wmi predator_v4=1` without the ANV15-51 quirk).
    MonitorProfiles,
    /// Full control: telemetry + `pwmX`/`pwmX_enable` manual fan control.
    Full,
}

/// What the backend found on this machine; powers `thermite doctor` and the
/// GUI's capability-aware UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilityReport {
    /// Coarse capability level.
    pub level: CapabilityLevel,
    /// Path of the acer-wmi hwmon directory when found.
    pub acer_wmi_hwmon: Option<String>,
    /// Platform profile choices exposed by the firmware.
    pub platform_profiles: Vec<String>,
    /// Human-readable findings (also shown by `doctor`).
    pub notes: Vec<String>,
}

/// Hardware access failures.
#[derive(Debug, Error)]
pub enum BackendError {
    /// Filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Not permitted (usually: daemon not running as root).
    #[error("permission denied: {0}")]
    Permission(String),
    /// Operation not available at the current capability level.
    #[error("unsupported on this machine/driver: {0}")]
    Unsupported(String),
    /// Value rejected by the driver.
    #[error("invalid value: {0}")]
    Invalid(String),
}

/// Hardware access abstraction implemented by `thermite-backend`
/// (sysfs/NVML) and by fakes in tests.
pub trait Backend: Send {
    /// Probe current capabilities (cheap; cacheable by callers).
    fn capability(&self) -> CapabilityReport;
    /// Read a full telemetry snapshot.
    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError>;
    /// Set a fan's hardware mode (auto/custom/turbo).
    fn set_fan_hw_mode(&mut self, fan: FanId, mode: HwFanMode) -> Result<(), BackendError>;
    /// Set fan duty percent (implies custom mode; caller sets mode too).
    fn set_fan_duty(&mut self, fan: FanId, duty_pct: u8) -> Result<(), BackendError>;
    /// Apply a platform profile by sysfs name; errors reflect firmware
    /// rejection (e.g. turbo → EIO on this model).
    fn set_platform_profile(&mut self, profile: &str) -> Result<(), BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_mode_enable_mapping() {
        assert_eq!(HwFanMode::Turbo.enable_value(), 0);
        assert_eq!(HwFanMode::Custom.enable_value(), 1);
        assert_eq!(HwFanMode::Auto.enable_value(), 2);
        assert_eq!(HwFanMode::from_enable_value(1), Some(HwFanMode::Custom));
        assert_eq!(HwFanMode::from_enable_value(7), None);
    }

    #[test]
    fn telemetry_temperature_selection() {
        let t = Telemetry {
            fans: vec![FanTelemetry {
                id: FanId::Cpu,
                rpm: Some(2400),
                duty_pct: Some(45),
                hw_mode: Some(HwFanMode::Custom),
            }],
            temps: vec![
                TempTelemetry {
                    label: "Package id 0".into(),
                    class: TempClass::Cpu,
                    temp_c: 62.0,
                },
                TempTelemetry {
                    label: "Core 2".into(),
                    class: TempClass::Cpu,
                    temp_c: 58.0,
                },
                TempTelemetry {
                    label: "acpitz".into(),
                    class: TempClass::Board,
                    temp_c: 51.0,
                },
                TempTelemetry {
                    label: "Composite".into(),
                    class: TempClass::Nvme,
                    temp_c: 67.0,
                },
            ],
            gpu: Some(GpuTelemetry {
                name: "RTX 4050".into(),
                temp_c: 47.0,
                power_w: 12.3,
            }),
            on_ac: Some(true),
            platform_profile: Some("balanced".into()),
            tick_ms: 1000,
        };
        assert_eq!(t.cpu_temp_c(), Some(62.0));
        assert_eq!(t.gpu_temp_c(), Some(47.0)); // NVML preferred
        assert_eq!(t.fan(FanId::Cpu).and_then(|f| f.rpm), Some(2400));
        assert_eq!(t.fan(FanId::Gpu), None);
    }

    #[test]
    fn telemetry_falls_back_to_board_zones() {
        let t = Telemetry {
            temps: vec![TempTelemetry {
                label: "acpitz".into(),
                class: TempClass::Board,
                temp_c: 54.0,
            }],
            ..Default::default()
        };
        assert_eq!(t.cpu_temp_c(), Some(54.0));
        assert_eq!(t.gpu_temp_c(), Some(54.0));
    }
}
