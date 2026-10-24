//! Daemon state: the shared runtime state, persistence of mutable
//! selections, and alert buffering.
//!
//! Ownership model: the control loop owns the [`thermite_core::Backend`];
//! IPC connection threads only ever touch [`SharedState`] under its mutex.
//! Mutations requested over IPC take effect on the next control tick
//! (≤ `poll_interval_ms`).

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thermite_core::FailsafePhase;
use thermite_core::config::{Config, FanConfig};
use thermite_core::sensors::{CapabilityReport, Telemetry};
use thermite_proto::{Alert, ControlSnapshot, FanRuntimeState};

/// Mutable per-fan runtime state (desired config + last observed effects).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FanRuntime {
    /// Mode the user asked for.
    pub config: FanConfig,
    /// Curve actually in effect (battery variant when applied).
    pub resolved_curve: Option<String>,
    /// Last duty percent commanded by us this session.
    pub commanded_duty: Option<u8>,
    /// Last hardware write error for this fan.
    pub last_write_error: Option<String>,
}

/// Persisted selections (`state.json`): what the user chose, so a daemon
/// restart restores their intent (config file provides the defaults).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PersistedState {
    /// Schema version (currently 1).
    pub version: u32,
    /// Per-fan desired configs.
    pub fans: BTreeMap<String, FanConfig>,
    /// Desired platform profile name.
    pub platform_profile: Option<String>,
}

/// Load persisted state from `dir/state.json`; missing file is not an error.
pub fn load_persisted(dir: &Path) -> anyhow::Result<PersistedState> {
    let path = dir.join("state.json");
    match fs::read_to_string(&path) {
        Ok(s) => {
            let state: PersistedState = serde_json::from_str(&s)
                .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
            if state.version != 1 {
                anyhow::bail!("unsupported state version {}", state.version);
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PersistedState::default()),
        Err(e) => Err(e.into()),
    }
}

/// Atomically persist state (tmp file + rename).
pub fn save_persisted(dir: &Path, state: &PersistedState) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join("state.json");
    let tmp = dir.join(".state.json.tmp");
    let json = serde_json::to_string_pretty(state)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Effective per-fan desired config: persisted state overlays config-file
/// defaults.
pub fn effective_fans(config: &Config, persisted: &PersistedState) -> BTreeMap<String, FanConfig> {
    let mut fans = BTreeMap::new();
    fans.insert("cpu".to_string(), config.fans.cpu.clone());
    fans.insert("gpu".to_string(), config.fans.gpu.clone());
    for (fan, cfg) in &persisted.fans {
        fans.insert(fan.clone(), cfg.clone());
    }
    fans
}

/// Everything the IPC server and control loop share.
pub struct SharedState {
    /// Current policy configuration (SIGHUP reloads it).
    pub config: Config,
    /// Per-fan desired + observed state, keyed by fan name string.
    pub fans: BTreeMap<String, FanRuntime>,
    /// Desired platform profile (applies at startup, on change, after resume).
    pub desired_profile: Option<String>,
    /// Set once the desired profile has been successfully applied.
    pub profile_applied: bool,
    /// Last profile application error, if any.
    pub profile_error: Option<String>,
    /// Latest telemetry snapshot.
    pub telemetry: Telemetry,
    /// Current failsafe phase.
    pub failsafe_phase: FailsafePhase,
    /// While `Some(now)`: post-resume grace, no hardware writes.
    pub grace_until: Option<Instant>,
    /// Shutting down: stop writing, restore auto on the way out.
    pub shutting_down: bool,
    /// Ring buffer of recent alerts (bounded).
    pub alerts: VecDeque<Alert>,
    /// Daemon start time.
    pub started: Instant,
    /// Dry-run mode (writes logged, not performed).
    pub dry_run: bool,
    /// Count of suppressed writes in dry-run mode.
    pub dry_run_writes: u64,
    /// Hardware capability summary (set once at startup).
    pub capability: CapabilityReport,
}

impl SharedState {
    /// Assemble initial state from config + persisted selections.
    pub fn new(config: Config, persisted: &PersistedState, dry_run: bool) -> Self {
        let fans = effective_fans(&config, persisted)
            .into_iter()
            .map(|(fan, config)| {
                (
                    fan,
                    FanRuntime {
                        config,
                        ..FanRuntime::default()
                    },
                )
            })
            .collect();
        Self {
            config,
            fans,
            desired_profile: persisted.platform_profile.clone(),
            profile_applied: false,
            profile_error: None,
            telemetry: Telemetry::default(),
            failsafe_phase: FailsafePhase::Normal,
            grace_until: None,
            shutting_down: false,
            alerts: VecDeque::new(),
            started: Instant::now(),
            dry_run,
            dry_run_writes: 0,
            capability: CapabilityReport::default(),
        }
    }

    /// Record an alert (bounded ring) and return it for broadcast.
    pub fn push_alert(&mut self, alert: Alert) -> Alert {
        if self.alerts.len() >= 128 {
            self.alerts.pop_front();
        }
        self.alerts.push_back(alert.clone());
        alert
    }

    /// Build the control snapshot for IPC.
    pub fn control_snapshot(&self) -> ControlSnapshot {
        ControlSnapshot {
            fans: self
                .fans
                .iter()
                .map(|(fan, rt)| FanRuntimeState {
                    id: if fan == "cpu" {
                        thermite_core::FanId::Cpu
                    } else {
                        thermite_core::FanId::Gpu
                    },
                    config: rt.config.clone(),
                    resolved_curve: rt.resolved_curve.clone(),
                    commanded_duty: rt.commanded_duty,
                    last_write_error: rt.last_write_error.clone(),
                })
                .collect(),
            failsafe_phase: self.failsafe_phase,
            suspended_grace: self.grace_until.is_some(),
        }
    }
}

/// Paths the daemon operates on.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    /// Configuration TOML.
    pub config: PathBuf,
    /// Runtime socket.
    pub socket: PathBuf,
    /// Persistent state directory.
    pub state_dir: PathBuf,
}
