//! Platform performance profiles.
//!
//! Profile names are opaque strings owned by the machine's firmware/driver
//! (e.g. `/sys/firmware/acpi/platform_profile` choices). Config accepts any
//! name; the daemon validates against the driver-reported available list at
//! runtime. [`PlatformProfile::KNOWN`] lists the common Linux
//! platform-profile names — on the Acer ANV15-51 (BIOS V1.60) turbo is
//! rejected with an I/O error, so clients must treat the *available* list
//! reported by the daemon as authoritative, not this list.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Common platform profile names, in quiet → loud order.
pub const KNOWN_PROFILES: &[&str] = &[
    "low-power",
    "quiet",
    "balanced",
    "balanced-performance",
    "performance",
    "turbo",
];

/// A platform profile name as reported by the firmware/driver.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct PlatformProfile(Arc<str>);

impl PlatformProfile {
    /// Wrap a profile name. Any non-empty name is accepted; validation
    /// against the machine's available list happens at runtime.
    pub fn new(name: &str) -> Self {
        Self(Arc::from(name))
    }

    /// The profile name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this name is one of the common Linux platform profiles.
    pub fn is_known(&self) -> bool {
        KNOWN_PROFILES.contains(&self.0.as_ref())
    }
}

impl From<&str> for PlatformProfile {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl std::fmt::Display for PlatformProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_name_accepted() {
        let p = PlatformProfile::new("custom-vendor-mode");
        assert_eq!(p.to_string(), "custom-vendor-mode");
        assert!(!p.is_known());
        assert!(PlatformProfile::new("quiet").is_known());
    }

    #[test]
    fn serde_plain_string() {
        assert_eq!(
            serde_json::to_string(&PlatformProfile::new("balanced-performance")).unwrap(),
            "\"balanced-performance\""
        );
    }
}
