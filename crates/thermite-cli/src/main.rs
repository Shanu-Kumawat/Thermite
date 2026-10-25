//! # thermite — command-line interface
//!
//! Talks to the daemon over its Unix socket; `doctor` probes hardware
//! directly and works without the daemon.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use thermite_core::config::{Config, FanConfig, FanModeKind};
use thermite_core::{CapabilityLevel, CurvePoint, FanId};
use thermite_proto::{
    Client, ControlSnapshot, DEFAULT_SOCKET_PATH, Event, Request, Response, StatusSnapshot,
};

#[derive(Debug, Parser)]
#[command(
    name = "thermite",
    version,
    about = "Thermal management for the Acer Nitro V 15 (ANV15-51)",
    after_help = "Fan control needs the daemon running as root (thermite-daemon / thermite.service)."
)]
struct Cli {
    /// Daemon socket path
    #[arg(long, default_value = DEFAULT_SOCKET_PATH, global = true)]
    socket: PathBuf,
    /// Machine-readable JSON output where supported
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Current status snapshot (temperatures, fans, modes, capability)
    Status,
    /// Live telemetry until Ctrl-C
    Monitor,
    /// Set a fan's control mode
    Fan {
        /// Fan to configure
        #[command(subcommand)]
        fan: FanSpec,
    },
    /// Platform performance profiles
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Inspect fan curves
    Curve {
        #[command(subcommand)]
        action: CurveAction,
    },
    /// Configuration utilities
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Diagnose hardware/driver/daemon wiring (no daemon required)
    Doctor,
}

#[derive(Debug, Subcommand)]
enum FanSpec {
    /// List fans reported by the driver
    List,
    /// Apply an action to one fan by name (e.g. cpu, gpu)
    Set {
        /// Fan name — see `thermite fan list`
        name: String,
        #[command(subcommand)]
        action: FanAction,
    },
}

#[derive(Debug, Subcommand)]
enum FanAction {
    /// Hand control back to the firmware
    Auto,
    /// Fixed duty cycle (percent)
    Manual { duty: u8 },
    /// Follow a named curve
    Curve { name: String },
}

#[derive(Debug, Subcommand)]
enum ProfileAction {
    /// Show the active profile
    Get,
    /// List profiles offered by the firmware
    List,
    /// Apply a profile (e.g. quiet, balanced, performance)
    Set { profile: String },
}

#[derive(Debug, Subcommand)]
enum CurveAction {
    /// List defined curves
    List,
    /// Show one curve's points
    Show { name: String },
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print the default config path
    Path,
    /// Print the daemon's active configuration (TOML)
    Show,
    /// Validate a config file locally (default: the standard path)
    Check { path: Option<PathBuf> },
}

