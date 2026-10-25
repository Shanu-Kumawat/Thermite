//! # thermite-gui
//!
//! Desktop client for the thermite daemon, built on GPUI (via gpui-kit).
//! Two connections to the daemon: one subscribed to events (reader thread),
//! one for request/response (command thread) — this avoids async plumbing
//! around the blocking IPC client.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::{Icon, Theme, ThemeMode, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use thermite_core::config::{Config, FanConfig, FanModeKind};
use thermite_core::{CapabilityLevel, CurveDef, CurvePoint, FailsafePhase, FanId, HwFanMode};
use thermite_proto::{
    Alert, AlertLevel, Client, DEFAULT_SOCKET_PATH, DaemonInfo, Event, Request, Response,
    StatusSnapshot,
};

mod icons {
    //! Lucide icons (ISC license, https://lucide.dev) bundled as raw SVG so
    //! the UI can use glyphs outside the default asset bundle.
    pub const HOUSE: &str = include_str!("../assets/icons/house.svg");
    pub const FAN: &str = include_str!("../assets/icons/fan.svg");
    pub const CHART_LINE: &str = include_str!("../assets/icons/chart-line.svg");
    pub const LAYOUT_GRID: &str = include_str!("../assets/icons/layout-grid.svg");
    pub const LAYERS: &str = include_str!("../assets/icons/layers.svg");
    pub const ZAP: &str = include_str!("../assets/icons/zap.svg");
    pub const SHIELD_CHECK: &str = include_str!("../assets/icons/shield-check.svg");
    pub const THERMOMETER: &str = include_str!("../assets/icons/thermometer.svg");
    pub const LIST_FILTER: &str = include_str!("../assets/icons/list-filter.svg");
    pub const FLAME: &str = include_str!("../assets/icons/flame.svg");
    pub const SETTINGS: &str = include_str!("../assets/icons/settings.svg");
    pub const INFO: &str = include_str!("../assets/icons/info.svg");
    pub const MONITOR: &str = include_str!("../assets/icons/monitor.svg");
}

/// Render a bundled Lucide SVG at a given size and color.
fn svg_icon(bytes: &str, color: Hsla, size: f32) -> Icon {
    Icon::default()
        .data(bytes.as_bytes())
        .text_color(color)
        .size(px(size))
}

/// Static machine identity for the header subtitle (read once at startup).
struct MachineInfo {
    product: String,
    os: String,
}

impl MachineInfo {
    fn probe() -> Self {
        let product = std::fs::read_to_string("/sys/class/dmi/id/product_name")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown machine".into());
        let os = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("PRETTY_NAME=")
                        .map(|v| v.trim_matches('"').trim().to_string())
                })
            })
            .unwrap_or_else(|| "Linux".into());
        Self { product, os }
    }
}

/// Samples kept for charts (at 1 Hz ≈ 4 minutes).
const MAX_SAMPLES: usize = 240;
/// Minimum gap between live slider duty pushes while dragging.
const THROTTLE: Duration = Duration::from_millis(100);

/// Monochrome design tokens: deep black background, charcoal surfaces,
/// subtle gray borders, white typography, restrained red accent.
mod ui {
    use gpui_kit::{Hsla, hsla};

    /// App background.
    pub const BG: Hsla = hsla(0., 0., 0.04, 1.);
    /// Card / pill surface.
    pub const SURFACE: Hsla = hsla(0., 0., 0.075, 1.);
    /// Hover / icon-tile surface.
    pub const SURFACE_RAISED: Hsla = hsla(0., 0., 0.12, 1.);
    /// Hairline borders.
    pub const BORDER: Hsla = hsla(0., 0., 0.15, 1.);
    /// Primary text.
    pub const TEXT: Hsla = hsla(0., 0., 0.95, 1.);
    /// Secondary text.
    pub const TEXT_MUTED: Hsla = hsla(0., 0., 0.62, 1.);
    /// Tertiary text (eyebrows, axis hints).
    pub const TEXT_FAINT: Hsla = hsla(0., 0., 0.42, 1.);
    /// Red accent: active states, thermal warnings, the CPU metric.
    pub const ACCENT: Hsla = hsla(0.008, 0.78, 0.56, 1.);
    /// Accent at low alpha: active nav, selected chips.
    pub const ACCENT_DIM: Hsla = hsla(0.008, 0.78, 0.56, 0.13);
    /// OK state (failsafe normal, daemon online dot).
    pub const OK: Hsla = hsla(0.4, 0.55, 0.52, 1.);
    /// Soft warning (battery, post-resume grace).
    pub const WARN: Hsla = hsla(0.09, 0.85, 0.62, 1.);
}
use ui::{
    ACCENT, ACCENT_DIM, BG, BORDER, OK, SURFACE, SURFACE_RAISED, TEXT, TEXT_FAINT, TEXT_MUTED, WARN,
};

/// One telemetry sample for the charts.
#[derive(Clone)]
struct Sample {
    t: f64,
    cpu: f64,
    gpu: f64,
    cpu_rpm: f64,
    gpu_rpm: f64,
}

/// Events delivered from IPC threads into the UI.
enum IpcEvent {
    /// Connection established (or re-established).
    Connected,
    /// Daemon went away.
    Disconnected,
    /// A tick arrived.
    Tick(Box<StatusSnapshot>),
    /// Alert pushed.
    Alert(Alert),
    /// Config (re)loaded.
    Config(Box<Config>),
    /// A command completed.
    CommandDone(Result<(), String>),
}

