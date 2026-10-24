//! acer-wmi gaming-interface backend: fans, pwm, platform profiles.
//!
//! Channel map (mainline driver, verified against the ANV15-51 quirk patch):
//! `fan1`/`pwm1` = CPU fan, `fan2`/`pwm2` = GPU fan; `temp1` = CPU,
//! `temp2` = GPU, `temp3` = external. `pwmX_enable`: 0 = turbo,
//! 1 = custom, 2 = auto — note the inverted convention documented on
//! [`thermite_core::HwFanMode`].

use std::io;
use std::path::{Path, PathBuf};

use thermite_core::{
    Backend, BackendError, CapabilityLevel, CapabilityReport, FanId, FanTelemetry, HwFanMode,
    Telemetry, TempClass, TempTelemetry,
};

/// sysfs-backed backend for the acer-wmi gaming interface.
pub struct AcerWmiBackend {
    root: PathBuf,
    hwmon: Option<PathBuf>,
    capability: CapabilityReport,
}

impl AcerWmiBackend {
    /// Probe the real machine.
    pub fn probe() -> Self {
        Self::with_root(PathBuf::from("/"))
    }

    /// Probe under an alternative filesystem root (tests).
    pub fn with_root(root: PathBuf) -> Self {
        let capability = crate::capability::probe(&root);
        let hwmon = crate::capability::find_acer_hwmon(&root);
        Self {
            root,
            hwmon,
            capability,
        }
    }

    /// Refresh cached capability (e.g. after loading a module).
    pub fn reprobe(&mut self) {
        self.capability = crate::capability::probe(&self.root);
        self.hwmon = crate::capability::find_acer_hwmon(&self.root);
    }

    fn fan_index(fan: FanId) -> usize {
        match fan {
            FanId::Cpu => 1,
            FanId::Gpu => 2,
        }
    }

    fn attr_file(&self, fan: FanId, attr: &str) -> Option<PathBuf> {
        self.hwmon
            .as_ref()
            .map(|h| h.join(format!("{attr}{}", Self::fan_index(fan))))
    }

    /// pwm value file — `pwmN` in the mainline driver; some kernels use
    /// `pwmN_input` for readback, so read paths try both.
    fn pwm_value(&self, fan: FanId) -> Option<PathBuf> {
        self.attr_file(fan, "pwm")
    }

    fn pwm_enable(&self, fan: FanId) -> Option<PathBuf> {
        self.attr_file(fan, "pwm")
            .map(|p| p.with_file_name(format!("pwm{}_enable", Self::fan_index(fan))))
    }
}

/// Map write failures to actionable error variants (EACCES → permission,
/// ENOENT → unsupported, EIO → firmware rejection).
fn map_write_err(e: io::Error, path: &Path) -> BackendError {
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            BackendError::Permission(format!("{}: {e}", path.display()))
        }
        io::ErrorKind::NotFound => {
            BackendError::Unsupported(format!("{}: attribute missing", path.display()))
        }
        _ if e.raw_os_error() == Some(libc_errno_eio()) => {
            BackendError::Invalid(format!("{}: firmware rejected: {e}", path.display()))
        }
        _ => BackendError::Io(e),
    }
}

fn libc_errno_eio() -> i32 {
    5 // EIO everywhere Linux runs
}

fn pwm_to_pct(pwm: u32) -> u8 {
    ((pwm.min(255) * 100 + 127) / 255) as u8
}

fn pct_to_pwm(pct: u8) -> u32 {
    (u32::from(pct.min(100)) * 255 + 50) / 100
}

impl Backend for AcerWmiBackend {
    fn capability(&self) -> CapabilityReport {
        self.capability.clone()
    }

    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError> {
        let mut telemetry = Telemetry::default();

        if let Some(hwmon) = &self.hwmon {
            for fan in FanId::all() {
                let n = Self::fan_index(fan);
                let rpm = crate::sysfs::parse_num::<u32>(&hwmon.join(format!("fan{n}_input")));
                let pwm =
                    crate::sysfs::parse_num::<u32>(&hwmon.join(format!("pwm{n}"))).or_else(|| {
                        crate::sysfs::parse_num::<u32>(&hwmon.join(format!("pwm{n}_input")))
                    });
                let hw_mode = crate::sysfs::parse_num::<u8>(&hwmon.join(format!("pwm{n}_enable")))
                    .and_then(HwFanMode::from_enable_value);
                telemetry.fans.push(FanTelemetry {
                    id: fan,
                    rpm,
                    duty_pct: pwm.map(pwm_to_pct),
                    hw_mode,
                });
            }

            for (n, label, class) in [
                (1usize, "CPU", TempClass::Cpu),
                (2, "GPU", TempClass::Gpu),
                (3, "External", TempClass::Other),
            ] {
                if let Some(milli) =
                    crate::sysfs::parse_num::<f32>(&hwmon.join(format!("temp{n}_input")))
                {
                    telemetry.temps.push(TempTelemetry {
                        label: format!("acer-wmi {label}"),
                        class,
                        temp_c: milli / 1000.0,
                    });
                }
            }
        }

        telemetry.platform_profile =
            crate::sysfs::read_opt(&self.root.join("sys/firmware/acpi/platform_profile"));
        Ok(telemetry)
    }