fn connect(socket: &Path) -> Result<Client> {
    Client::connect(
        socket,
        &format!("thermite-cli {}", env!("CARGO_PKG_VERSION")),
    )
    .with_context(|| format!("connecting to daemon at {}", socket.display()))
    .inspect_err(|_| {
        eprintln!("hint: is thermite-daemon running? Try `systemctl status thermite`.");
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Status => status(&cli.socket, cli.json),
        Command::Monitor => monitor(&cli.socket),
        Command::Fan { fan } => set_fan(&cli.socket, fan),
        Command::Profile { action } => profile(&cli.socket, action),
        Command::Curve { action } => curve(&cli.socket, action),
        Command::Config { action } => config(&cli.socket, action),
        Command::Doctor => doctor(cli.json),
    }
}

// ---------- status ----------

fn status(socket: &Path, json: bool) -> Result<()> {
    let mut client = connect(socket)?;
    let Response::Status { status } = client.call(Request::GetStatus)? else {
        bail!("unexpected response");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    print_status(&status);
    Ok(())
}

fn print_status(s: &StatusSnapshot) {
    let cap = s.daemon.capability.level;
    println!(
        "daemon      {} (pid {}, up {}s{})",
        s.daemon.version,
        s.daemon.pid,
        s.daemon.uptime_ms / 1000,
        if s.daemon.dry_run { ", DRY-RUN" } else { "" }
    );
    println!(
        "capability  {}",
        match cap {
            CapabilityLevel::Full => "full (monitoring + manual fan control)".to_string(),
            CapabilityLevel::MonitorProfiles =>
                "monitoring + platform profiles (no manual pwm)".to_string(),
            CapabilityLevel::None => "no acer-wmi fan interface (generic sensors only)".to_string(),
        }
    );
    println!(
        "platform    {}",
        s.telemetry.platform_profile.as_deref().unwrap_or("n/a")
    );
    println!(
        "failsafe    {}{}",
        match s.control.failsafe_phase {
            thermite_core::FailsafePhase::Normal => "normal".to_string(),
            thermite_core::FailsafePhase::ForceFull =>
                "FORCE FULL DUTY (warn threshold)".to_string(),
            thermite_core::FailsafePhase::ReleasedAuto =>
                "RELEASED TO FIRMWARE (crit threshold)".to_string(),
        },
        if s.control.suspended_grace {
            " [post-resume grace]"
        } else {
            ""
        }
    );
    println!(
        "power       {}",
        match s.telemetry.on_ac {
            Some(true) => "AC",
            Some(false) => "battery",
            None => "unknown",
        }
    );

    println!();
    println!(
        "{:<14} {:>10} {:>8} {:>10} {:<14}",
        "fan", "rpm", "duty%", "hw mode", "configured"
    );
    for runtime in &s.control.fans {
        let hw = s.telemetry.fans.iter().find(|f| f.id == runtime.id);
        println!(
            "{:<14} {:>10} {:>8} {:>10} {:<14}",
            runtime.id,
            hw.and_then(|f| f.rpm)
                .map(|r| r.to_string())
                .unwrap_or("-".into()),
            hw.and_then(|f| f.duty_pct)
                .map(|d| d.to_string())
                .unwrap_or("-".into()),
            hw.and_then(|f| f.hw_mode)
                .map(|m| hw_mode_str(m).to_string())
                .unwrap_or("-".into()),
            configured_str(runtime),
        );
    }

    println!();
    let mut temps = s.telemetry.temps.clone();
    temps.sort_by(|a, b| b.temp_c.total_cmp(&a.temp_c));
    println!("{:<28} {:<10} {:>8}", "sensor", "class", "temp");
    for t in &temps {
        println!(
            "{:<28} {:<10} {:>7.1}°C",
            t.label,
            temp_class_str(t.class),
            t.temp_c
        );
    }
    if let Some(gpu) = &s.telemetry.gpu {
        println!("{:<28} {:<10} {:>7.1}°C", gpu.name, "gpu", gpu.temp_c);
    }

    if !s.recent_alerts.is_empty() {
        println!();
        println!("recent alerts:");
        for a in &s.recent_alerts {
            println!("  [{:?}] {:?}: {}", a.level, a.kind, a.message);
        }
    }
}

fn hw_mode_str(m: thermite_core::HwFanMode) -> &'static str {
    match m {
        thermite_core::HwFanMode::Auto => "auto",
        thermite_core::HwFanMode::Custom => "custom",
        thermite_core::HwFanMode::Turbo => "turbo",
    }
}

fn temp_class_str(c: thermite_core::TempClass) -> &'static str {
    match c {
        thermite_core::TempClass::Cpu => "cpu",
        thermite_core::TempClass::Gpu => "gpu",
        thermite_core::TempClass::Nvme => "nvme",
        thermite_core::TempClass::Board => "board",
        thermite_core::TempClass::Wifi => "wifi",
        thermite_core::TempClass::Other => "other",
    }
}

fn configured_str(rt: &thermite_proto::FanRuntimeState) -> String {
    match rt.config.mode {
        FanModeKind::Auto => "auto".to_string(),
        FanModeKind::Manual => format!("manual {}%", rt.config.manual_duty_pct.unwrap_or(0)),
        FanModeKind::Curve => format!(
            "curve {}",
            rt.resolved_curve
                .as_deref()
                .or(rt.config.curve.as_deref())
                .unwrap_or("?")
        ),
    }
}

// ---------- monitor ----------

