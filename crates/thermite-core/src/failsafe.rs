//! Thermal failsafe policy.
//!
//! A small, explicitly tested state machine that sits *above* curve
//! evaluation:
//!
//! - `Normal` — curves/hysteresis drive duty, with conservative floors.
//! - `ForceFull` — a warn threshold was crossed: command 100% duty.
//! - `ReleasedAuto` — a crit threshold was crossed: **give control back to
//!   the firmware** (`pwmX_enable = 2`). The EC's own controller is the most
//!   reliable cooling authority we have; the daemon latches in this phase
//!   until temperatures have recovered by `recovery_c`.
//!
//! Floors are applied even in `Normal` so that no curve, however miswritten,
//! can command 0% duty on a hot machine.

use serde::{Deserialize, Serialize};

/// Failsafe thresholds in °C / percent. Defaults are tuned for the
/// i5-13420H + RTX 4050 in the ANV15-51.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FailsafeThresholds {
    /// CPU warn threshold: force both fans to full duty.
    pub cpu_warn_c: f32,
    /// CPU crit threshold: release control to firmware auto mode.
    pub cpu_crit_c: f32,
    /// GPU warn threshold.
    pub gpu_warn_c: f32,
    /// GPU crit threshold.
    pub gpu_crit_c: f32,
    /// While latched in `ReleasedAuto`, temperatures must fall this far below
    /// crit before the daemon may resume manual control.
    pub recovery_c: f32,
    /// At or above this temperature the duty floor applies.
    pub duty_floor_temp_c: f32,
    /// Minimum duty (percent) forced when at/above `duty_floor_temp_c`.
    pub duty_floor_pct: u8,
    /// Never command 0% duty above this temperature.
    pub zero_duty_max_c: f32,
    /// Duty forced in place of 0 above `zero_duty_max_c`.
    pub min_spin_pct: u8,
}

impl Default for FailsafeThresholds {
    fn default() -> Self {
        Self {
            cpu_warn_c: 87.0,
            cpu_crit_c: 92.0,
            gpu_warn_c: 82.0,
            gpu_crit_c: 87.0,
            recovery_c: 5.0,
            duty_floor_temp_c: 60.0,
            duty_floor_pct: 30,
            zero_duty_max_c: 45.0,
            min_spin_pct: 15,
        }
    }
}

/// Coarse phase the daemon is in; surfaced over IPC to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FailsafePhase {
    /// Curves drive duty (floors applied).
    #[default]
    Normal,
    /// Warn threshold crossed: commanding full duty.
    ForceFull,
    /// Crit threshold crossed: firmware auto mode owns the fans; latched
    /// until recovery.
    ReleasedAuto,
}

/// What the daemon should do this tick, as decided by [`FailsafeEngine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PhaseDirective {
    /// Manual control allowed; apply floors to the curve duty.
    Normal,
    /// Command 100% duty on every fan under manual control.
    ForceFull,
    /// Write `pwm_enable = auto` and write no duties this tick.
    ReleaseToAuto,
}

/// Latching state machine over warn/crit thresholds.
#[derive(Debug, Clone)]
pub struct FailsafeEngine {
    thresholds: FailsafeThresholds,
    phase: FailsafePhase,
}

impl Default for FailsafeEngine {
    fn default() -> Self {
        Self::new(FailsafeThresholds::default())
    }
}

impl FailsafeEngine {
    /// New engine starting in `Normal`.
    pub fn new(thresholds: FailsafeThresholds) -> Self {
        Self {
            thresholds,
            phase: FailsafePhase::Normal,
        }
    }

    /// Current phase (published in status).
    pub fn phase(&self) -> FailsafePhase {
        self.phase
    }

    /// Thresholds in use (published in status).
    pub fn thresholds(&self) -> &FailsafeThresholds {
        &self.thresholds
    }

    /// Update thresholds (e.g. on config reload). Never changes phase.
    pub fn set_thresholds(&mut self, t: FailsafeThresholds) {
        self.thresholds = t;
    }