    fn set_fan_hw_mode(&mut self, fan: FanId, mode: HwFanMode) -> Result<(), BackendError> {
        let file = self.pwm_enable(fan).ok_or_else(|| {
            BackendError::Unsupported(format!(
                "no pwm control for {fan} fan (capability: {:?}); \
                 manual fan control needs kernel 7.3+ or the stopgap module",
                self.capability.level
            ))
        })?;
        if self.capability.level != CapabilityLevel::Full {
            return Err(BackendError::Unsupported(
                "manual fan control unavailable at this capability level".to_string(),
            ));
        }
        crate::sysfs::write_str(&file, &mode.enable_value().to_string())
            .map_err(|e| map_write_err(e, &file))
    }

    fn set_fan_duty(&mut self, fan: FanId, duty_pct: u8) -> Result<(), BackendError> {
        let file = self
            .pwm_value(fan)
            .ok_or_else(|| BackendError::Unsupported(format!("no pwm control for {fan} fan")))?;
        if self.capability.level != CapabilityLevel::Full {
            return Err(BackendError::Unsupported(
                "manual fan control unavailable at this capability level".to_string(),
            ));
        }
        if duty_pct > 100 {
            return Err(BackendError::Invalid(format!(
                "duty {duty_pct}% out of range 0-100"
            )));
        }
        crate::sysfs::write_str(&file, &pct_to_pwm(duty_pct).to_string())
            .map_err(|e| map_write_err(e, &file))
    }

