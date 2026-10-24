//! # thermite-daemon
//!
//! Thermite's privileged daemon: control loop with failsafes, UDS IPC
//! server, suspend handling, state persistence.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod control;
pub mod dryrun;
pub mod server;
pub mod state;
pub mod suspend;