    /// Advance the state machine one tick with the current peak CPU/GPU
    /// temperatures and return the directive for this tick.
    pub fn step(&mut self, cpu_temp_c: f32, gpu_temp_c: f32) -> PhaseDirective {
        let t = &self.thresholds;
        let crit = cpu_temp_c >= t.cpu_crit_c || gpu_temp_c >= t.gpu_crit_c;
        match self.phase {
            FailsafePhase::ReleasedAuto => {
                let cpu_ok = cpu_temp_c < t.cpu_crit_c - t.recovery_c;
                let gpu_ok = gpu_temp_c < t.gpu_crit_c - t.recovery_c;
                if crit || !(cpu_ok && gpu_ok) {
                    PhaseDirective::ReleaseToAuto
                } else {
                    self.phase = FailsafePhase::Normal;
                    PhaseDirective::Normal
                }
            }
            FailsafePhase::ForceFull => {
                if crit {
                    self.phase = FailsafePhase::ReleasedAuto;
                    return PhaseDirective::ReleaseToAuto;
                }
                let warn = cpu_temp_c >= t.cpu_warn_c || gpu_temp_c >= t.gpu_warn_c;
                if warn {
                    PhaseDirective::ForceFull
                } else {
                    self.phase = FailsafePhase::Normal;
                    PhaseDirective::Normal
                }
            }
            FailsafePhase::Normal => {
                if crit {
                    self.phase = FailsafePhase::ReleasedAuto;
                    PhaseDirective::ReleaseToAuto
                } else if cpu_temp_c >= t.cpu_warn_c || gpu_temp_c >= t.gpu_warn_c {
                    self.phase = FailsafePhase::ForceFull;
                    PhaseDirective::ForceFull
                } else {
                    PhaseDirective::Normal
                }
            }
        }
    }

    /// Conservative floors applied to every commanded duty in `Normal`.
    /// `driving_temp_c` is the temperature that drives this particular fan.
    pub fn apply_floors(&self, duty_pct: u8, driving_temp_c: f32) -> u8 {
        let t = &self.thresholds;
        let mut duty = duty_pct;
        if driving_temp_c >= t.duty_floor_temp_c {
            duty = duty.max(t.duty_floor_pct);
        }
        if duty == 0 && driving_temp_c > t.zero_duty_max_c {
            duty = t.min_spin_pct;
        }
        duty.min(100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_below_thresholds_directs_normal() {
        let mut e = FailsafeEngine::default();
        assert_eq!(e.step(50.0, 45.0), PhaseDirective::Normal);
        assert_eq!(e.phase(), FailsafePhase::Normal);
    }

    #[test]
    fn warn_crossing_forces_full_then_recovers() {
        let mut e = FailsafeEngine::default();
        assert_eq!(e.step(88.0, 50.0), PhaseDirective::ForceFull);
        assert_eq!(e.step(88.0, 50.0), PhaseDirective::ForceFull);
        assert_eq!(e.step(80.0, 50.0), PhaseDirective::Normal); // below warn -> back to normal
    }

    #[test]
    fn gpu_warn_also_forces_full() {
        let mut e = FailsafeEngine::default();
        assert_eq!(e.step(50.0, 83.0), PhaseDirective::ForceFull);
    }

    #[test]
    fn crit_releases_to_auto_and_latches() {
        let mut e = FailsafeEngine::default();
        assert_eq!(e.step(93.0, 50.0), PhaseDirective::ReleaseToAuto);
        // Still latched just under crit (recovery_c = 5)
        assert_eq!(e.step(90.0, 50.0), PhaseDirective::ReleaseToAuto);
        // Recovers once below crit - 5
        assert_eq!(e.step(86.0, 50.0), PhaseDirective::Normal);
    }

    #[test]
    fn force_full_escalates_to_crit_release() {
        let mut e = FailsafeEngine::default();
        assert_eq!(e.step(88.0, 50.0), PhaseDirective::ForceFull);
        assert_eq!(e.step(93.0, 50.0), PhaseDirective::ReleaseToAuto);
    }

    #[test]
    fn released_auto_stays_while_gpu_hot() {
        let mut e = FailsafeEngine::default();
        assert_eq!(e.step(50.0, 88.0), PhaseDirective::ReleaseToAuto);
        assert_eq!(e.step(50.0, 84.0), PhaseDirective::ReleaseToAuto); // gpu not recovered
        assert_eq!(e.step(50.0, 81.0), PhaseDirective::Normal);
    }

    #[test]
    fn floors_protect_hot_chips() {
        let e = FailsafeEngine::default();
        assert_eq!(e.apply_floors(10, 70.0), 30); // floor above 60°C
        assert_eq!(e.apply_floors(40, 70.0), 40); // above floor already
        assert_eq!(e.apply_floors(0, 50.0), 15); // no zero duty above 45°C
        assert_eq!(e.apply_floors(0, 40.0), 0); // zero allowed when cold
        assert_eq!(e.apply_floors(20, 59.9), 20); // below floor temp
    }
}
