//! AC/battery detection from `/sys/class/power_supply`.

use std::path::Path;

/// `true` on AC power, `false` on battery, `None` if no mains supply is
/// exposed. Scans the real machine.
pub fn on_ac() -> Option<bool> {
    on_ac_under(Path::new("/"))
}

/// Same, under an alternative filesystem root (tests).
pub fn on_ac_under(root: &Path) -> Option<bool> {
    let dir = root.join("sys/class/power_supply");
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let supply = entry.path();
        if crate::sysfs::read_opt(&supply.join("type")).as_deref() != Some("Mains") {
            continue;
        }
        match crate::sysfs::read_opt(&supply.join("online")).as_deref() {
            Some("1") => return Some(true),
            Some("0") => return Some(false),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_ac_and_battery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ac = root.join("sys/class/power_supply/ACAD");
        fs::create_dir_all(&ac).unwrap();
        fs::write(ac.join("type"), "Mains").unwrap();
        fs::write(ac.join("online"), "1").unwrap();
        assert_eq!(on_ac_under(root), Some(true));
        fs::write(ac.join("online"), "0").unwrap();
        assert_eq!(on_ac_under(root), Some(false));
    }

    #[test]
    fn ignores_non_mains_supplies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bat = root.join("sys/class/power_supply/BAT1");
        fs::create_dir_all(&bat).unwrap();
        fs::write(bat.join("type"), "Battery").unwrap();
        fs::write(bat.join("online"), "1").unwrap();
        assert_eq!(on_ac_under(root), None);
    }
}