/// UI-to-command-thread requests.
enum GuiCommand {
    /// Change one fan's settings.
    SetFan { fan: FanId, config: FanConfig },
    /// Apply a platform profile.
    SetProfile { profile: String },
    /// Create/replace a curve.
    PutCurve {
        name: String,
        points: Vec<CurvePoint>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Nav {
    #[default]
    Dashboard,
    Fans,
    Profiles,
}

impl Nav {
    fn label(self) -> &'static str {
        match self {
            Nav::Dashboard => "Dashboard",
            Nav::Fans => "Fans",
            Nav::Profiles => "Profiles",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Nav::Dashboard => icons::HOUSE,
            Nav::Fans => icons::FAN,
            Nav::Profiles => icons::LAYOUT_GRID,
        }
    }
    fn all() -> [Nav; 3] {
        [Nav::Dashboard, Nav::Fans, Nav::Profiles]
    }
}

/// Root UI entity: owns connection state, latest snapshots, chart history
/// and the command channel.
pub struct ThermiteApp {
    socket: PathBuf,
    machine: MachineInfo,
    nav: Nav,
    connected: bool,
    status: Option<StatusSnapshot>,
    config: Config,
    samples: VecDeque<Sample>,
    alerts: VecDeque<Alert>,
    toast: Option<(String, bool)>, // (message, is_error)
    cmd_tx: Option<Sender<GuiCommand>>,
    /// Per-fan working copy of curve points while editing (fan -> points).
    curve_draft: BTreeMap<String, Vec<CurvePoint>>,
    /// Per-fan duty preview while the manual slider is being dragged.
    manual_draft: BTreeMap<String, u8>,
    /// Curve point currently being dragged: (fan, point index).
    drag_point: Option<(String, usize)>,
    /// Fan whose duty slider is being dragged.
    slider_drag: Option<String>,
    /// Last instant a slider drag sent SetFanConfig (throttled).
    last_slider_send: Option<(String, Instant)>,
}

impl ThermiteApp {
    fn new(socket: PathBuf, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (ipc_tx, ipc_rx) = mpsc::channel::<IpcEvent>();
        spawn_reader_thread(socket.clone(), ipc_tx.clone());
        let (cmd_tx, cmd_rx) = mpsc::channel::<GuiCommand>();
        spawn_command_thread(socket.clone(), ipc_tx, cmd_rx);

        // Drain IPC events into UI state at 10 Hz.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let mut queued: Vec<IpcEvent> = Vec::new();
                while let Ok(ev) = ipc_rx.try_recv() {
                    queued.push(ev);
                }
                if !queued.is_empty() {
                    let _ = this.update(cx, |app, cx| {
                        for ev in queued {
                            app.apply_event(ev);
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();

        Self {
            socket,
            machine: MachineInfo::probe(),
            nav: Nav::Dashboard,
            connected: false,
            status: None,
            config: Config::with_default_curves(),
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            alerts: VecDeque::new(),
            toast: None,
            cmd_tx: Some(cmd_tx),
            curve_draft: BTreeMap::new(),
            manual_draft: BTreeMap::new(),
            drag_point: None,
            slider_drag: None,
            last_slider_send: None,
        }
    }

    fn apply_event(&mut self, ev: IpcEvent) {
        match ev {
            // Connection state is already visible in the sidebar pill; no
            // status-bar toast for it.
            IpcEvent::Connected => {
                self.connected = true;
            }
            IpcEvent::Disconnected => {
                self.connected = false;
                self.status = None;
            }
            IpcEvent::Tick(status) => {
                let telemetry = &status.telemetry; // Box deref
                self.samples.push_back(Sample {
                    // Window-relative seconds: each tick ≈ 1s, so the chart
                    // axis reads 0s..240s instead of raw daemon uptime.
                    t: self.samples.len() as f64,
                    cpu: telemetry.cpu_temp_c().map(f64::from).unwrap_or(0.0),
                    gpu: telemetry.gpu_temp_c().map(f64::from).unwrap_or(0.0),
                    cpu_rpm: telemetry
                        .fan(FanId::Cpu)
                        .and_then(|f| f.rpm)
                        .map(f64::from)
                        .unwrap_or(0.0),
                    gpu_rpm: telemetry
                        .fan(FanId::Gpu)
                        .and_then(|f| f.rpm)
                        .map(f64::from)
                        .unwrap_or(0.0),
                });
                while self.samples.len() > MAX_SAMPLES {
                    self.samples.pop_front();
                }
                self.status = Some(*status);
            }
            IpcEvent::Alert(alert) => {
                if self.alerts.len() >= 64 {
                    self.alerts.pop_front();
                }
                self.alerts.push_back(alert);
            }
            IpcEvent::Config(config) => {
                self.config = *config;
                // Committed curves arrived; local drafts are superseded.
                self.curve_draft.clear();
            }
            IpcEvent::CommandDone(Ok(())) => {
                self.toast = Some(("applied".to_string(), false));
            }
            IpcEvent::CommandDone(Err(e)) => {
                self.toast = Some((e, true));
            }
        }
    }

    fn send(&self, cmd: GuiCommand) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }

    fn fan_state(&self, fan: FanId) -> Option<&thermite_proto::FanRuntimeState> {
        self.status
            .as_ref()
            .and_then(|s| s.control.fans.iter().find(|f| f.id == fan))
    }

    fn default_curve_name(&self) -> String {
        if self.config.curves.contains_key("balanced") {
            "balanced".to_string()
        } else {
            self.config
                .curves
                .keys()
                .next()
                .cloned()
                .unwrap_or_default()
        }
    }

    // ---------- fan page helpers ----------

    /// The curve a fan currently follows (config-selected, or the default).
    fn fan_curve_name(&self, fan: FanId) -> String {
        self.fan_state(fan)
            .and_then(|r| r.config.curve.clone())
            .filter(|c| self.config.curves.contains_key(c))
            .unwrap_or_else(|| self.default_curve_name())
    }

    /// The points shown in a fan's curve editor: the local draft while
    /// editing, otherwise the committed curve.
    fn fan_points(&self, fan: FanId) -> Vec<CurvePoint> {
        if let Some(draft) = self.curve_draft.get(&fan.to_string()) {
            return draft.clone();
        }
        self.config
            .curves
            .get(&self.fan_curve_name(fan))
            .map(|c| c.points.clone())
            .unwrap_or_default()
    }

    /// The duty shown for a fan in manual mode: slider preview or committed.
    fn fan_duty(&self, fan: FanId) -> u8 {
        if let Some(d) = self.manual_draft.get(&fan.to_string()) {
            return *d;
        }
        self.fan_state(fan)
            .and_then(|r| r.config.manual_duty_pct)
            .unwrap_or(40)
    }

    /// Validate and commit a curve edit: preview locally, push to the
    /// daemon (which persists it); invalid curves are rejected silently —
    /// the editor falls back to the last committed state.
    fn commit_curve(&mut self, fan: FanId, points: Vec<CurvePoint>) {
        if points.is_empty() || CurveDef::validated(points.clone()).is_err() {
            self.curve_draft.remove(&fan.to_string());
            return;
        }
        self.curve_draft.insert(fan.to_string(), points.clone());
        self.send(GuiCommand::PutCurve {
            name: self.fan_curve_name(fan),
            points,
        });
    }

    // ---------- rendering ----------

    fn render_nav(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(210.))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(BORDER)
            .bg(BG)
            .child(
                // Brand
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_5()
                    .h(px(56.))
                    .child(svg_icon(icons::FLAME, ACCENT, 20.))
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(TEXT)
                            .child("Thermite"),
                    ),
            )
            .px_2()
            .gap_1()
            .children(Nav::all().map(|nav| {
                let active = self.nav == nav;
                let label = nav.label();
                h_flex()
                    .id(SharedString::new(format!("nav-{label}")))
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .text_sm()
                    .when(active, |this| this.bg(ACCENT_DIM).text_color(TEXT))
                    .when(!active, |this| {
                        this.text_color(TEXT_MUTED)
                            .hover(|this| this.bg(SURFACE_RAISED).text_color(TEXT))
                    })
                    .on_click(cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                        this.nav = nav;
                        cx.notify();
                    }))
                    .child(svg_icon(
                        nav.icon(),
                        if active { TEXT } else { TEXT_MUTED },
                        16.,
                    ))
                    .child(label.to_string())
            }))
            .child(
                // Daemon status pinned to the bottom
                h_flex()
                    .mt_auto()
                    .mx_2()
                    .mb_3()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .when(self.connected, |this| this.bg(SURFACE).text_color(OK))
                    .when(!self.connected, |this| this.bg(SURFACE).text_color(ACCENT))
                    .child(div().size(px(8.)).rounded_full().bg(if self.connected {
                        OK
                    } else {
                        ACCENT
                    }))
                    .child(div().text_sm().child(if self.connected {
                        "daemon online".to_string()
                    } else {
                        "daemon offline".to_string()
                    })),
            )
    }
}

/// A header status chip: colored icon tile + label + value. `clickable`
/// adds a hover state (the Profile chip navigates to the Profiles page).
macro_rules! status_chip {
    ($cx:expr, $id:expr, $icon:expr, $icon_color:expr, $label:expr, $value:expr, $value_color:expr, clickable: $click:expr) => {{
        let __cx = $cx;
        let __base = h_flex()
            .id($id)
            .gap_3()
            .items_center()
            .px_4()
            .py_2()
            .min_w(px(150.))
            .rounded_lg()
            .border_1()
            .border_color(BORDER)
            .bg(SURFACE)
            .child(
                div()
                    .size(px(30.))
                    .rounded_md()
                    .bg($icon_color.opacity(0.22))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(svg_icon($icon, $icon_color, 16.)),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(TEXT_MUTED)
                            .child($label.to_string()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color($value_color)
                            .child($value),
                    ),
            );
        if $click {
            __base.hover(|this| this.bg(SURFACE_RAISED))
        } else {
            __base
        }
    }};
}

