//! NVIDIA GPU telemetry via NVML.
//!
//! The 615.x proprietary driver exposes no hwmon interface on this machine,
//! so NVML supplements the GPU hwmon sensor with name + power draw.
//! NVML calls can block for ~1 s and burn real CPU during driver
//! memory-pressure windows (NVRM errors observed periodically on some
//! driver builds) — running them on the control thread stalled fan control
//! and aligned with audio-buffer deadlines. All NVML work therefore lives
//! on a dedicated background thread at a relaxed cadence; `read()` serves
//! the latest value instantly and never blocks.

use std::time::Duration;

use thermite_core::GpuTelemetry;

/// How often the background thread refreshes NVML telemetry.
const READ_INTERVAL: Duration = Duration::from_secs(30);
/// Sleep after a failed read; doubles up to [`BACKOFF_MAX`].
const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(120);

/// Background-thread NVML source.
pub struct NvmlSource {
    /// Latest reading from the background thread.
    #[cfg(feature = "nvml")]
    slot: std::sync::Arc<std::sync::Mutex<Option<GpuTelemetry>>>,
    note: Option<String>,
}

#[cfg(feature = "nvml")]
type Slot = std::sync::Arc<std::sync::Mutex<Option<GpuTelemetry>>>;

#[cfg(feature = "nvml")]
static SLOT: std::sync::LazyLock<Slot> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(std::sync::Mutex::new(None)));

impl NvmlSource {
    /// Try to initialize NVML; failures degrade to an empty source with a
    /// diagnostic note (surfaced by `thermite doctor` and status). On
    /// success this spawns the background reader thread.
    pub fn new() -> Self {
        let mut note = None;
        #[cfg(feature = "nvml")]
        if let Err(e) = std::thread::Builder::new()
            .name("nvml-reader".into())
            .spawn(|| {
                let mut backoff = BACKOFF_START;
                loop {
                    match read_once() {
                        Ok(Some(t)) => {
                            *SLOT.lock().unwrap_or_else(|p| p.into_inner()) = Some(t);
                            backoff = BACKOFF_START;
                            std::thread::sleep(READ_INTERVAL);
                        }
                        // NVML absent/unusable: idle long; the capability
                        // note already says so.
                        Ok(None) => std::thread::sleep(Duration::from_secs(3600)),
                        Err(_) => {
                            std::thread::sleep(backoff);
                            backoff = (backoff * 2).min(BACKOFF_MAX);
                        }
                    }
                }
            })
        {
            note = Some(format!("NVML reader thread failed to spawn: {e}"));
        }
        Self {
            #[cfg(feature = "nvml")]
            slot: SLOT.clone(),
            note,
        }
    }

    /// Latest GPU telemetry; never blocks. `None` when NVML is unavailable
    /// or the background thread has not read yet.
    #[cfg(feature = "nvml")]
    pub fn read(&self) -> Option<GpuTelemetry> {
        self.slot.lock().ok().and_then(|g| g.clone())
    }

    #[cfg(not(feature = "nvml"))]
    pub fn read(&self) -> Option<GpuTelemetry> {
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

/// One NVML read cycle on the background thread. `Ok(None)` = NVML not
/// usable at all; `Err` = transient failure (back off and retry).
#[cfg(feature = "nvml")]
fn read_once() -> Result<Option<GpuTelemetry>, ()> {
    static NVML: std::sync::OnceLock<Result<nvml_wrapper::Nvml, String>> =
        std::sync::OnceLock::new();
    let nvml = match NVML
        .get_or_init(|| nvml_wrapper::Nvml::init().map_err(|e| format!("NVML init failed: {e}")))
    {
        Ok(n) => n,
        Err(_) => return Ok(None),
    };
    let device = nvml.device_by_index(0).map_err(|_| ())?;
    let name = device
        .name()
        .ok()
        .unwrap_or_else(|| "NVIDIA GPU".to_string());
    let temp_c = device
        .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
        .map_err(|_| ())? as f32;
    let power_w = device.power_usage().unwrap_or(0) as f32 / 1000.0;
    Ok(Some(GpuTelemetry {
        name,
        temp_c,
        power_w,
    }))
}

impl Default for NvmlSource {
    fn default() -> Self {
        Self::new()
    }
}
