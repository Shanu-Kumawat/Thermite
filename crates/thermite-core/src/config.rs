//! Daemon configuration (`/etc/thermite/thermite.toml`).
//!
//! The file is the *policy* source: curves, thresholds, intervals, default fan
//! modes. Mutable runtime selections (which mode is active right now, last
//! chosen profile) persist separately in the daemon's state file; on startup
//! the daemon overlays state on top of these defaults.
//!
//! Parsing is strict (`deny_unknown_fields`) so typos fail loudly instead of
//! silently changing thermal behavior.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::curve::{CurveDef, CurvePoint};
use crate::failsafe::FailsafeThresholds;
use crate::profile::PlatformProfile;

/// Problems loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Filesystem error reading the config.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// TOML syntax/type error.
    #[error("parse: {0}")]
    Parse(#[from] toml::de::Error),
    /// TOML serialization error.
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// Semantic validation failure (one entry per problem).
    #[error("invalid configuration:\n  - {}\n", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

/// Intervals and behavior knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeneralConfig {
    /// Control loop period in milliseconds (200-10000).
    pub poll_interval_ms: u64,
    /// Re-assert fan mode + duty at least this often even if unchanged
    /// (defeats firmware reverting manual settings; observed ~10s reverts on
    /// sibling models). 250-10000.
    pub reassert_interval_ms: u64,
    /// When on battery, a fan set to curve `X` uses curve `X{suffix}` if it
    /// exists (e.g. `balanced-battery`). `None` disables the mechanism.
    pub battery_curve_suffix: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            reassert_interval_ms: 2000,
            battery_curve_suffix: Some("-battery".to_string()),
        }
    }
}

/// Hysteresis tuning (see [`crate::hysteresis`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HysteresisConfig {
    /// Temperature drop (°C) required before a duty downshift is considered.
    pub drop_c: f32,
    /// Consecutive ticks a lower duty must hold before it is committed.
    pub hold_ticks: u32,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            drop_c: 2.0,
            hold_ticks: 5,
        }
    }
}

/// How one fan is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FanModeKind {
    /// Firmware controls the fan (`pwm_enable = auto`).
    #[default]
    Auto,
    /// Fixed duty from `manual_duty_pct`.
    Manual,
    /// Named curve from the `curves` table.
    Curve,
}

/// Per-fan settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FanConfig {
    /// Control mode for this fan.
    pub mode: FanModeKind,
    /// Curve name (required when `mode = "curve"`).
    pub curve: Option<String>,
    /// Duty percent (required when `mode = "manual"`).
    pub manual_duty_pct: Option<u8>,
}

impl Default for FanConfig {
    fn default() -> Self {
        Self {
            mode: FanModeKind::Auto,
            curve: None,
            manual_duty_pct: None,
        }
    }
}

/// Both fans, keyed by hardware role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FansConfig {
    /// CPU fan settings.
    pub cpu: FanConfig,
    /// GPU fan settings.
    pub gpu: FanConfig,
}

/// Platform (performance) profile settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformConfig {
    /// Profile the daemon keeps applied; `None` leaves the firmware choice
    /// untouched.
    pub active: Option<PlatformProfile>,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            active: Some(PlatformProfile::Balanced),
        }
    }
}

/// Complete daemon configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Intervals and behavior knobs.
    pub general: GeneralConfig,
    /// Thermal failsafe thresholds.
    pub failsafe: FailsafeThresholds,
    /// Hysteresis tuning.
    pub hysteresis: HysteresisConfig,
    /// Per-fan control mode.
    pub fans: FansConfig,
    /// Platform profile policy.
    pub platform: PlatformConfig,
    /// Named fan curves (`[curves.<name>]`).
    pub curves: BTreeMap<String, CurveDef>,
}

impl Config {
    /// Parse and validate from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load and validate from a file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path)?;
        Self::from_toml_str(&s)
    }

    /// Serialize back to TOML (used when the daemon persists GUI edits).
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string(self)?)
    }

    /// Semantic validation. Collects *all* problems, not just the first.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut problems = Vec::new();

        let g = &self.general;
        if !(200..=10_000).contains(&g.poll_interval_ms) {
            problems.push(format!(
                "general.poll_interval_ms = {} outside 200-10000",
                g.poll_interval_ms
            ));
        }
        if !(250..=10_000).contains(&g.reassert_interval_ms) {
            problems.push(format!(
                "general.reassert_interval_ms = {} outside 250-10000",
                g.reassert_interval_ms
            ));
        }
        if g.reassert_interval_ms < g.poll_interval_ms {
            problems.push(format!(
                "general.reassert_interval_ms ({}) < poll_interval_ms ({}); re-assert would never fire",
                g.reassert_interval_ms, g.poll_interval_ms
            ));
        }

        let f = &self.failsafe;
        if f.cpu_warn_c >= f.cpu_crit_c {
            problems.push(format!(
                "failsafe.cpu_warn_c ({}) must be below cpu_crit_c ({})",
                f.cpu_warn_c, f.cpu_crit_c
            ));
        }
        if f.gpu_warn_c >= f.gpu_crit_c {
            problems.push(format!(
                "failsafe.gpu_warn_c ({}) must be below gpu_crit_c ({})",
                f.gpu_warn_c, f.gpu_crit_c
            ));
        }
        if f.recovery_c <= 0.0 {
            problems.push(format!(
                "failsafe.recovery_c ({}) must be > 0",
                f.recovery_c
            ));
        }
        if f.duty_floor_pct > 100 || f.min_spin_pct > 100 {
            problems.push("failsafe duty floors must be 0-100".to_string());
        }
        if f.duty_floor_temp_c >= f.cpu_crit_c || f.duty_floor_temp_c >= f.gpu_crit_c {
            problems.push(format!(
                "failsafe.duty_floor_temp_c ({}) should be well below crit thresholds",
                f.duty_floor_temp_c
            ));
        }

        if self.hysteresis.hold_ticks == 0 {
            problems.push("hysteresis.hold_ticks must be >= 1".to_string());
        }
        if self.hysteresis.drop_c < 0.0 {
            problems.push("hysteresis.drop_c must be >= 0".to_string());
        }

        for (name, curve) in &self.curves {
            if let Err(e) = curve.validate() {
                problems.push(format!("curves.{name}: {e}"));
            }
        }

        for (fan, cfg) in [("cpu", &self.fans.cpu), ("gpu", &self.fans.gpu)] {
            match cfg.mode {
                FanModeKind::Auto => {}
                FanModeKind::Manual => match cfg.manual_duty_pct {
                    Some(d) if d <= 100 => {}
                    Some(d) => {
                        problems.push(format!("fans.{fan}.manual_duty_pct = {d} outside 0-100"))
                    }
                    None => problems.push(format!(
                        "fans.{fan}.mode = \"manual\" requires manual_duty_pct"
                    )),
                },
                FanModeKind::Curve => match &cfg.curve {
                    Some(name) if self.curves.contains_key(name) => {}
                    Some(name) => problems.push(format!(
                        "fans.{fan}: curve \"{name}\" is not defined in [curves]"
                    )),
                    None => problems.push(format!(
                        "fans.{fan}.mode = \"curve\" requires curve = <name>"
                    )),
                },
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }

    /// Resolve the curve a fan should follow: on battery, prefer
    /// `{name}{battery_curve_suffix}` when it exists.
    pub fn effective_curve(&self, requested: &str, on_battery: bool) -> Option<&CurveDef> {
        if on_battery
            && let Some(suffix) = &self.general.battery_curve_suffix
            && let Some(c) = self.curves.get(&format!("{requested}{suffix}"))
        {
            return Some(c);
        }
        self.curves.get(requested)
    }
}

