//! Small sysfs helpers. Every function takes an explicit filesystem `root`
//! so the whole backend is testable against a tempdir fixture; production
//! code passes `/`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Read a file and trim surrounding whitespace (sysfs convention).
pub fn read_trimmed(path: &Path) -> io::Result<String> {
    let s = fs::read_to_string(path)?;
    Ok(s.trim().to_string())
}

/// Read a file to a trimmed string, `None` on any error.
pub fn read_opt(path: &Path) -> Option<String> {
    read_trimmed(path).ok()
}

/// Parse a whole file as `T`.
pub fn parse_num<T: FromStr>(path: &Path) -> Option<T> {
    read_opt(path).and_then(|s| s.parse().ok())
}

/// Overwrite a sysfs attribute.
pub fn write_str(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value)
}

/// All `hwmon*` directories under `root/sys/class/hwmon`, sorted by name.
/// Handles the real sysfs (symlinked dirs) and plain fixture dirs alike.
pub fn hwmon_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match fs::read_dir(root.join("sys/class/hwmon")) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("hwmon"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort();
    dirs
}

/// The `name` attribute of a hwmon directory, if readable.
pub fn hwmon_name(dir: &Path) -> Option<String> {
    read_opt(&dir.join("name"))
}
