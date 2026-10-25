//! Device drivers.
//!
//! A *driver* binds Thermite to one machine's fan/thermal interface. Drivers
//! implement [`thermite_core::Backend`] (fan enumeration, telemetry, control)
//! plus [`Driver`] for discovery. Nothing requires a particular kernel
//! interface: a driver may sit on hwmon sysfs (acer-wmi, asus), WMI, or
//! anything else — it only has to answer the `Backend` contract and be honest
//! about what it cannot do via [`thermite_core::BackendError::Unsupported`].
//!
//! Adding a driver:
//! 1. create `drivers/<name>.rs` implementing `Driver` + `Backend`,
//! 2. register it in [`probe_drivers`],
//! 3. keep unsafe/no-EC-write rules (see docs/SAFETY.md).

pub mod acer_wmi;

use std::path::Path;

use thermite_core::Backend;

/// Driver discovery: binds a [`Backend`] implementation to hardware.
pub trait Driver: Backend + Sized {
    /// Stable driver name shown in `doctor` and capability reports.
    const NAME: &'static str;

    /// Try to bind under `root` (real root `/`, or a test fixture root).
    /// Return `None` when this driver doesn't match the machine — the
    /// registry then tries the next one.
    fn probe_driver(root: &Path) -> Option<Self>;
}

/// A driver that matched, ready to serve as the machine's backend.
pub struct DriverMatch {
    /// Driver name (e.g. `"acer-wmi"`).
    pub name: &'static str,
    /// The bound backend.
    pub backend: Box<dyn Backend>,
}

/// Registered drivers in priority order. First match wins.
pub fn probe_drivers(root: &Path) -> Option<DriverMatch> {
    if let Some(d) = acer_wmi::AcerWmiBackend::probe_driver(root) {
        return Some(DriverMatch {
            name: acer_wmi::AcerWmiBackend::NAME,
            backend: Box::new(d),
        });
    }
    None
}
