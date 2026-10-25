//! # thermite-core
//!
//! Pure thermal-policy logic for Thermite: fan curves, hysteresis, thermal
//! failsafes, performance profiles, configuration, and the hardware [`Backend`]
//! trait. This crate performs **no I/O** — everything here is deterministic and
//! unit-testable. Sysfs/NVML access lives in `thermite-backend`, the service
//! loop in `thermite-daemon`.
//!
//! ## Safety stance
//!
//! Fan-control policy is safety-critical: a wrong duty at a high temperature
//! can damage hardware. This crate therefore errs on the conservative side:
//! duty floors are applied *after* curve evaluation, crit thresholds hand
//! control back to the firmware's own fan controller, and every curve is
//! validated before use.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod curve;
pub mod failsafe;
pub mod hysteresis;
pub mod profile;
pub mod sensors;

pub use config::{Config, ConfigError, FanConfig, FanModeKind, GeneralConfig, PlatformConfig};
pub use curve::{CurveDef, CurveError, CurvePoint};
pub use failsafe::{FailsafeEngine, FailsafePhase, FailsafeThresholds, PhaseDirective};
pub use hysteresis::Hysteresis;
pub use profile::{PlatformProfile, KNOWN_PROFILES};
pub use sensors::{
    Backend, BackendError, CapabilityLevel, CapabilityReport, FanDescriptor, FanId, FanTelemetry,
    GpuTelemetry, HwFanMode, Telemetry, TempClass, TempSource, TempTelemetry,
};
