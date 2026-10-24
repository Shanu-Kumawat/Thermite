//! # thermite-gui
//!
//! Desktop client for the thermite daemon, built on GPUI (via gpui-kit).
//! Two connections to the daemon: one subscribed to events (reader thread),
//! one for request/response (command thread) — this avoids async plumbing
//! around the blocking IPC client.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::{h_flex, v_flex, Icon, Theme, ThemeMode, TitleBar};
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
                    l.strip_prefix("PRETTY_NAME=").map(|v| {
                        v.trim_matches('"').trim().to_string()
                    })
                })
            })
            .unwrap_or_else(|| "Linux".into());
        Self { product, os }
    }
}

/// Samples kept for charts (at 1 Hz ≈ 4 minutes).
const MAX_SAMPLES: usize = 240;

/// Monochrome design tokens: deep black background, charcoal surfaces,
/// subtle gray borders, white typography, restrained red accent.
mod ui {
    use gpui_kit::{hsla, Hsla};

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
use ui::{ACCENT, ACCENT_DIM, BG, BORDER, OK, SURFACE, SURFACE_RAISED, TEXT, TEXT_FAINT, TEXT_MUTED, WARN};

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
    Curves,
    Profiles,
}

impl Nav {
    fn label(self) -> &'static str {
        match self {
            Nav::Dashboard => "Dashboard",
            Nav::Fans => "Fans",
            Nav::Curves => "Curves",
            Nav::Profiles => "Profiles",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Nav::Dashboard => icons::HOUSE,
            Nav::Fans => icons::FAN,
            Nav::Curves => icons::CHART_LINE,
            Nav::Profiles => icons::LAYOUT_GRID,
        }
    }
    fn all() -> [Nav; 4] {
        [Nav::Dashboard, Nav::Fans, Nav::Curves, Nav::Profiles]
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
    selected_curve: String,
    curve_edit: Option<Vec<CurvePoint>>,
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
            selected_curve: "balanced".to_string(),
            curve_edit: None,
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
                    t: status.daemon.uptime_ms as f64 / 1000.0,
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
                if !self.config.curves.contains_key(&self.selected_curve) {
                    self.selected_curve = self
                        .config
                        .curves
                        .keys()
                        .next()
                        .cloned()
                        .unwrap_or_default();
                }
                self.curve_edit = None;
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

    // ---------- curve editing ----------

    fn editing_points(&self) -> Option<Vec<CurvePoint>> {
        self.curve_edit.clone().or_else(|| {
            self.config
                .curves
                .get(&self.selected_curve)
                .map(|c| c.points.clone())
        })
    }

    fn adjust_curve_point(&mut self, index: usize, dt: f32, dd: i16) {
        if let Some(mut points) = self.editing_points()
            && let Some(p) = points.get_mut(index)
        {
            p.0 = (p.0 + dt).clamp(20.0, 110.0);
            p.1 = (i16::from(p.1) + dd).clamp(0, 100) as u8;
            self.curve_edit = Some(points);
        }
    }

    fn delete_curve_point(&mut self, index: usize) {
        if let Some(mut points) = self.editing_points()
            && points.len() > 1
        {
            points.remove(index);
            self.curve_edit = Some(points);
        }
    }

    fn add_curve_point(&mut self) {
        if let Some(mut points) = self.editing_points() {
            let next = (points.last().map(|p| p.0).unwrap_or(40.0) + 10.0).min(105.0);
            let duty = points.last().map(|p| p.1).unwrap_or(50);
            points.push(CurvePoint(next, duty));
            self.curve_edit = Some(points);
        }
    }

    fn save_curve(&mut self) {
        let name = self.selected_curve.clone();
        let Some(points) = self.curve_edit.clone() else {
            return;
        };
        if CurveDef::validated(points.clone()).is_err() {
            self.toast = Some((
                "curve invalid: temps must be strictly increasing, duties 0-100".to_string(),
                true,
            ));
            return;
        }
        self.send(GuiCommand::PutCurve { name, points });
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
                    .when(active, |this| {
                        this.bg(ACCENT_DIM)
                            .text_color(TEXT)
                    })
                    .when(!active, |this| {
                        this.text_color(TEXT_MUTED)
                            .hover(|this| {
                                this.bg(SURFACE_RAISED)
                                    .text_color(TEXT)
                            })
                    })
                    .on_click(cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                        this.nav = nav;
                        cx.notify();
                    }))
                    .child(svg_icon(nav.icon(), if active {
                        TEXT
                    } else {
                        TEXT_MUTED
                    }, 16.))
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
                    .when(self.connected, |this| {
                        this.bg(SURFACE)
                            .text_color(OK)
                    })
                    .when(!self.connected, |this| {
                        this.bg(SURFACE)
                            .text_color(ACCENT)
                    })
                    .child(
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(if self.connected {
                                OK
                            } else {
                                ACCENT
                            }),
                    )
                    .child(
                        div().text_sm().child(if self.connected {
                            "daemon online".to_string()
                        } else {
                            "daemon offline".to_string()
                        }),
                    ),
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
                        .on_click(cx.listener(|this, _ev: &ClickEvent, _window, cx| {
                            this.nav = Nav::Profiles;
                            cx.notify();
                        })),
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
        let owned: Vec<Sample> = data.to_vec();
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
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(TEXT_MUTED)
                                    .child(unit),
                            ),
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
                                let v =
                                    domain.1 - (domain.1 - domain.0) * i as f64 / intervals;
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
            .children(temps.iter().map(|t| {
                Self::sensor_row(&t.label, temp_class_str(t.class), t.temp_c)
            }))
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
        let rt = self.fan_state(fan);
        let hw = self.status.as_ref().and_then(|s| s.telemetry.fan(fan));
        let mode = rt.map(|r| r.config.mode).unwrap_or_default();
        let manual_duty = rt
            .and_then(|r| r.config.manual_duty_pct)
            .unwrap_or(40)
            .clamp(0, 100);
        let curve_name = rt
            .and_then(|r| r.config.curve.clone())
            .unwrap_or_else(|| "balanced".to_string());
        let fan_label = fan.to_string();

        v_flex()
            .flex_1()
            .gap_3()
            .border_1()
            .border_color(BORDER)
            .rounded_md()
            .p_4()
            .child(
                h_flex()
                    .justify_between()
                    .child(div().text_base().child(format!("{fan_label} fan")))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(TEXT_MUTED)
                                    .child(format!(
                                        "{} rpm",
                                        hw.and_then(|h| h.rpm)
                                            .map(|r| r.to_string())
                                            .unwrap_or_else(|| "—".into())
                                    )),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(TEXT_MUTED)
                                    .child(format!(
                                        "hw {}",
                                        hw.and_then(|h| h.hw_mode).map(hw_mode_str).unwrap_or("—")
                                    )),
                            ),
                    ),
            )
            .child(
                h_flex().gap_2().children(
                    [
                        (FanModeKind::Auto, "Auto", mode == FanModeKind::Auto),
                        (FanModeKind::Manual, "Manual", mode == FanModeKind::Manual),
                        (FanModeKind::Curve, "Curve", mode == FanModeKind::Curve),
                    ]
                    .map(|(kind, label, active)| {
                        div()
                            .id(SharedString::new(format!("{fan_label}-mode-{label}")))
                            .px_4()
                            .py_1()
                            .text_sm()
                            .rounded_md()
                            .border_1()
                            .border_color(BORDER)
                            .when(active, |this| {
                                this.bg(ACCENT)
                                    .text_color(TEXT)
                            })
                            .when(!active, |this| {
                                this.text_color(TEXT)
                                    .hover(|this| this.bg(ACCENT_DIM))
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
                                        manual_duty_pct: Some(
                                            this.fan_state(fan)
                                                .and_then(|r| r.config.manual_duty_pct)
                                                .unwrap_or(40),
                                        ),
                                    },
                                    FanModeKind::Curve => FanConfig {
                                        mode: FanModeKind::Curve,
                                        curve: Some(this.default_curve_name()),
                                        manual_duty_pct: None,
                                    },
                                };
                                this.send(GuiCommand::SetFan { fan, config });
                                cx.notify();
                            }))
                            .child(label.to_string())
                    }),
                ),
            )
            .when(mode == FanModeKind::Manual, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(TEXT_MUTED)
                                .child("duty"),
                        )
                        .children([("−5", -5i16), ("+5", 5), ("−15", -15), ("+15", 15)].map(
                            |(sym, delta)| {
                                div()
                                    .id(SharedString::new(format!("{fan_label}-duty-{sym}")))
                                    .px_3()
                                    .py_1()
                                    .text_sm()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(BORDER)
                                    .text_color(TEXT)
                                    .hover(|this| this.bg(ACCENT_DIM))
                                    .on_click(cx.listener(
                                        move |this, _ev: &ClickEvent, _window, cx| {
                                            let current = this
                                                .fan_state(fan)
                                                .and_then(|r| r.config.manual_duty_pct)
                                                .unwrap_or(40);
                                            let next =
                                                (i16::from(current) + delta).clamp(0, 100) as u8;
                                            this.send(GuiCommand::SetFan {
                                                fan,
                                                config: FanConfig {
                                                    mode: FanModeKind::Manual,
                                                    curve: None,
                                                    manual_duty_pct: Some(next),
                                                },
                                            });
                                            cx.notify();
                                        },
                                    ))
                                    .child(sym.to_string())
                            },
                        ))
                        .child(
                            div()
                                .text_sm()
                                .text_color(ACCENT)
                                .child(format!("{manual_duty}%")),
                        ),
                )
            })
            .when(mode == FanModeKind::Curve, |this| {
                this.child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .children(self.config.curves.keys().map(|name| {
                            let active = *name == curve_name;
                            div()
                                .id(SharedString::new(format!("{fan_label}-curve-{name}")))
                                .px_3()
                                .py_1()
                                .text_sm()
                                .rounded_md()
                                .border_1()
                                .border_color(BORDER)
                                .when(active, |this| {
                                    this.bg(ACCENT)
                                        .text_color(TEXT)
                                })
                                .when(!active, |this| {
                                    this.text_color(TEXT)
                                        .hover(|this| this.bg(ACCENT_DIM))
                                })
                                .on_click({
                                    let name = name.clone();
                                    cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                                        this.send(GuiCommand::SetFan {
                                            fan,
                                            config: FanConfig {
                                                mode: FanModeKind::Curve,
                                                curve: Some(name.clone()),
                                                manual_duty_pct: None,
                                            },
                                        });
                                        cx.notify();
                                    })
                                })
                                .child(name.clone())
                        })),
                )
            })
    }

    fn render_fans(&self, cx: &Context<Self>) -> impl IntoElement {
        let full_control = self
            .status
            .as_ref()
            .map(|s| s.daemon.capability.level == CapabilityLevel::Full)
            .unwrap_or(false);
        v_flex()
            .p_4()
            .gap_4()
            .flex_1()
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
            .child(
                h_flex()
                    .gap_4()
                    .flex_none()
                    .child(self.render_fan_card(FanId::Cpu, cx)),
            )
            .child(
                h_flex()
                    .gap_4()
                    .flex_none()
                    .child(self.render_fan_card(FanId::Gpu, cx)),
            )
    }

    fn render_curves(&self, cx: &Context<Self>) -> impl IntoElement {
        let names: Vec<String> = self.config.curves.keys().cloned().collect();
        let selected = self.selected_curve.clone();
        let points = self.editing_points();

        v_flex()
            .p_4()
            .gap_4()
            .flex_1()
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .children(names.iter().map(|name| {
                        let active = *name == selected;
                        div()
                            .id(SharedString::new(format!("curve-tab-{name}")))
                            .px_3()
                            .py_1()
                            .text_sm()
                            .rounded_md()
                            .border_1()
                            .border_color(BORDER)
                            .when(active, |this| {
                                this.bg(ACCENT)
                                    .text_color(TEXT)
                            })
                            .when(!active, |this| {
                                this.text_color(TEXT)
                                    .hover(|this| this.bg(ACCENT_DIM))
                            })
                            .on_click({
                                let name = name.clone();
                                cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                                    this.selected_curve = name.clone();
                                    this.curve_edit = None;
                                    cx.notify();
                                })
                            })
                            .child(name.clone())
                    })),
            )
            .when_some(points, |this, points| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .text_color(TEXT_MUTED)
                                .child(format!(
                                    "{selected} — adjust points, then Save (temps strictly \
                                     increasing, duty 0-100)"
                                )),
                        )
                        .children(points.iter().enumerate().map(|(i, p)| {
                            h_flex()
                                .gap_3()
                                .items_center()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .hover(|this| this.bg(ACCENT_DIM))
                                .child(
                                    div()
                                        .w(px(64.))
                                        .text_sm()
                                        .text_color(TEXT_MUTED)
                                        .child(format!("{:.0}°C", p.0)),
                                )
                                .child(
                                    div()
                                        .w(px(64.))
                                        .text_sm()
                                        .text_color(ACCENT)
                                        .child(format!("{}%", p.1)),
                                )
                                .child(
                                    h_flex().gap_1().children(
                                        [
                                            ("temp −5", -5.0f32, 0i16),
                                            ("temp +5", 5.0, 0),
                                            ("duty −5", 0.0, -5),
                                            ("duty +5", 0.0, 5),
                                        ]
                                        .map(
                                            |(sym, dt, dd)| {
                                                div()
                                                .id(SharedString::new(format!(
                                                    "curve-pt-{i}-{sym}"
                                                )))
                                                .px_2()
                                                .py_1()
                                                .text_xs()
                                                .rounded_sm()
                                                .border_1()
                                                .border_color(BORDER)
                                                .hover(|this| {
                                                    this.bg(ACCENT_DIM)
                                                })
                                                .on_click(cx.listener(
                                                    move |this, _ev: &ClickEvent, _window, cx| {
                                                        this.adjust_curve_point(i, dt, dd);
                                                        cx.notify();
                                                    },
                                                ))
                                                .child(sym.to_string())
                                            },
                                        ),
                                    ),
                                )
                                .when(points.len() > 1, |this| {
                                    this.child(
                                        div()
                                            .id(SharedString::new(format!("curve-del-{i}")))
                                            .px_2()
                                            .py_1()
                                            .text_xs()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(BORDER)
                                            .text_color(ACCENT)
                                            .hover(|this| this.bg(SURFACE))
                                            .on_click(cx.listener(
                                                move |this, _ev: &ClickEvent, _window, cx| {
                                                    this.delete_curve_point(i);
                                                    cx.notify();
                                                },
                                            ))
                                            .child("remove"),
                                    )
                                })
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .id("curve-add-point")
                                        .px_3()
                                        .py_1()
                                        .text_sm()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(BORDER)
                                        .hover(|this| this.bg(ACCENT_DIM))
                                        .on_click(cx.listener(
                                            |this, _ev: &ClickEvent, _window, cx| {
                                                this.add_curve_point();
                                                cx.notify();
                                            },
                                        ))
                                        .child("add point"),
                                )
                                .child(
                                    div()
                                        .id("curve-save")
                                        .px_3()
                                        .py_1()
                                        .text_sm()
                                        .rounded_md()
                                        .bg(ACCENT)
                                        .text_color(TEXT)
                                        .hover(|this| this.opacity(0.9))
                                        .on_click(cx.listener(
                                            |this, _ev: &ClickEvent, _window, cx| {
                                                this.save_curve();
                                                cx.notify();
                                            },
                                        ))
                                        .child("save"),
                                ),
                        ),
                )
            })
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
                    .border_color(if is_active {
                        ACCENT
                    } else {
                        BORDER
                    })
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
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(ACCENT)
                                        .child("active"),
                                )
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
            .child(h_flex().gap_2().items_center().when_some(
                toast_msg,
                |this, msg| {
                    this.text_color(if toast_err.unwrap_or(false) {
                        err_color
                    } else {
                        ok_color
                    })
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(if toast_err.unwrap_or(false) {
                                err_color
                            } else {
                                ok_color
                            }),
                    )
                    .child(msg)
                },
            ))
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
                                Nav::Curves => this.child(self.render_curves(cx)),
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
