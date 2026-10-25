//! Capability probing for the acer-wmi driver (invoked from
//! [`crate::drivers::acer_wmi`]).
//!
//! Three states matter on an ANV15-51 (kernel 7.2, BIOS V1.60):
//!
//! | Level | Trigger | Meaning |
//! |---|---|---|
//! | `Full` | acer hwmon has `pwm1_enable` | telemetry + manual fan control |
//! | `MonitorProfiles` | acer hwmon exists, no `pwm1_enable` | telemetry + platform profiles only (`acer_wmi predator_v4=1`) |
//! | `None` | no hwmon | stock driver state; generic sensors still work |
//!
//! `Full` arrives with kernel ≥ 7.3 (upstream ANV15-51 quirk) or the
//! stopgap DKMS module from this repo.

use std::path::Path;

use thermite_core::{CapabilityLevel, CapabilityReport};

/// Platform profile choices from
/// `root/sys/firmware/acpi/platform_profile_choices`.
pub fn platform_profiles(root: &Path) -> Option<Vec<String>> {
    crate::sysfs::read_opt(&root.join("sys/firmware/acpi/platform_profile_choices"))
        .map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
}

/// Probe everything and produce a report with actionable notes.
pub fn probe(root: &Path) -> CapabilityReport {
    let mut report = CapabilityReport {
        platform_profiles: platform_profiles(root).unwrap_or_default(),
        ..CapabilityReport::default()
    };

    match super::drivers::acer_wmi::find_acer_hwmon(root) {
        Some(hwmon) => {
            report.driver = Some("acer-wmi".to_string());
            report.driver_detail = Some(hwmon.display().to_string());
            let fans = hwmon.join("fan1_input").exists();
            let pwm = hwmon.join("pwm1_enable").exists();
            if fans && pwm {
                report.level = CapabilityLevel::Full;
                report
                    .notes
                    .push("acer-wmi fan control: full (manual pwm available)".to_string());
            } else if fans {
                report.level = CapabilityLevel::MonitorProfiles;
                report.notes.push(
                    "acer-wmi fan telemetry available, but no manual pwm: load the kernel \
                     7.3+ ANV15-51 quirk (or the stopgap DKMS module) for fan control"
                        .to_string(),
                );
            } else {
                report.level = CapabilityLevel::None;
                report
                    .notes
                    .push("acer-wmi hwmon present but reports no sensors".to_string());
            }
        }
        None => {
            report.level = CapabilityLevel::None;
            report.notes.push(
                "acer-wmi hwmon not found: run `modprobe acer_wmi predator_v4=1` for \
                 monitoring+profiles, and kernel 7.3+/stopgap module for manual control"
                    .to_string(),
            );
        }
    }

    if report.platform_profiles.is_empty() {
        report
            .notes
            .push("platform_profile interface absent (comes with predator_v4/quirk)".to_string());
    } else {
        report.notes.push(format!(
            "platform profiles: {}",
            report.platform_profiles.join(", ")
        ));
    }
    report
}