fn monitor(socket: &Path) -> Result<()> {
    let mut client = connect(socket)?;
    client.subscribe()?;
    println!("monitoring (Ctrl-C to stop)");
    loop {
        match client.read_event()? {
            Event::Tick { telemetry, control } => {
                let cpu = telemetry
                    .cpu_temp_c()
                    .map(|t| format!("{t:.0}°C"))
                    .unwrap_or("-".into());
                let gpu = telemetry
                    .gpu_temp_c()
                    .map(|t| format!("{t:.0}°C"))
                    .unwrap_or("-".into());
                let fans: Vec<String> = telemetry
                    .fans
                    .iter()
                    .map(|f| {
                        format!(
                            "{} {}rpm {}%",
                            f.id,
                            f.rpm.map(|r| r.to_string()).unwrap_or("-".into()),
                            f.duty_pct.map(|d| d.to_string()).unwrap_or("-".into())
                        )
                    })
                    .collect();
                println!(
                    "\r\x1b[2Kcpu {cpu}  gpu {gpu}  |  {}  |  {}  |  {}  ",
                    fans.join("  |  "),
                    telemetry.platform_profile.as_deref().unwrap_or("-"),
                    phase_str(&control)
                );
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            Event::Alert(alert) => {
                println!();
                println!(
                    "ALERT [{:?}] {:?}: {}",
                    alert.level, alert.kind, alert.message
                );
            }
            Event::ConfigChanged { .. } => {
                println!();
                println!("configuration changed");
            }
        }
    }
}

fn phase_str(control: &ControlSnapshot) -> &'static str {
    match control.failsafe_phase {
        thermite_core::FailsafePhase::Normal => "ok",
        thermite_core::FailsafePhase::ForceFull => "FORCE-FULL",
        thermite_core::FailsafePhase::ReleasedAuto => "RELEASED",
    }
}

// ---------- fan ----------