impl Config {
    /// Sensible default configuration with three curves.
    pub fn with_default_curves() -> Self {
        let mk = |pts: &[(f32, u8)]| CurveDef {
            points: pts.iter().map(|&(t, d)| CurvePoint(t, d)).collect(),
        };
        let mut curves = BTreeMap::new();
        curves.insert(
            "quiet".to_string(),
            mk(&[(45.0, 25), (60.0, 35), (75.0, 55), (90.0, 100)]),
        );
        curves.insert(
            "balanced".to_string(),
            mk(&[
                (40.0, 30),
                (55.0, 40),
                (65.0, 50),
                (75.0, 65),
                (85.0, 85),
                (95.0, 100),
            ]),
        );
        curves.insert(
            "performance".to_string(),
            mk(&[(40.0, 40), (55.0, 55), (65.0, 70), (75.0, 85), (85.0, 100)]),
        );
        Self {
            curves,
            ..Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        Config::default().validate().unwrap();
        Config::with_default_curves().validate().unwrap();
    }

    #[test]
    fn full_toml_roundtrip() {
        let toml_src = r#"
[general]
poll_interval_ms = 500
reassert_interval_ms = 1500
battery_curve_suffix = "-ac-battery-nothing"

[failsafe]
cpu_warn_c = 85.0
cpu_crit_c = 90.0

[hysteresis]
drop_c = 3.0
hold_ticks = 4

[fans.cpu]
mode = "curve"
curve = "balanced"

[fans.gpu]
mode = "manual"
manual_duty_pct = 55

[platform]
active = "quiet"

[curves.balanced]
points = [[40, 30], [60, 50], [85, 100]]

[curves.balanced-ac-battery-nothing]
points = [[50, 25], [70, 45]]
"#;
        let cfg = Config::from_toml_str(toml_src).unwrap();
        assert_eq!(cfg.general.poll_interval_ms, 500);
        assert_eq!(cfg.fans.gpu.manual_duty_pct, Some(55));
        assert_eq!(cfg.platform.active, Some(PlatformProfile::Quiet));
        cfg.validate().unwrap();

        let out = cfg.to_toml_string().unwrap();
        let back = Config::from_toml_str(&out).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn unknown_fields_rejected() {
        let err = Config::from_toml_str("[general]\npoll_interavl_ms = 1000\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[test]
    fn collects_all_problems() {
        let cfg = Config::from_toml_str(
            r#"
[fans.cpu]
mode = "curve"
curve = "missing"

[fans.gpu]
mode = "manual"

[failsafe]
cpu_warn_c = 95.0
cpu_crit_c = 90.0
"#,
        )
        .unwrap_err();
        let msg = format!("{cfg}");
        assert!(msg.contains("missing"), "{msg}");
        assert!(msg.contains("manual_duty_pct"), "{msg}");
        assert!(msg.contains("cpu_warn_c"), "{msg}");
    }

    #[test]
    fn battery_suffix_prefers_battery_curve() {
        let cfg = Config::from_toml_str(
            r#"
[curves.quiet]
points = [[45, 30]]

[curves.quiet-battery]
points = [[45, 20]]
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.effective_curve("quiet", true).unwrap().duty_at(45.0),
            20
        );
        assert_eq!(
            cfg.effective_curve("quiet", false).unwrap().duty_at(45.0),
            30
        );
        // Falls back to base when no battery variant exists
        assert!(cfg.effective_curve("nonexistent", true).is_none());
    }

    #[test]
    fn partial_config_gets_defaults() {
        let cfg = Config::from_toml_str("[fans.cpu]\nmode = \"auto\"\n").unwrap();
        assert_eq!(cfg.general.poll_interval_ms, 1000);
        assert_eq!(cfg.platform.active, Some(PlatformProfile::Balanced));
    }
}
