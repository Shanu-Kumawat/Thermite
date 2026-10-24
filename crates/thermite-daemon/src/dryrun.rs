//! Dry-run backend wrapper: exercises the full control loop without
//! touching hardware. Writes are counted and simulated in a shadow state so
//! telemetry readbacks stay consistent (otherwise the drift detector would
//! fire constantly against unwritten hardware).

use std::collections::BTreeMap;

use thermite_core::BackendError;
use thermite_core::sensors::{Backend, CapabilityReport, FanId, HwFanMode, Telemetry};

/// Wraps a backend and suppresses its writes.
pub struct DryRunBackend {
    inner: Box<dyn Backend>,
    shadow: BTreeMap<FanId, (HwFanMode, u8)>,
    shadow_profile: Option<String>,
    suppressed: u64,
}

impl DryRunBackend {
    /// Wrap a backend.
    pub fn new(inner: Box<dyn Backend>) -> Self {
        Self {
            inner,
            shadow: BTreeMap::new(),
            shadow_profile: None,
            suppressed: 0,
        }
    }

    /// Number of suppressed writes so far.
    pub fn suppressed_writes(&self) -> u64 {
        self.suppressed
    }
}

impl Backend for DryRunBackend {
    fn capability(&self) -> CapabilityReport {
        self.inner.capability()
    }

    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError> {
        let mut telemetry = self.inner.read_telemetry()?;
        // Simulate our writes in the readback so drift detection sees a
        // coherent world.
        for fan in FanId::all() {
            if let Some((mode, duty)) = self.shadow.get(&fan)
                && let Some(slot) = telemetry.fans.iter_mut().find(|f| f.id == fan)
            {
                slot.hw_mode = Some(*mode);
                slot.duty_pct = Some(*duty);
            }
        }
        if let Some(profile) = &self.shadow_profile {
            telemetry.platform_profile = Some(profile.clone());
        }
        Ok(telemetry)
    }

    fn set_fan_hw_mode(&mut self, fan: FanId, mode: HwFanMode) -> Result<(), BackendError> {
        self.suppressed += 1;
        let entry = self.shadow.entry(fan).or_insert((mode, 0));
        entry.0 = mode;
        tracing::info!("[dry-run] set {fan} fan mode -> {mode:?}");
        Ok(())
    }

    fn set_fan_duty(&mut self, fan: FanId, duty_pct: u8) -> Result<(), BackendError> {
        self.suppressed += 1;
        let entry = self
            .shadow
            .entry(fan)
            .or_insert((HwFanMode::Custom, duty_pct));
        entry.1 = duty_pct;
        tracing::info!("[dry-run] set {fan} fan duty -> {duty_pct}%");
        Ok(())
    }

    fn set_platform_profile(&mut self, profile: &str) -> Result<(), BackendError> {
        self.suppressed += 1;
        tracing::info!("[dry-run] set platform profile -> {profile}");
        self.shadow_profile = Some(profile.to_string());
        Ok(())
    }
}