impl ThermiteApp {
    /// "System overview" title + machine subtitle, with the right-hand
    /// status chips (Profile / Power / Failsafe).
    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let (profile, power, phase, grace) = match &self.status {
            Some(s) => (
                s.telemetry.platform_profile.clone().unwrap_or("—".into()),
                match s.telemetry.on_ac {
                    Some(true) => "AC".to_string(),
                    Some(false) => "battery".to_string(),
                    None => "—".to_string(),
                },
                s.control.failsafe_phase,
                s.control.suspended_grace,
            ),
            None => ("—".into(), "—".into(), FailsafePhase::Normal, false),
        };
        h_flex()
            .justify_between()
            .items_start()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(TEXT_FAINT)
                            .child("SYSTEM OVERVIEW"),
                    )
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(TEXT)
                            .child(self.machine.product.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .text_color(TEXT_MUTED)
                            .child(self.machine.os.clone())
                            .child("·")
                            .child(format!("Thermite v{}", env!("CARGO_PKG_VERSION")))
                            .when(grace, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(WARN.opacity(0.15))
                                        .text_color(WARN)
                                        .child("post-resume grace"),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        status_chip!(
                            cx,
                            "chip-profile",
                            icons::LAYERS,
                            TEXT_MUTED,
                            "Profile",
                            profile,
                            TEXT,
                            clickable: true
                        )
                        .on_click(cx.listener(
                            |this, _ev: &ClickEvent, _window, cx| {
                                this.nav = Nav::Profiles;
                                cx.notify();
                            },
                        )),
                    )
                    .child(status_chip!(
                        cx,
                        "chip-power",
                        icons::ZAP,
                        TEXT,
                        "Power",
                        power,
                        TEXT,
                        clickable: false
                    ))
                    .child(status_chip!(
                        cx,
                        "chip-failsafe",
                        icons::SHIELD_CHECK,
                        match phase {
                            FailsafePhase::Normal => OK,
                            _ => ACCENT,
                        },
                        "Failsafe",
                        match phase {
                            FailsafePhase::Normal => "Normal".to_string(),
                            FailsafePhase::ForceFull => "Force full".to_string(),
                            FailsafePhase::ReleasedAuto => "Released to firmware".to_string(),
                        },
                        match phase {
                            FailsafePhase::Normal => OK,
                            _ => ACCENT,
                        },
                        clickable: false
                    )),
            )
    }

    /// One metric card: icon + title header, big current value, area chart
    /// with a fixed y-domain, grid and axis labels (mockup style). `ticks`
    /// is the number of labels (intervals = ticks - 1).
    fn render_metric_card(
        &self,
        id: &'static str,
        title: &str,
        icon: &'static str,
        icon_color: Hsla,
        value_color: Hsla,
        line_color: Hsla,
        unit: &'static str,
        data: &[Sample],
        value_fn: impl Fn(&Sample) -> f64 + 'static,
        y_domain: (f64, f64),
        ticks: usize,
        _cx: &Context<Self>,
    ) -> impl IntoElement {
        let mut owned: Vec<Sample> = data.to_vec();
        // Labels are window-relative: the oldest sample reads 0s.
        let t0 = owned.first().map(|d| d.t).unwrap_or(0.0);
        for d in &mut owned {
            d.t -= t0;
        }
        let last = owned.last().map(&value_fn).unwrap_or(0.0);
        let domain = y_domain;
        let intervals = (ticks.max(2) - 1) as f64;
        v_flex()
            .id(id)
            .flex_1()
            .gap_2()
            .border_1()
            .border_color(BORDER)
            .rounded_lg()
            .bg(SURFACE)
            .p_4()
            .min_h(px(190.))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(svg_icon(icon, icon_color, 16.))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(TEXT)
                                    .child(title.to_string()),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_baseline()
                            .gap_1()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(value_color)
                                    .child(format!("{last:.0}")),
                            )
                            .child(div().text_sm().text_color(TEXT_MUTED).child(unit)),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    // h_flex centers children vertically by default, which
                    // collapses the height-less AreaChart to zero — stretch
                    // instead so the plot fills the row.
                    .items_stretch()
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            AreaChart::new(owned)
                                .x(|d: &Sample| format!("{:.0}s", d.t))
                                .y(value_fn)
                                .y_domain(domain.0, domain.1)
                                // The library's default 10px top headroom
                                // skews the pixel-even grid off the domain
                                // steps; zero padding keeps grid lines on
                                // exact domain fractions.
                                .y_padding(0., 0.)
                                .x_tick_count(8)
                                .grid(true)
                                .grid_dashed(true)
                                .stroke(line_color)
                                .fill(linear_gradient(
                                    0.,
                                    linear_color_stop(line_color.opacity(0.22), 1.),
                                    linear_color_stop(hsla(0., 0., 0.075, 0.), 0.),
                                ))
                                .tick_margin(8),
                        ),
                    )
                    // Y labels on the right edge (mockup style): evenly
                    // spaced domain values, aligned to the plot by skipping
                    // the chart's 18px x-axis gutter at the bottom.
                    .child(
                        v_flex()
                            .justify_between()
                            .h_full()
                            .pb(px(18.))
                            .w(px(46.))
                            .children((0..ticks.max(2)).map(|i| {
                                let v = domain.1 - (domain.1 - domain.0) * i as f64 / intervals;
                                div()
                                    .text_size(px(10.))
                                    .text_right()
                                    .text_color(TEXT_MUTED)
                                    .child(format!("{v:.0}{unit}"))
                            })),
                    ),
            )
    }

    /// One row of the sensor table.
    fn sensor_row(label: &str, class: &'static str, temp: f32) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_1p5()
            .rounded_sm()
            .text_sm()
            .hover(|this| this.bg(SURFACE_RAISED))
            .child(
                div()
                    .flex_1()
                    .text_color(TEXT)
                    .truncate()
                    .child(format!("{label} ({class})")),
            )
            .child(
                div()
                    .w(px(120.))
                    .text_color(TEXT_MUTED)
                    .child(class.to_string()),
            )
            .child(
                div()
                    .w(px(80.))
                    .text_right()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if temp > 45.0 { ACCENT } else { TEXT })
                    .child(format!("{temp:.1}°C")),
            )
    }

    /// One sensors card: header + column headers + rows. Two of these sit
    /// side by side (hottest half left, cooler half right).
    fn render_sensor_card(title: &str, temps: &[thermite_core::TempTelemetry]) -> impl IntoElement {
        v_flex()
            .flex_1()
            .border_1()
            .border_color(BORDER)
            .rounded_lg()
            .bg(SURFACE)
            .pb_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .px_4()
                    .py_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(svg_icon(icons::THERMOMETER, ACCENT, 16.))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(TEXT)
                                    .child(title.to_string()),
                            ),
                    )
                    .child(svg_icon(icons::LIST_FILTER, TEXT_MUTED, 14.)),
            )
            .child(
                // Column headers
                h_flex()
                    .px_3()
                    .pb_1p5()
                    .mx_1()
                    .text_xs()
                    .text_color(TEXT_MUTED)
                    .border_b_1()
                    .border_color(BORDER)
                    .child(div().flex_1().child("Sensor"))
                    .child(div().w(px(120.)).child("Source"))
                    .child(div().w(px(80.)).text_right().child("Temp")),
            )
            .children(
                temps
                    .iter()
                    .map(|t| Self::sensor_row(&t.label, temp_class_str(t.class), t.temp_c)),
            )
    }

    fn render_dashboard(&self, cx: &Context<Self>) -> impl IntoElement {
        let samples: Vec<Sample> = self.samples.iter().cloned().collect();
        let mut temps: Vec<_> = self
            .status
            .as_ref()
            .map(|s| s.telemetry.temps.clone())
            .unwrap_or_default();
        temps.sort_by(|a, b| b.temp_c.total_cmp(&a.temp_c));

        v_flex()
            .p_5()
            .gap_5()
            .flex_1()
            .child(self.render_header(cx))
            // Both chart rows flex-grow to absorb spare vertical space;
            // items_stretch so the cards fill the row (h_flex would center
            // and collapse them otherwise).
            .child(
                h_flex()
                    .gap_4()
                    .flex_1()
                    .items_stretch()
                    .child(self.render_metric_card(
                        "chart-cpu-temp",
                        "CPU temperature",
                        icons::THERMOMETER,
                        ACCENT,
                        ACCENT,
                        ACCENT,
                        "°C",
                        &samples,
                        |d| d.cpu,
                        (0.0, 100.0),
                        5,
                        cx,
                    ))
                    .child(self.render_metric_card(
                        "chart-gpu-temp",
                        "GPU temperature",
                        icons::THERMOMETER,
                        ACCENT,
                        TEXT,
                        TEXT.opacity(0.9),
                        "°C",
                        &samples,
                        |d| d.gpu,
                        (0.0, 100.0),
                        5,
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .gap_4()
                    .flex_1()
                    .items_stretch()
                    .child(self.render_metric_card(
                        "chart-cpu-fan",
                        "CPU fan",
                        icons::FAN,
                        ACCENT,
                        TEXT,
                        TEXT.opacity(0.8),
                        "rpm",
                        &samples,
                        |d| d.cpu_rpm,
                        (0.0, 8000.0),
                        5,
                        cx,
                    ))
                    .child(self.render_metric_card(
                        "chart-gpu-fan",
                        "GPU fan",
                        icons::FAN,
                        ACCENT,
                        TEXT,
                        TEXT.opacity(0.8),
                        "rpm",
                        &samples,
                        |d| d.gpu_rpm,
                        (0.0, 8000.0),
                        5,
                        cx,
                    )),
            )
            .child(
                // Sensors: two cards side by side — hottest half on the left
                // so the ranking still reads naturally. 8 rows per card.
                h_flex()
                    .gap_4()
                    .items_stretch()
                    .child(Self::render_sensor_card(
                        "Sensors (hottest first)",
                        &temps[..temps.len().min(8)],
                    ))
                    .child(Self::render_sensor_card(
                        "Sensors (continued)",
                        &temps[temps.len().min(8)..temps.len().min(16)],
                    )),
            )
    }

    fn render_fan_card(&self, fan: FanId, cx: &Context<Self>) -> impl IntoElement {
        let name = fan.to_string();
        let rt = self.fan_state(fan);
        let hw = self.status.as_ref().and_then(|s| s.telemetry.fan(fan));
        let mode = rt.map(|r| r.config.mode).unwrap_or_default();
        let hw_rpm = hw.and_then(|h| h.rpm);
        let hw_label = hw.and_then(|h| h.hw_mode).map(hw_mode_str).unwrap_or("—");
        let temp = match fan {
            FanId::Cpu => self.status.as_ref().and_then(|s| s.telemetry.cpu_temp_c()),
            FanId::Gpu => self.status.as_ref().and_then(|s| s.telemetry.gpu_temp_c()),
        }
        .unwrap_or(0.0);
        let (title, sub) = match fan {
            FanId::Cpu => ("CPU Fan", "Controls the left fan (CPU)"),
            FanId::Gpu => ("GPU Fan", "Controls the right fan (GPU)"),
        };

        let curve_points = self.fan_points(fan);
        let curve_mode = mode == FanModeKind::Curve;

        // Content row: left control column, plus mode-specific right side.
        let left_share = if curve_mode { 0.5 } else { 0.4 };
        let mut content = h_flex().gap_4().items_stretch().child(
            v_flex()
                .w(relative(left_share))
                .flex_none()
                .min_w_0()
                .gap_3()
                .child(self.render_mode_buttons(fan, mode, cx))
                .child(match mode {
                    FanModeKind::Auto => self.render_auto_info(),
                    FanModeKind::Manual => self.render_manual_control(fan, cx),
                    FanModeKind::Curve => self.render_curve_graph(fan, &curve_points, temp, cx),
                }),
        );

        if curve_mode {
            content = content.child(self.render_curve_panel(fan, &curve_points, cx));
        } else {
            // Final color rule: a speed chart turns red while its fan is
            // spinning above 4000 rpm, white at or below — both fans alike.
            let (value_color, line_color) = if hw_rpm.is_some_and(|r| r > 4000) {
                (ACCENT, ACCENT.opacity(0.9))
            } else {
                (TEXT, TEXT.opacity(0.8))
            };
            let chart_id = match fan {
                FanId::Cpu => "rpm-chart-cpu",
                FanId::Gpu => "rpm-chart-gpu",
            };
            content = content.child(div().flex_1().min_w_0().child(self.render_metric_card(
                chart_id,
                &format!(
                    "Fan speed ({})",
                    if fan == FanId::Cpu { "CPU" } else { "GPU" }
                ),
                icons::CHART_LINE,
                TEXT_MUTED,
                value_color,
                line_color,
                "rpm",
                &self.samples.iter().cloned().collect::<Vec<Sample>>(),
                move |d: &Sample| match fan {
                    FanId::Cpu => d.cpu_rpm,
                    FanId::Gpu => d.gpu_rpm,
                },
                (0.0, 8000.0),
                5,
                cx,
            )));
        }

        v_flex()
            .id(SharedString::new(format!("fan-card-{name}")))
            .border_1()
            .border_color(BORDER)
            .rounded_lg()
            .bg(SURFACE)
            .p_5()
            .gap_4()
            // Header
            .child(
                h_flex()
                    .justify_between()
                    .items_start()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(match fan {
                                FanId::Cpu => {
                                    svg_icon(icons::SETTINGS, ACCENT, 20.).into_any_element()
                                }
                                FanId::Gpu => {
                                    svg_icon(icons::MONITOR, ACCENT, 20.).into_any_element()
                                }
                            })
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(TEXT)
                                            .child(title),
                                    )
                                    .child(div().text_xs().text_color(TEXT_MUTED).child(sub)),
                            ),
                    )
                    .child(
                        v_flex()
                            .items_end()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_2xl()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(TEXT)
                                            .child(
                                                hw_rpm
                                                    .map(|r| r.to_string())
                                                    .unwrap_or_else(|| "—".into()),
                                            ),
                                    )
                                    .child(div().text_sm().text_color(TEXT_MUTED).child("rpm"))
                                    .child(div().w(px(12.)))
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .child(svg_icon(icons::THERMOMETER, TEXT_MUTED, 13.))
                                            .child(
                                                div()
                                                    .text_base()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(if temp > 45.0 {
                                                        ACCENT
                                                    } else {
                                                        TEXT
                                                    })
                                                    .child(format!("{temp:.0}°C")),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(TEXT_MUTED)
                                    .child(format!("Hardware: {hw_label}")),
                            ),
                    ),
            )
            .child(content)
    }

    /// Segmented Auto / Manual / Curve mode selector.
    fn render_mode_buttons(
        &self,
        fan: FanId,
        mode: FanModeKind,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex().gap_2().children(
            [
                (FanModeKind::Auto, "Auto"),
                (FanModeKind::Manual, "Manual"),
                (FanModeKind::Curve, "Curve"),
            ]
            .map(|(kind, label)| {
                let active = mode == kind;
                div()
                    .id(SharedString::new(format!("mode-{}-{label}", fan)))
                    .px_5()
                    .py_2()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .rounded_md()
                    .border_1()
                    .when(active, |this| {
                        this.bg(ACCENT).border_color(ACCENT).text_color(TEXT)
                    })
                    .when(!active, |this| {
                        this.bg(BG)
                            .border_color(BORDER)
                            .text_color(TEXT_MUTED)
                            .hover(|this| this.bg(SURFACE_RAISED).text_color(TEXT))
                    })
                    .on_click(cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                        let config = match kind {
                            FanModeKind::Auto => FanConfig {
                                mode: FanModeKind::Auto,
                                curve: None,
                                manual_duty_pct: None,
                            },
                            FanModeKind::Manual => FanConfig {
                                mode: FanModeKind::Manual,
                                curve: None,
                                manual_duty_pct: Some(this.fan_duty(fan)),
                            },
                            FanModeKind::Curve => FanConfig {
                                mode: FanModeKind::Curve,
                                curve: Some(this.fan_curve_name(fan)),
                                manual_duty_pct: None,
                            },
                        };
                        this.manual_draft.remove(&fan.to_string());
                        this.send(GuiCommand::SetFan { fan, config });
                        cx.notify();
                    }))
                    .child(label)
            }),
        )
    }

    /// Explanatory box shown in Auto mode.
    fn render_auto_info(&self) -> Div {
        div()
            .flex_1()
            .rounded_md()
            .bg(BG)
            .border_1()
            .border_color(BORDER)
            .p_4()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .items_start()
                    .child(svg_icon(icons::INFO, TEXT_MUTED, 14.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(TEXT_MUTED)
                            .child(
                                "In auto mode, fan speed is controlled by the embedded                                  controller based on temperature and power limits.",
                            ),
                    ),
            )
    }

    /// Manual-mode control: duty label + big percentage + slider.
    fn render_manual_control(&self, fan: FanId, cx: &Context<Self>) -> Div {
        let duty = self.fan_duty(fan);
        v_flex()
            .flex_1()
            .gap_4()
            .child(
                h_flex()
                    .justify_between()
                    .items_end()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(TEXT)
                            .child("Fan speed (duty)"),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(ACCENT)
                            .child(format!("{duty}%")),
                    ),
            )
            .child(self.render_duty_slider(fan, duty, cx))
    }

    /// Custom duty slider: red fill, draggable thumb; commits on release.
    fn render_duty_slider(&self, fan: FanId, duty: u8, cx: &Context<Self>) -> Div {
        let name = fan.to_string();
        let bounds_cell: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
        let frac = f32::from(duty) / 100.0;

        let track = div()
            .relative()
            .flex_1()
            .h(px(6.))
            .rounded_full()
            .bg(SURFACE_RAISED)
            // invisible canvas: captures the track bounds for event math
            .child(
                canvas(
                    {
                        let bounds_cell = Rc::clone(&bounds_cell);
                        move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
                            *bounds_cell.borrow_mut() = Some(bounds);
                        }
                    },
                    |_: Bounds<Pixels>, _: (), _: &mut Window, _: &mut App| {},
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(relative(frac))
                    .rounded_full()
                    .bg(ACCENT),
            )
            .child(
                div()
                    .absolute()
                    .top(px(-4.))
                    .left(relative(frac))
                    .ml(px(-7.))
                    .size(px(14.))
                    .rounded_full()
                    .bg(ACCENT)
                    .border_2()
                    .border_color(BG),
            )
            .child(
                div()
                    .absolute()
                    .inset(px(-10.))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, {
                        let bounds_cell = Rc::clone(&bounds_cell);
                        let name = name.clone();
                        cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                            if let Some(duty) =
                                this.slider_duty_at(&name, *bounds_cell.borrow(), ev.position.x)
                            {
                                this.slider_drag = Some(name.clone());
                                this.manual_draft.insert(name.clone(), duty);
                                let due = this
                                    .last_slider_send
                                    .as_ref()
                                    .is_none_or(|(f, t)| f != &name || t.elapsed() >= THROTTLE);
                                if due {
                                    this.last_slider_send = Some((name.clone(), Instant::now()));
                                    this.send_slider_duty(fan);
                                }
                                cx.notify();
                            }
                        })
                    })
                    .on_mouse_move({
                        let bounds_cell = Rc::clone(&bounds_cell);
                        let name = name.clone();
                        cx.listener(move |this, ev: &MouseMoveEvent, _window, cx| {
                            if this.slider_drag.as_deref() != Some(name.as_str())
                                || ev.pressed_button != Some(MouseButton::Left)
                            {
                                return;
                            }
                            if let Some(duty) =
                                this.slider_duty_at(&name, *bounds_cell.borrow(), ev.position.x)
                            {
                                this.manual_draft.insert(name.clone(), duty);
                                let due = this
                                    .last_slider_send
                                    .as_ref()
                                    .is_none_or(|(f, t)| f != &name || t.elapsed() >= THROTTLE);
                                if due {
                                    this.last_slider_send = Some((name.clone(), Instant::now()));
                                    this.send_slider_duty(fan);
                                }
                                cx.notify();
                            }
                        })
                    })
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseUpEvent, _window, cx| {
                            if this.slider_drag.as_deref() != Some(name.as_str()) {
                                return;
                            }
                            this.slider_drag = None;
                            this.last_slider_send = None;
                            this.send_slider_duty(fan);
                            this.manual_draft.remove(&fan.to_string());
                            cx.notify();
                        }),
                    ),
            );

        h_flex()
            .gap_3()
            .items_center()
            .child(div().text_xs().text_color(TEXT_FAINT).child("0%"))
            .child(track)
            .child(div().text_xs().text_color(TEXT_FAINT).child("100%"))
    }

    /// Push the current slider duty to the daemon immediately (live
    /// preview while dragging; the control loop applies it within a tick).
    fn send_slider_duty(&mut self, fan: FanId) {
        let duty = self
            .manual_draft
            .get(&fan.to_string())
            .copied()
            .unwrap_or_else(|| self.fan_duty(fan));
        self.send(GuiCommand::SetFan {
            fan,
            config: FanConfig {
                mode: FanModeKind::Manual,
                curve: None,
                manual_duty_pct: Some(duty),
            },
        });
    }

    /// Map a slider pointer x to a duty percentage using the stashed
    /// track bounds.
    fn slider_duty_at(&self, _fan: &str, bounds: Option<Bounds<Pixels>>, x: Pixels) -> Option<u8> {
        let bounds = bounds?;
        if f32::from(bounds.size.width) <= 0.0 {
            return None;
        }
        let frac = (f32::from(x - bounds.origin.x) / f32::from(bounds.size.width)).clamp(0.0, 1.0);
        Some((frac * 100.0).round() as u8)
    }

    /// Interactive temperature-to-duty curve editor. Grid, fill, line,
    /// dots and the live operating marker are all painted in one canvas
    /// from the same points; the overlay div only handles drag events.
    fn render_curve_graph(
        &self,
        fan: FanId,
        points: &[CurvePoint],
        temp: f32,
        cx: &Context<Self>,
    ) -> Div {
        let name = fan.to_string();
        let bounds_cell: Rc<RefCell<Option<Bounds<Pixels>>>> = Rc::default();
        let pts: Vec<(f32, f32)> = points.iter().map(|p| (p.0, f32::from(p.1))).collect();
        let operating_duty = CurveDef::from_points(points.to_vec()).duty_at(temp);
        let drag_idx = self
            .drag_point
            .as_ref()
            .filter(|(f, _)| f == &name)
            .map(|(_, i)| *i);

        let paint_pts = pts.clone();
        let graph = canvas(
            {
                let bounds_cell = Rc::clone(&bounds_cell);
                move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
                    *bounds_cell.borrow_mut() = Some(bounds);
                }
            },
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, _: &mut App| {
                let h = f32::from(bounds.size.height);
                let w = f32::from(bounds.size.width);
                if h <= 0.0 || w <= 0.0 {
                    return;
                }
                let xy = move |t: f32, d: f32| {
                    point(
                        bounds.origin.x + bounds.size.width * (t / 100.0).clamp(0.0, 1.0),
                        bounds.origin.y + bounds.size.height * (1.0 - (d / 100.0).clamp(0.0, 1.0)),
                    )
                };
                // Grid: 5 horizontal (duty steps), 6 vertical (temp steps).
                for i in 0..=4 {
                    let y = bounds.origin.y + bounds.size.height * (i as f32 / 4.0);
                    window.paint_quad(fill(
                        Bounds::new(point(bounds.origin.x, y), size(bounds.size.width, px(1.))),
                        BORDER,
                    ));
                }
                for i in 0..=5 {
                    let x = bounds.origin.x + bounds.size.width * (i as f32 / 5.0);
                    window.paint_quad(fill(
                        Bounds::new(point(x, bounds.origin.y), size(px(1.), bounds.size.height)),
                        BORDER,
                    ));
                }
                if paint_pts.is_empty() {
                    return;
                }
                let (ft, fd) = paint_pts[0];
                let (lt, _ld) = *paint_pts.last().unwrap();
                // Area fill under the curve.
                let mut area = PathBuilder::fill();
                area.move_to(xy(ft, fd));
                for (t, d) in paint_pts.iter().skip(1) {
                    area.line_to(xy(*t, *d));
                }
                area.line_to(xy(lt, 0.0));
                area.line_to(xy(ft, 0.0));
                if let Ok(path) = area.build() {
                    window.paint_path(path, ACCENT.opacity(0.15));
                }
                // Curve line.
                let mut line = PathBuilder::stroke(px(2.));
                line.move_to(xy(ft, fd));
                for (t, d) in paint_pts.iter().skip(1) {
                    line.line_to(xy(*t, *d));
                }
                if let Ok(path) = line.build() {
                    window.paint_path(path, ACCENT);
                }
                // Points: dark halo + red core (bigger while dragged).
                for (i, (t, d)) in paint_pts.iter().enumerate() {
                    let c = xy(*t, *d);
                    let r = if drag_idx == Some(i) { 7.0 } else { 5.0 };
                    window.paint_quad(quad(
                        Bounds::new(
                            point(c.x - px(r + 2.0), c.y - px(r + 2.0)),
                            size(px((r + 2.0) * 2.0), px((r + 2.0) * 2.0)),
                        ),
                        px(r + 2.0),
                        BG,
                        px(0.),
                        BG,
                        BorderStyle::default(),
                    ));
                    window.paint_quad(quad(
                        Bounds::new(
                            point(c.x - px(r), c.y - px(r)),
                            size(px(r * 2.0), px(r * 2.0)),
                        ),
                        px(r),
                        ACCENT,
                        px(0.),
                        BG,
                        BorderStyle::default(),
                    ));
                }
                // Live operating point (white).
                let c = xy(temp, f32::from(operating_duty));
                window.paint_quad(quad(
                    Bounds::new(point(c.x - px(3.0), c.y - px(3.0)), size(px(6.0), px(6.0))),
                    px(3.0),
                    TEXT,
                    px(0.),
                    BG,
                    BorderStyle::default(),
                ));
            },
        )
        .absolute()
        .inset_0();

        let overlay = div()
            .absolute()
            .inset_0()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, {
                let bounds_cell = Rc::clone(&bounds_cell);
                let name = name.clone();
                cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                    let Some(bounds) = *bounds_cell.borrow() else {
                        return;
                    };
                    let pts = this.fan_points(fan);
                    let rel_x = f32::from(ev.position.x - bounds.origin.x);
                    let rel_y = f32::from(ev.position.y - bounds.origin.y);
                    let mut best: Option<(usize, f32)> = None;
                    for (i, p) in pts.iter().enumerate() {
                        let px_x = f32::from(bounds.size.width) * (p.0 / 100.0).clamp(0.0, 1.0);
                        let px_y = f32::from(bounds.size.height)
                            * (1.0 - (f32::from(p.1) / 100.0).clamp(0.0, 1.0));
                        let dist = ((rel_x - px_x).powi(2) + (rel_y - px_y).powi(2)).sqrt();
                        if dist <= 18.0 && best.map_or(true, |(_, d)| dist < d) {
                            best = Some((i, dist));
                        }
                    }
                    if let Some((i, _)) = best {
                        this.drag_point = Some((name.clone(), i));
                        cx.notify();
                    }
                })
            })
            .on_mouse_move({
                let bounds_cell = Rc::clone(&bounds_cell);
                let name = name.clone();
                cx.listener(move |this, ev: &MouseMoveEvent, _window, cx| {
                    let Some((f, idx)) = this.drag_point.clone() else {
                        return;
                    };
                    if f != name || ev.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    let Some(bounds) = *bounds_cell.borrow() else {
                        return;
                    };
                    if f32::from(bounds.size.width) <= 0.0 || f32::from(bounds.size.height) <= 0.0 {
                        return;
                    }
                    let mut pts = this.fan_points(fan);
                    let Some(_slot) = pts.get_mut(idx) else {
                        return;
                    };
                    let fx = ((f32::from(ev.position.x - bounds.origin.x)
                        / f32::from(bounds.size.width))
                        * 100.0)
                        .clamp(0.0, 100.0);
                    let fy = ((f32::from(ev.position.y - bounds.origin.y)
                        / f32::from(bounds.size.height))
                        * 100.0)
                        .clamp(0.0, 100.0);
                    // Strictly increasing temps against the neighbors.
                    let lo = if idx > 0 { pts[idx - 1].0 + 1.0 } else { 0.0 };
                    let hi = pts.get(idx + 1).map_or(100.0, |p| p.0 - 1.0);
                    if lo > hi {
                        return;
                    }
                    pts[idx] = CurvePoint(
                        fx.clamp(lo, hi),
                        (100.0 - fy).round().clamp(0.0, 100.0) as u8,
                    );
                    this.curve_draft.insert(name.clone(), pts);
                    cx.notify();
                })
            })
            .on_mouse_up(MouseButton::Left, {
                let name = name.clone();
                cx.listener(move |this, _ev: &MouseUpEvent, _window, cx| {
                    let Some((f, _)) = this.drag_point.clone() else {
                        return;
                    };
                    if f != name {
                        return;
                    }
                    let points = this.curve_draft.get(&name).cloned();
                    if let Some(points) = points {
                        this.commit_curve(fan, points);
                    }
                    this.drag_point = None;
                    cx.notify();
                })
            });

        let fan_label = if fan == FanId::Cpu { "CPU" } else { "GPU" };

        v_flex()
            .flex_1()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(svg_icon(icons::CHART_LINE, TEXT_MUTED, 14.).into_any_element())
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(TEXT)
                            .child(format!("Fan curve ({fan_label})")),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .h(px(240.))
                    .child(v_flex().justify_between().h_full().w(px(24.)).children(
                        [100, 75, 50, 25, 0].map(|v| {
                            div()
                                .text_size(px(10.))
                                .text_right()
                                .text_color(TEXT_FAINT)
                                .child(format!("{v}"))
                        }),
                    ))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h_full()
                            .child(graph)
                            .child(overlay),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .pl(px(24.))
                    .children([0, 20, 40, 60, 80, 100].map(|v| {
                        div()
                            .text_size(px(10.))
                            .text_color(TEXT_FAINT)
                            .child(format!("{v}"))
                    })),
            )
            .child(
                div()
                    .pl(px(24.))
                    .text_size(px(10.))
                    .text_color(TEXT_FAINT)
                    .child("Temperature (°C)"),
            )
    }

    /// Curve-mode right panel: presets, and the point list with full
    /// editing (temp/duty steppers, remove, add).
    fn render_curve_panel(&self, fan: FanId, points: &[CurvePoint], cx: &Context<Self>) -> Div {
        let name = fan.to_string();
        let active_curve = self.fan_curve_name(fan);
        let curves: Vec<String> = self.config.curves.keys().cloned().collect();

        v_flex()
            .flex_1()
            .min_w_0()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(TEXT_MUTED)
                    .child("Preset curves"),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .children(curves.iter().map(|cname| {
                        let active = *cname == active_curve;
                        div()
                            .id(SharedString::new(format!("preset-{name}-{cname}")))
                            .px_3()
                            .py_1p5()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .rounded_md()
                            .border_1()
                            .when(active, |this| {
                                this.bg(ACCENT_DIM).border_color(ACCENT).text_color(TEXT)
                            })
                            .when(!active, |this| {
                                this.bg(BG)
                                    .border_color(BORDER)
                                    .text_color(TEXT_MUTED)
                                    .hover(|this| this.bg(SURFACE_RAISED).text_color(TEXT))
                            })
                            .on_click(cx.listener({
                                let cname = cname.clone();
                                move |this, _ev: &ClickEvent, _window, cx| {
                                    this.curve_draft.remove(&fan.to_string());
                                    this.send(GuiCommand::SetFan {
                                        fan,
                                        config: FanConfig {
                                            mode: FanModeKind::Curve,
                                            curve: Some(cname.clone()),
                                            manual_duty_pct: None,
                                        },
                                    });
                                    cx.notify();
                                }
                            }))
                            .child(pretty_curve_name(cname))
                    })),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(TEXT_MUTED)
                    .child("Curve points — drag dots on the graph or edit here"),
            )
            .child(
                v_flex()
                    .id(SharedString::new(format!("points-{name}")))
                    .gap_1()
                    .children(points.iter().enumerate().map(|(i, p)| {
                        h_flex()
                            .gap_6()
                            .items_center()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .bg(BG)
                            .child(div().size(px(8.)).rounded_full().bg(ACCENT))
                            // temperature cluster: value + its steppers
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        div()
                                            .w(px(48.))
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(TEXT)
                                            .child(format!("{:.0}°C", p.0)),
                                    )
                                    .child(self.stepper(format!("pt-t-{name}-{i}"), "−", {
                                        let points = points.to_vec();
                                        cx.listener(move |this, _ev: &ClickEvent, _window, _cx| {
                                            this.adjust_point(fan, i, &points, -5.0, 0);
                                        })
                                    }))
                                    .child(self.stepper(format!("pt-t+{name}-{i}"), "+", {
                                        let points = points.to_vec();
                                        cx.listener(move |this, _ev: &ClickEvent, _window, _cx| {
                                            this.adjust_point(fan, i, &points, 5.0, 0);
                                        })
                                    })),
                            )
                            // duty cluster: value + its steppers
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        div()
                                            .w(px(44.))
                                            .text_sm()
                                            .text_color(TEXT_MUTED)
                                            .child(format!("{}%", p.1)),
                                    )
                                    .child(self.stepper(format!("pt-d-{name}-{i}"), "−", {
                                        let points = points.to_vec();
                                        cx.listener(move |this, _ev: &ClickEvent, _window, _cx| {
                                            this.adjust_point(fan, i, &points, 0.0, -5);
                                        })
                                    }))
                                    .child(self.stepper(format!("pt-d+{name}-{i}"), "+", {
                                        let points = points.to_vec();
                                        cx.listener(move |this, _ev: &ClickEvent, _window, _cx| {
                                            this.adjust_point(fan, i, &points, 0.0, 5);
                                        })
                                    })),
                            )
                            .child(div().flex_1())
                            .when(points.len() > 1, |this| {
                                this.child(
                                    div()
                                        .id(SharedString::new(format!("pt-del-{name}-{i}")))
                                        .px_1()
                                        .text_sm()
                                        .text_color(TEXT_FAINT)
                                        .hover(|this| this.text_color(ACCENT))
                                        .on_click(cx.listener({
                                            let points = points.to_vec();
                                            move |this, _ev: &ClickEvent, _window, cx| {
                                                let mut pts = points.clone();
                                                pts.remove(i);
                                                this.commit_curve(fan, pts);
                                                cx.notify();
                                            }
                                        }))
                                        .child("✕"),
                                )
                            })
                    })),
            )
            .child(
                div()
                    .id(SharedString::new(format!("pt-add-{name}")))
                    .flex()
                    .justify_center()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(BORDER)
                    .text_sm()
                    .text_color(TEXT_MUTED)
                    .hover(|this| this.bg(SURFACE_RAISED).text_color(TEXT))
                    .on_click(cx.listener({
                        let points = points.to_vec();
                        move |this, _ev: &ClickEvent, _window, cx| {
                            let mut pts = points.clone();
                            let last = pts.last().copied().unwrap_or(CurvePoint(40.0, 40));
                            let new_temp = if last.0 >= 99.0 {
                                let prev = pts
                                    .get(pts.len().saturating_sub(2))
                                    .map(|p| p.0)
                                    .unwrap_or(0.0);
                                (prev + last.0) / 2.0
                            } else {
                                last.0 + 10.0
                            };
                            pts.push(CurvePoint(new_temp, last.1));
                            this.commit_curve(fan, pts);
                            cx.notify();
                        }
                    }))
                    .child("+ Add point"),
            )
    }

    /// A small −/+ stepper button for the point list.
    fn stepper(
        &self,
        id: String,
        label: &str,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(id))
            .size(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .text_xs()
            .text_color(TEXT_MUTED)
            .hover(|this| this.bg(SURFACE_RAISED).text_color(TEXT))
            .on_click(on_click)
            .child(label.to_string())
    }

    /// Adjust one curve point (list steppers) and commit if still valid.
    fn adjust_point(&mut self, fan: FanId, idx: usize, points: &[CurvePoint], dt: f32, dd: i16) {
        let mut pts = points.to_vec();
        let lo = if idx > 0 { pts[idx - 1].0 + 1.0 } else { 0.0 };
        let hi = pts.get(idx + 1).map_or(100.0, |p| p.0 - 1.0);
        if lo > hi {
            return;
        }
        let Some(p) = pts.get_mut(idx) else {
            return;
        };
        p.0 = (p.0 + dt).clamp(lo, hi);
        p.1 = (i16::from(p.1) + dd).clamp(0, 100) as u8;
        self.commit_curve(fan, pts);
    }

    fn render_fans(&self, cx: &Context<Self>) -> impl IntoElement {
        let full_control = self
            .status
            .as_ref()
            .map(|s| s.daemon.capability.level == CapabilityLevel::Full)
            .unwrap_or(false);
        v_flex()
            .p_5()
            .gap_5()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(TEXT_FAINT)
                            .child("FAN CONTROL"),
                    )
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(TEXT)
                            .child("Fan Control"),
                    )
                    .child(div().text_sm().text_color(TEXT_MUTED).child(format!(
                        "Manage fan modes, custom curves and fan speeds for your {}",
                        self.machine.product
                    ))),
            )
            .when(!full_control, |this| {
                this.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(WARN.opacity(0.5))
                        .text_color(WARN)
                        .text_sm()
                        .child(
                            "Manual fan control unavailable at the current capability level — \
                             run `thermite doctor` to see how to enable it (kernel 7.3+ or the \
                             stopgap module).",
                        ),
                )
            })
            .children(FanId::all().map(|fan| self.render_fan_card(fan, cx)))
    }

    fn render_profiles(&self, cx: &Context<Self>) -> impl IntoElement {
        let (available, active) = match &self.status {
            Some(s) => (
                s.daemon.capability.platform_profiles.clone(),
                s.telemetry.platform_profile.clone().unwrap_or_default(),
            ),
            None => (Vec::new(), String::new()),
        };

        v_flex()
            .p_4()
            .gap_3()
            .flex_1()
            .when(available.is_empty(), |this| {
                this.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(WARN.opacity(0.5))
                        .text_color(WARN)
                        .text_sm()
                        .child(
                            "No platform profiles exposed by the firmware. Try `sudo modprobe \
                             acer_wmi predator_v4=1` — see `thermite doctor`.",
                        ),
                )
            })
            .children(available.iter().map(|name| {
                let is_active = *name == active;
                div()
                    .id(SharedString::new(format!("profile-{name}")))
                    .px_4()
                    .py_3()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_active { ACCENT } else { BORDER })
                    .when(is_active, |this| this.bg(ACCENT_DIM))
                    .hover(|this| this.bg(ACCENT_DIM))
                    .on_click({
                        let name = name.clone();
                        cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                            this.send(GuiCommand::SetProfile {
                                profile: name.clone(),
                            });
                            cx.notify();
                        })
                    })
                    .child(
                        h_flex()
                            .justify_between()
                            .child(div().text_base().child(name.clone()))
                            .when(is_active, |this| {
                                this.child(div().text_sm().text_color(ACCENT).child("active"))
                            }),
                    )
            }))
    }

    fn render_status_bar(&self) -> impl IntoElement {
        let cap = self
            .status
            .as_ref()
            .map(|s| s.daemon.capability.level)
            .unwrap_or(CapabilityLevel::None);
        let (toast_msg, toast_err) = self.toast.clone().unzip();
        let ok_color = TEXT_MUTED;
        let err_color = ACCENT;
        h_flex()
            .px_5()
            .gap_3()
            .h(px(36.))
            .flex_none()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(BORDER)
            .bg(BG)
            .text_xs()
            .text_color(TEXT_MUTED)
            .child(
                h_flex()
                    .gap_3()
                    .child(match cap {
                        CapabilityLevel::Full => "full fan control".to_string(),
                        CapabilityLevel::MonitorProfiles => "monitoring + profiles".to_string(),
                        CapabilityLevel::None => "no fan interface".to_string(),
                    })
                    .child("·")
                    .child(format!("{}", self.socket.display())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when_some(toast_msg, |this, msg| {
                        this.text_color(if toast_err.unwrap_or(false) {
                            err_color
                        } else {
                            ok_color
                        })
                        .child(div().size(px(6.)).rounded_full().bg(
                            if toast_err.unwrap_or(false) {
                                err_color
                            } else {
                                ok_color
                            },
                        ))
                        .child(msg)
                    }),
            )
    }

    fn render_alerts_panel(&self) -> impl IntoElement {
        let recent: Vec<Alert> = self.alerts.iter().rev().take(3).cloned().collect();
        if recent.is_empty() {
            return v_flex().into_any_element();
        }
        v_flex()
            .px_5()
            .pb_2()
            .gap_1()
            .children(recent.iter().map(|a| {
                let color = if a.level == AlertLevel::Critical {
                    ACCENT
                } else {
                    WARN
                };
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(color)
                    .child(svg_icon(icons::SHIELD_CHECK, color, 12.))
                    .child(format!("{:?}: {}", a.kind, a.message))
            }))
            .into_any_element()
    }
}

