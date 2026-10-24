//! Generic hwmon temperature scanning.
//!
//! Enumerates every hwmon device under `root/sys/class/hwmon` except the
//! acer-wmi one (which [`crate::acer_wmi`] reads with proper CPU/GPU
//! classification) and classifies sensors by driver name.

use std::path::Path;

use thermite_core::{BackendError, TempClass, TempTelemetry};

/// Scan the real machine.
pub fn scan_temps() -> Result<Vec<TempTelemetry>, BackendError> {
    scan_temps_under(Path::new("/"))
}

/// Scan under an alternative filesystem root (tests).
pub fn scan_temps_under(root: &Path) -> Result<Vec<TempTelemetry>, BackendError> {
    let mut out = Vec::new();
    for dir in crate::sysfs::hwmon_dirs(root) {
        let Some(name) = crate::sysfs::hwmon_name(&dir) else {
            continue;
        };
        if name == crate::capability::ACER_WMI_HWMON_NAME {
            continue; // classified separately by AcerWmiBackend
        }
        let class = classify(&name);
        // Numeric sort of tempN_input indices, not lexicographic.
        let mut indices: Vec<u32> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let file = e.file_name();
                let file = file.to_string_lossy();
                let rest = file.strip_prefix("temp")?;
                let n = rest.strip_suffix("_input")?;
                n.parse().ok()
            })
            .collect();
        indices.sort_unstable();
        for n in indices {
            let milli = crate::sysfs::parse_num::<f32>(&dir.join(format!("temp{n}_input")));
            if let Some(milli) = milli {
                let label = crate::sysfs::read_opt(&dir.join(format!("temp{n}_label")))
                    .unwrap_or_else(|| format!("{name} temp{n}"));
                out.push(TempTelemetry {
                    label,
                    class,
                    temp_c: milli / 1000.0,
                });
            }
        }
    }
    Ok(out)
}

/// Map a hwmon driver name to a sensor class.
fn classify(name: &str) -> TempClass {
    match name {
        "coretemp" | "k10temp" | "zenpower" => TempClass::Cpu,
        "nvme" => TempClass::Nvme,
        "acpitz" => TempClass::Board,
        "iwlwifi" => TempClass::Wifi,
        "nvidia" | "amdgpu" | "radeon" => TempClass::Gpu,
        _ => TempClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scans_and_classifies_and_excludes_acer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mk = |h: &str, name: &str| {
            let d = root.join(format!("sys/class/hwmon/{h}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("name"), name).unwrap();
            d
        };
        let coretemp = mk("hwmon0", "coretemp");
        fs::write(coretemp.join("temp2_input"), "53000").unwrap();
        fs::write(coretemp.join("temp2_label"), "Package id 0").unwrap();
        fs::write(coretemp.join("temp10_input"), "48000").unwrap(); // numeric sort check
        let nvme = mk("hwmon1", "nvme");
        fs::write(nvme.join("temp1_input"), "67854").unwrap();
        let acpitz = mk("hwmon2", "acpitz");
        fs::write(acpitz.join("temp1_input"), "51000").unwrap();
        let acer = mk("hwmon3", "acer_wmi");
        fs::write(acer.join("temp1_input"), "99000").unwrap(); // must be excluded

        let temps = scan_temps_under(root).unwrap();
        let labels: Vec<&str> = temps.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Package id 0",
                "coretemp temp10",
                "nvme temp1",
                "acpitz temp1"
            ]
        );
        assert!(temps.iter().all(|t| t.temp_c < 70.0), "acer excluded");
    }
}
