//! Platform performance profiles.
//!
//! The ANV15-51 exposes platform profiles through
//! `/sys/firmware/acpi/platform_profile` once the acer-wmi gaming interface is
//! active (kernel ≥ 7.3 with the ANV15-51 quirk, the stopgap DKMS module, or
//! `acer_wmi predator_v4=1`). Known firmware behavior on BIOS V1.60: turbo is
//! rejected with an I/O error, so it is offered but must fail gracefully —
//! clients treat the *available* list reported by the daemon as authoritative,
//! not this enum.

use serde::{Deserialize, Serialize};

/// Platform profile names understood by the firmware, in
/// quiet → loud order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformProfile {
    /// Lowest power ("low-power" in sysfs).
    LowPower,
    /// Quiet bias.
    Quiet,
    /// Balanced (factory default).
    #[default]
    Balanced,
    /// Balanced-performance (between balanced and performance).
    BalancedPerformance,
    /// Full performance.
    Performance,
    /// Turbo — rejected by ANV15-51 firmware (kept for completeness).
    Turbo,
}

impl PlatformProfile {
    /// sysfs string for this profile.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LowPower => "low-power",
            Self::Quiet => "quiet",
            Self::Balanced => "balanced",
            Self::BalancedPerformance => "balanced-performance",
            Self::Performance => "performance",
            Self::Turbo => "turbo",
        }
    }

    /// Parse a sysfs profile string, tolerating unknown values as `None`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "low-power" => Self::LowPower,
            "quiet" => Self::Quiet,
            "balanced" => Self::Balanced,
            "balanced-performance" => Self::BalancedPerformance,
            "performance" => Self::Performance,
            "turbo" => Self::Turbo,
            _ => return None,
        })
    }

    /// Every profile we know about.
    pub fn all() -> [PlatformProfile; 6] {
        [
            Self::LowPower,
            Self::Quiet,
            Self::Balanced,
            Self::BalancedPerformance,
            Self::Performance,
            Self::Turbo,
        ]
    }
}

impl std::fmt::Display for PlatformProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_sysfs_names() {
        for p in PlatformProfile::all() {
            assert_eq!(PlatformProfile::parse(p.as_str()), Some(p));
            assert_eq!(p.to_string(), p.as_str());
        }
    }

    #[test]
    fn unknown_names_are_none() {
        assert_eq!(PlatformProfile::parse("custom"), None);
        assert_eq!(PlatformProfile::parse(""), None);
    }

    #[test]
    fn serde_kebab_case() {
        assert_eq!(
            serde_json::to_string(&PlatformProfile::BalancedPerformance).unwrap(),
            "\"balanced-performance\""
        );
    }
}