/// "quiet-battery" -> "Quiet (Battery)", "balanced" -> "Balanced".
fn pretty_curve_name(name: &str) -> String {
    fn cap(s: &str) -> String {
        let mut c = s.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    }
    match name.strip_suffix("-battery") {
        Some(base) => format!("{} (Battery)", cap(base)),
        None => cap(name),
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

fn hw_mode_str(m: HwFanMode) -> &'static str {
    match m {
        HwFanMode::Auto => "auto",
        HwFanMode::Custom => "custom",
        HwFanMode::Turbo => "turbo",
    }
}

impl Render for ThermiteApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(TitleBar::new())
            .bg(BG)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_nav(cx))
                    .child(
                        div()
                            .id("content")
                            .flex_1()
                            .overflow_y_scroll()
                            .map(|this| match self.nav {
                                Nav::Dashboard => this.child(self.render_dashboard(cx)),
                                Nav::Fans => this.child(self.render_fans(cx)),
                                Nav::Profiles => this.child(self.render_profiles(cx)),
                            }),
                    ),
            )
            .child(self.render_alerts_panel())
            .child(self.render_status_bar())
    }
}

/// Reader thread: maintains one subscribed connection; reconnects with
/// backoff. On (re)connect it pushes full status+config snapshots, then
/// streams events (upgrading ticks with the last-known daemon info so the
/// UI keeps capability/failsafe context).
fn spawn_reader_thread(socket: PathBuf, tx: Sender<IpcEvent>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut up = false;
        let mark = |up: bool, tx: &Sender<IpcEvent>| {
            tx.send(if up {
                IpcEvent::Connected
            } else {
                IpcEvent::Disconnected
            })
            .ok();
        };
        loop {
            if let Ok(mut client) = Client::connect(&socket, "thermite-gui") {
                if !up {
                    mark(true, &tx);
                    up = true;
                }
                let mut daemon = DaemonInfo {
                    version: String::new(),
                    pid: 0,
                    uptime_ms: 0,
                    dry_run: false,
                    capability: Default::default(),
                };
                if let Ok(Response::Status { status }) = client.call(Request::GetStatus) {
                    daemon = status.daemon.clone();
                    tx.send(IpcEvent::Tick(Box::new(status))).ok();
                }
                if let Ok(Response::Config { config }) = client.call(Request::GetConfig) {
                    tx.send(IpcEvent::Config(Box::new(config))).ok();
                }
                if client.subscribe().is_ok() {
                    loop {
                        match client.read_event() {
                            Ok(Event::Tick { telemetry, control }) => {
                                daemon.uptime_ms = daemon.uptime_ms.saturating_add(1000);
                                tx.send(IpcEvent::Tick(Box::new(StatusSnapshot {
                                    telemetry,
                                    control,
                                    daemon: daemon.clone(),
                                    recent_alerts: Vec::new(),
                                })))
                                .ok();
                            }
                            Ok(Event::Alert(a)) => {
                                tx.send(IpcEvent::Alert(a)).ok();
                            }
                            Ok(Event::ConfigChanged { config }) => {
                                tx.send(IpcEvent::Config(Box::new(config))).ok();
                            }
                            Err(_) => break,
                        }
                    }
                }
                if up {
                    mark(false, &tx);
                    up = false;
                }
            } else if up {
                mark(false, &tx);
                up = false;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    })
}

