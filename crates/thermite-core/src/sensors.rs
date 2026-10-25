//! Sensor/control telemetry types and the hardware [`Backend`] trait.
//!
//! These types are shared by the daemon, the IPC protocol, the CLI and the
//! GUI, so they carry serde derives and stable field semantics. Nothing here
//! assumes a particular vendor, fan count, or kernel interface — drivers
//! enumerate their fans via [`Backend::fans`] and translate abstract intents
//! ([`HwFanMode`], duty percent) into whatever their hardware speaks.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Identifies a fan by driver-reported name (e.g. `"cpu"`, `"gpu"`, `"intake"`).
///
/// Names come from the active driver's [`FanDescriptor`]s, are used as config
/// keys (`[fans."cpu"]`), IPC identifiers, and display keys. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct FanId(Arc<str>);

impl FanId {
    /// Create a fan id from a name.
    pub fn new(name: &str) -> Self {
        Self(Arc::from(name))
    }

    /// The fan name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for FanId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for FanId {
    fn from(s: String) -> Self {
        Self(Arc::from(s.as_str()))
    }
}

impl std::fmt::Display for FanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which temperature input drives a fan, as declared by the driver.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "name")]
pub enum TempSource {
    /// Peak CPU-class temperature (coretemp etc., board zones as fallback).
    Cpu,
    /// Peak GPU temperature (NVML preferred, then GPU-class sensors).
    Gpu,
    /// A specific named sensor; the driver resolves the label at runtime.
    Named(String),
}

/// A fan reported by the active driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FanDescriptor {
    /// Stable fan name (config key / IPC id).
    pub id: FanId,
    /// Human-readable label for UI, e.g. `"CPU Fan"`.
    pub label: String,
    /// Which temperature input drives this fan.
    pub temp_source: TempSource,
}

/// Abstract fan-control intent. Drivers translate this into their hardware's
/// convention — e.g. the acer-wmi `pwmX_enable` mapping is *inverted* relative
/// to the usual hwmon convention (`0` = turbo, `1` = custom, `2` = auto); that
/// translation lives in the acer driver, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HwFanMode {
    /// Maximum cooling.
    Turbo,
    /// Duty from the last `set_fan_duty` write.
    Custom,
    /// Firmware automatic control.
    Auto,
}

/// One fan's current hardware state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FanTelemetry {
    /// Which fan.
    pub id: FanId,
    /// RPM if reported.
    pub rpm: Option<u32>,
    /// Commanded duty percent readback if present.
    pub duty_pct: Option<u8>,
    /// Hardware mode readback, when the driver can express it.
    pub hw_mode: Option<HwFanMode>,
}

/// Broad classification of a temperature sensor, used to pick control inputs
/// and dashboard grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TempClass {
    /// CPU package/cores.
    Cpu,
    /// GPU.
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
    /// zones).
    pub fn cpu_temp_c(&self) -> Option<f32> {
        self.temp_by_class(TempClass::Cpu)
            .or_else(|| self.board_max())
    }

    /// Peak GPU temperature (NVML preferred, then GPU-class sensors, then
    /// board zones).
    pub fn gpu_temp_c(&self) -> Option<f32> {
        self.gpu
            .as_ref()
            .map(|g| g.temp_c)
            .or_else(|| self.temp_by_class(TempClass::Gpu))
            .or_else(|| self.board_max())
    }

    /// Peak temperature across sensors of one class.
    pub fn temp_by_class(&self, class: TempClass) -> Option<f32> {
        self.temps
            .iter()
            .filter(|t| t.class == class)
            .map(|t| t.temp_c)
            .fold(None, |acc: Option<f32>, t| {
                Some(acc.map_or(t, |a| a.max(t)))
            })
    }

    /// Resolve a fan's driving temperature per its declared [`TempSource`].
    pub fn temp_for(&self, source: &TempSource) -> Option<f32> {
        match source {
            TempSource::Cpu => self.cpu_temp_c(),
            TempSource::Gpu => self.gpu_temp_c(),
            TempSource::Named(label) => self
                .temps
                .iter()
                .find(|t| &t.label == label)
                .map(|t| t.temp_c),
        }
    }

    fn board_max(&self) -> Option<f32> {
        self.temp_by_class(TempClass::Board)
    }

    /// Telemetry for one fan.
    pub fn fan(&self, id: &FanId) -> Option<&FanTelemetry> {
        self.fans.iter().find(|f| &f.id == id)
    }
}

/// How much the active driver currently offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityLevel {
    /// No fan/profile interface at all (telemetry may still be available from
    /// generic hwmon sensors).
    #[default]
    None,
    /// Fan RPM/temps readable and platform profiles settable, but no manual
    /// fan control.
    MonitorProfiles,
    /// Full control: telemetry + manual fan duty/mode control.
    Full,
}

/// What the backend found on this machine; powers `thermite doctor` and the
/// GUI's capability-aware UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilityReport {
    /// Coarse capability level.
    pub level: CapabilityLevel,
    /// Name of the active fan/thermal driver, when one matched.
    pub driver: Option<String>,
    /// Driver-specific detail line (e.g. the matched hwmon path).
    pub driver_detail: Option<String>,
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

/// Hardware access abstraction implemented by `thermite-backend` drivers
/// (sysfs/NVML/…) and by fakes in tests.
///
/// A driver is anything that can enumerate fans and read temperatures; control
/// methods may return `BackendError::Unsupported` when the hardware doesn't
/// allow them. The trait assumes nothing about *how* a driver talks to
/// hardware (hwmon sysfs, WMI, EC, …).
pub trait Backend: Send {
    /// Fans this driver controls/reports, in stable order.
    fn fans(&self) -> &[FanDescriptor];
    /// Probe current capabilities (cheap; cacheable by callers).
    fn capability(&self) -> CapabilityReport;
    /// Read a full telemetry snapshot.
    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError>;
    /// Set a fan's hardware mode (auto/custom/turbo).
    fn set_fan_hw_mode(&mut self, fan: &FanId, mode: HwFanMode) -> Result<(), BackendError>;
    /// Set fan duty percent (implies custom mode; caller sets mode too).
    fn set_fan_duty(&mut self, fan: &FanId, duty_pct: u8) -> Result<(), BackendError>;
    /// Apply a platform profile by driver-reported name; errors reflect
    /// firmware rejection.
    fn set_platform_profile(&mut self, profile: &str) -> Result<(), BackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_id_display_and_serde() {
        let id = FanId::new("intake");
        assert_eq!(id.to_string(), "intake");
        assert_eq!(
            serde_json::to_string(&FanId::new("cpu")).unwrap(),
            "\"cpu\""
        );
        assert_eq!(
            serde_json::from_str::<FanId>("\"gpu\"").unwrap(),
            FanId::new("gpu")
        );
    }

    #[test]
    fn telemetry_temperature_selection() {
        let t = Telemetry {
            fans: vec![FanTelemetry {
                id: FanId::new("cpu"),
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
        assert_eq!(
            t.temp_for(&TempSource::Named("Composite".into())),
            Some(67.0)
        );
        assert_eq!(t.fan(&FanId::new("cpu")).and_then(|f| f.rpm), Some(2400));
        assert!(t.fan(&FanId::new("gpu")).is_none());
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
        assert_eq!(t.temp_for(&TempSource::Cpu), Some(54.0));
    }
}
