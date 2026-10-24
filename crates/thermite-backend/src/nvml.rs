//! NVIDIA GPU telemetry via NVML.
//!
//! The 615.x proprietary driver exposes no hwmon interface on this machine,
//! so NVML is the only GPU temperature source when the acer-wmi gaming
//! hwmon is absent. Absence of the library at runtime is not an error — the
//! source simply reports `None`.

use thermite_core::GpuTelemetry;

/// Lazily-initialized NVML handle; `None` inside when unavailable.
pub struct NvmlSource {
    #[cfg(feature = "nvml")]
    nvml: Option<nvml_wrapper::Nvml>,
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
                note = Some(format!(
                    "NVML unavailable ({e}); GPU temp via acer-wmi hwmon only"
                ));
                None
            }
        };
        Self {
            #[cfg(feature = "nvml")]
            nvml,
            note,
        }
    }

    /// Read GPU telemetry; `None` when unavailable or on a transient error
    /// (logged once, not fatal).
    #[cfg(feature = "nvml")]
    pub fn read(&mut self) -> Option<GpuTelemetry> {
        let nvml = self.nvml.as_ref()?;
        let device = nvml.device_by_index(0).ok()?;
        let name = device
            .name()
            .ok()
            .unwrap_or_else(|| "NVIDIA GPU".to_string());
        let temp_c = device
            .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
            .ok()? as f32;
        let power_w = device
            .power_usage()
            .map(|mw| mw as f32 / 1000.0)
            .unwrap_or(0.0);
        Some(GpuTelemetry {
            name,
            temp_c,
            power_w,
        })
    }

    #[cfg(not(feature = "nvml"))]
    pub fn read(&mut self) -> Option<GpuTelemetry> {
        None
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