/// Command thread: dedicated lazily-connected client for request/response.
fn spawn_command_thread(
    socket: PathBuf,
    tx: Sender<IpcEvent>,
    rx: mpsc::Receiver<GuiCommand>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut client: Option<Client> = None;
        while let Ok(cmd) = rx.recv() {
            if client.is_none() {
                client = Client::connect(&socket, "thermite-gui").ok();
            }
            let Some(c) = client.as_mut() else {
                tx.send(IpcEvent::CommandDone(Err("daemon unreachable".into())))
                    .ok();
                continue;
            };
            let request = match cmd {
                GuiCommand::SetFan { fan, config } => Request::SetFanConfig { fan, config },
                GuiCommand::SetProfile { profile } => Request::SetPlatformProfile { profile },
                GuiCommand::PutCurve { name, points } => Request::PutCurve { name, points },
            };
            let outcome = match c.call(request) {
                Ok(Response::Ok) => Ok(()),
                Ok(Response::Error { message }) => Err(message),
                Ok(other) => Err(format!("unexpected response: {other:?}")),
                Err(e) => Err(e.to_string()),
            };
            if outcome.is_err() {
                client = None; // possibly stale; reconnect next command
            }
            tx.send(IpcEvent::CommandDone(outcome)).ok();
        }
    })
}

fn main() {
    let socket = std::env::var("THERMITE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET_PATH));

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1024.), px(720.)), cx)),
            ..TitleBar::window_options()
        };

        let socket_for_window = socket.clone();
        #[allow(clippy::expect_used)]
        gpui_kit::open_window(window_options, cx, move |window, cx| {
            window.activate_window();
            window.set_window_title("Thermite");
            Theme::change(ThemeMode::Dark, Some(window), cx);
            cx.new(|cx| ThermiteApp::new(socket_for_window.clone(), window, cx))
        })
        .expect("failed to open window");
    });
}