    fn set_platform_profile(&mut self, profile: &str) -> Result<(), BackendError> {
        let file = self.root.join("sys/firmware/acpi/platform_profile");
        if !file.exists() {
            return Err(BackendError::Unsupported(
                "platform_profile interface absent; try `modprobe acer_wmi predator_v4=1`"
                    .to_string(),
            ));
        }
        if !self.capability.platform_profiles.is_empty()
            && !self
                .capability
                .platform_profiles
                .iter()
                .any(|p| p == profile)
        {
            return Err(BackendError::Invalid(format!(
                "profile \"{profile}\" not offered by firmware (available: {})",
                self.capability.platform_profiles.join(", ")
            )));
        }
        crate::sysfs::write_str(&file, profile).map_err(|e| map_write_err(e, &file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Full-control fixture: acer hwmon with pwm, plus coretemp/nvme and AC.
    fn full_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let h0 = root.join("sys/class/hwmon/hwmon0");
        write(&h0.join("name"), "acer_wmi");
        write(&h0.join("fan1_input"), "2400");
        write(&h0.join("fan2_input"), "2100");
        write(&h0.join("pwm1"), "128");
        write(&h0.join("pwm2"), "77");
        write(&h0.join("pwm1_enable"), "2");
        write(&h0.join("pwm2_enable"), "2");
        write(&h0.join("temp1_input"), "52000");
        write(&h0.join("temp2_input"), "47000");
        write(&h0.join("temp3_input"), "31000");

        let h1 = root.join("sys/class/hwmon/hwmon1");
        write(&h1.join("name"), "coretemp");
        write(&h1.join("temp1_input"), "53000");
        write(&h1.join("temp1_label"), "Package id 0");

        write(
            &root.join("sys/firmware/acpi/platform_profile_choices"),
            "low-power quiet balanced balanced-performance performance\n",
        );
        write(
            &root.join("sys/firmware/acpi/platform_profile"),
            "balanced\n",
        );

        write(&root.join("sys/class/power_supply/ACAD/type"), "Mains");
        write(&root.join("sys/class/power_supply/ACAD/online"), "1");
        (dir, root)
    }

    #[test]
    fn full_capability_detected() {
        let (_d, root) = full_fixture();
        let report = crate::capability::probe(&root);
        assert_eq!(report.level, CapabilityLevel::Full);
        assert_eq!(report.platform_profiles.len(), 5);
        assert!(report.acer_wmi_hwmon.is_some());
    }

    #[test]
    fn telemetry_reads_fans_temps_profile() {
        let (_d, root) = full_fixture();
        let mut b = AcerWmiBackend::with_root(root);
        let t = b.read_telemetry().unwrap();
        let cpu = t.fan(FanId::Cpu).unwrap();
        assert_eq!(cpu.rpm, Some(2400));
        assert_eq!(cpu.duty_pct, Some(50)); // 128/255
        assert_eq!(cpu.hw_mode, Some(HwFanMode::Auto));
        let gpu = t.fan(FanId::Gpu).unwrap();
        assert_eq!(gpu.duty_pct, Some(30)); // 77/255
        assert_eq!(
            t.temps,
            vec![
                TempTelemetry {
                    label: "acer-wmi CPU".into(),
                    class: TempClass::Cpu,
                    temp_c: 52.0
                },
                TempTelemetry {
                    label: "acer-wmi GPU".into(),
                    class: TempClass::Gpu,
                    temp_c: 47.0
                },
                TempTelemetry {
                    label: "acer-wmi External".into(),
                    class: TempClass::Other,
                    temp_c: 31.0
                },
            ]
        );
        assert_eq!(t.platform_profile.as_deref(), Some("balanced"));
        assert_eq!(t.cpu_temp_c(), Some(52.0));
        assert_eq!(t.gpu_temp_c(), Some(47.0));
    }

    #[test]
    fn duty_and_mode_writes() {
        let (_d, root) = full_fixture();
        let mut b = AcerWmiBackend::with_root(root);
        b.set_fan_duty(FanId::Cpu, 50).unwrap();
        let hwmon = b.hwmon.clone().unwrap();
        assert_eq!(
            fs::read_to_string(hwmon.join("pwm1")).unwrap().trim(),
            "128"
        );
        b.set_fan_hw_mode(FanId::Cpu, HwFanMode::Custom).unwrap();
        assert_eq!(
            fs::read_to_string(hwmon.join("pwm1_enable"))
                .unwrap()
                .trim(),
            "1"
        );
        // duty rounding: 33% -> 84 (33*255+50)/100
        b.set_fan_duty(FanId::Gpu, 33).unwrap();
        assert_eq!(fs::read_to_string(hwmon.join("pwm2")).unwrap().trim(), "84");
        assert!(matches!(
            b.set_fan_duty(FanId::Cpu, 101),
            Err(BackendError::Invalid(_))
        ));
    }

    #[test]
    fn profile_write_and_validation() {
        let (_d, root) = full_fixture();
        let mut b = AcerWmiBackend::with_root(root.clone());
        b.set_platform_profile("quiet").unwrap();
        assert_eq!(
            crate::sysfs::read_opt(&root.join("sys/firmware/acpi/platform_profile")).as_deref(),
            Some("quiet")
        );
        assert!(matches!(
            b.set_platform_profile("turbo-ludo"),
            Err(BackendError::Invalid(_))
        ));
    }

    #[test]
    fn monitor_only_fixture_degrades() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let h0 = root.join("sys/class/hwmon/hwmon0");
        write(&h0.join("name"), "acer_wmi");
        write(&h0.join("fan1_input"), "2000");
        write(&h0.join("temp1_input"), "50000");
        write(
            &root.join("sys/firmware/acpi/platform_profile_choices"),
            "quiet balanced\n",
        );
        write(
            &root.join("sys/firmware/acpi/platform_profile"),
            "balanced\n",
        );

        let mut b = AcerWmiBackend::with_root(root);
        assert_eq!(b.capability().level, CapabilityLevel::MonitorProfiles);
        let t = b.read_telemetry().unwrap();
        assert_eq!(t.fan(FanId::Cpu).unwrap().rpm, Some(2000));
        // Profile writes still allowed at MonitorProfiles (that's the point
        // of predator_v4=1), but fan writes are not.
        b.set_platform_profile("quiet").unwrap();
        assert!(matches!(
            b.set_fan_duty(FanId::Cpu, 50),
            Err(BackendError::Unsupported(_))
        ));
        assert!(matches!(
            b.set_fan_hw_mode(FanId::Cpu, HwFanMode::Custom),
            Err(BackendError::Unsupported(_))
        ));
    }

    #[test]
    fn absent_interface_reports_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = AcerWmiBackend::with_root(dir.path().to_path_buf());
        assert_eq!(b.capability().level, CapabilityLevel::None);
        let t = b.read_telemetry().unwrap();
        assert!(t.fans.is_empty());
        assert!(matches!(
            b.set_platform_profile("quiet"),
            Err(BackendError::Unsupported(_))
        ));
    }
}