fn set_fan(socket: &Path, fan: FanSpec) -> Result<()> {
    if let FanSpec::List = fan {
        let mut client = connect(socket)?;
        let Response::Status { status } = client.call(Request::GetStatus)? else {
            bail!("unexpected response");
        };
        for f in &status.control.fans {
            println!("{:<12} {}", f.id, configured_str_raw(&f.config));
        }
        return Ok(());
    }
    let FanSpec::Set { name, action } = fan else {
        unreachable!("list handled above");
    };
    let fan = FanId::new(&name);
    let config = match action {
        FanAction::Auto => FanConfig {
            mode: FanModeKind::Auto,
            curve: None,
            manual_duty_pct: None,
        },
        FanAction::Manual { duty } if duty <= 100 => FanConfig {
            mode: FanModeKind::Manual,
            curve: None,
            manual_duty_pct: Some(duty),
        },
        FanAction::Manual { duty } => bail!("duty must be 0-100, got {duty}"),
        FanAction::Curve { name } => FanConfig {
            mode: FanModeKind::Curve,
            curve: Some(name),
            manual_duty_pct: None,
        },
    };
    let mut client = connect(socket)?;
    match client.call(Request::SetFanConfig {
        fan: fan.clone(),
        config: config.clone(),
    })? {
        Response::Ok => {
            println!("{fan} fan -> {}", configured_str_raw(&config));
            Ok(())
        }
        Response::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

fn configured_str_raw(config: &FanConfig) -> String {
    match config.mode {
        FanModeKind::Auto => "firmware auto".to_string(),
        FanModeKind::Manual => format!("manual {}%", config.manual_duty_pct.unwrap_or(0)),
        FanModeKind::Curve => format!("curve \"{}\"", config.curve.clone().unwrap_or_default()),
    }
}

// ---------- profile ----------

fn profile(socket: &Path, action: ProfileAction) -> Result<()> {
    let mut client = connect(socket)?;
    match action {
        ProfileAction::Get => {
            let Response::Status { status } = client.call(Request::GetStatus)? else {
                bail!("unexpected response");
            };
            match &status.telemetry.platform_profile {
                Some(p) => {
                    println!("{p}");
                    Ok(())
                }
                None => bail!("platform profile interface not available (see `thermite doctor`)"),
            }
        }
        ProfileAction::List => {
            let Response::Status { status } = client.call(Request::GetStatus)? else {
                bail!("unexpected response");
            };
            let profiles = &status.daemon.capability.platform_profiles;
            if profiles.is_empty() {
                bail!("no platform profiles exposed (see `thermite doctor`)");
            }
            for p in profiles {
                println!("{p}");
            }
            Ok(())
        }
        ProfileAction::Set { profile } => match client.call(Request::SetPlatformProfile {
            profile: profile.clone(),
        })? {
            Response::Ok => {
                println!("profile -> {profile}");
                Ok(())
            }
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected response: {other:?}"),
        },
    }
}

// ---------- curve ----------

fn curve(socket: &Path, action: CurveAction) -> Result<()> {
    let mut client = connect(socket)?;
    let Response::Config { config } = client.call(Request::GetConfig)? else {
        bail!("unexpected response");
    };
    match action {
        CurveAction::List => {
            for (name, def) in &config.curves {
                let first = def.points.first().map(|p| p.0).unwrap_or(0.0);
                let last = def.points.last().map(|p| p.0).unwrap_or(0.0);
                println!(
                    "{name:<24} {} points, {first:.0}-{last:.0}°C",
                    def.points.len()
                );
            }
            Ok(())
        }
        CurveAction::Show { name } => {
            let Some(def) = config.curves.get(&name) else {
                bail!("no curve named \"{name}\" (see `thermite curve list`)");
            };
            println!("{name}:");
            for CurvePoint(temp, duty) in &def.points {
                println!("  {:>6.1}°C -> {:>3}%", temp, duty);
            }
            Ok(())
        }
    }
}

// ---------- config ----------

fn config(socket: &Path, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Path => {
            println!("/etc/thermite/thermite.toml");
            Ok(())
        }
        ConfigAction::Show => {
            let mut client = connect(socket)?;
            let Response::Config { config } = client.call(Request::GetConfig)? else {
                bail!("unexpected response");
            };
            print!("{}", config.to_toml_string()?);
            Ok(())
        }
        ConfigAction::Check { path } => {
            let path = path.unwrap_or_else(|| PathBuf::from("/etc/thermite/thermite.toml"));
            match Config::load(&path) {
                Ok(_) => {
                    println!("{}: OK", path.display());
                    Ok(())
                }
                Err(e) => bail!("{}: {e}", path.display()),
            }
        }
    }
}

// ---------- doctor ----------

fn doctor(json: bool) -> Result<()> {
    let report = thermite_backend::capability::probe(Path::new("/"));
    let temps = thermite_backend::hwmon::scan_temps().unwrap_or_default();
    let on_ac = thermite_backend::power::on_ac();
    let gpu = thermite_backend::nvml::NvmlSource::new().read();
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let product = read_dmi("product_name");
    let bios = read_dmi("bios_version");
    let predator_param = std::fs::read_to_string("/sys/module/acer_wmi/parameters/predator_v4")
        .map(|s| s.trim().to_string())
        .ok();
    let daemon = Client::connect(Path::new(DEFAULT_SOCKET_PATH), "thermite-cli doctor")
        .ok()
        .and_then(|mut c| match c.call(Request::GetStatus) {
            Ok(Response::Status { status }) => Some(status),
            _ => None,
        });

    if json {
        #[derive(serde::Serialize)]
        struct DoctorOut {
            kernel: String,
            product: String,
            bios: String,
            capability: thermite_core::CapabilityReport,
            predator_v4: Option<String>,
            sensors: Vec<thermite_core::TempTelemetry>,
            on_ac: Option<bool>,
            gpu: Option<thermite_core::GpuTelemetry>,
            daemon: Option<StatusSnapshot>,
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&DoctorOut {
                kernel,
                product,
                bios,
                capability: report,
                predator_v4: predator_param,
                sensors: temps,
                on_ac,
                gpu,
                daemon,
            })?
        );
        return Ok(());
    }

    println!("thermite doctor");
    println!("  kernel    {kernel}");
    println!("  platform  {} (BIOS {})", product, bios);
    if let Some(p) = predator_param {
        println!("  acer_wmi.predator_v4 = {p}");
    }
    println!();
    println!(
        "driver: {} (capability {:?})",
        report.driver.as_deref().unwrap_or("none matched"),
        report.level
    );
    if let Some(h) = &report.driver_detail {
        println!("  detail: {h}");
    }
    for note in &report.notes {
        println!("  - {note}");
    }
    println!();
    println!("sensors:");
    for t in &temps {
        println!(
            "  {:<28} {:<8} {:>6.1}°C",
            t.label,
            temp_class_str(t.class),
            t.temp_c
        );
    }
    match &gpu {
        Some(g) => println!("  {:<28} {:<8} {:>6.1}°C", g.name, "gpu", g.temp_c),
        None => println!("  NVML: unavailable"),
    }
    println!();
    println!(
        "power: {}",
        match on_ac {
            Some(true) => "AC",
            Some(false) => "battery",
            None => "unknown",
        }
    );
    match &daemon {
        Some(s) => println!(
            "daemon: running (pid {}, {}, capability {:?})",
            s.daemon.pid,
            if s.daemon.dry_run { "dry-run" } else { "live" },
            s.daemon.capability.level
        ),
        None => println!("daemon: not reachable at {DEFAULT_SOCKET_PATH}"),
    }
    if report.level == CapabilityLevel::Full {
        println!();
        println!("everything wired for full fan control.");
    } else if report.level == CapabilityLevel::MonitorProfiles {
        println!();
        println!("monitoring + profiles work; manual fan control needs kernel 7.3+");
        println!("or the stopgap DKMS module (driver/acer-wmi-anv15).");
    }
    Ok(())
}

fn read_dmi(field: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/dmi/id/{field}"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}
