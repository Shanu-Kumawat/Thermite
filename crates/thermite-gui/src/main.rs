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
use gpui_kit::component::{ActiveTheme, Theme, ThemeMode, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use thermite_core::config::{Config, FanConfig, FanModeKind};
use thermite_core::{CapabilityLevel, CurveDef, CurvePoint, FailsafePhase, FanId, HwFanMode};
use thermite_proto::{
    Alert, AlertLevel, Client, DEFAULT_SOCKET_PATH, DaemonInfo, Event, Request, Response,
    StatusSnapshot,
};

/// Samples kept for charts (at 1 Hz ≈ 4 minutes).
const MAX_SAMPLES: usize = 240;

/// CPU/GPU/RPM palette.
const CPU_COLOR: Hsla = hsla(0.066, 0.9, 0.6, 1.0); // orange
const GPU_COLOR: Hsla = hsla(0.75, 0.6, 0.68, 1.0); // violet
const CPU_RPM_COLOR: Hsla = hsla(0.52, 0.85, 0.62, 1.0); // cyan
const GPU_RPM_COLOR: Hsla = hsla(0.52, 0.7, 0.5, 1.0); // dimmer cyan

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
    Tick(StatusSnapshot),
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
    fn all() -> [Nav; 4] {
        [Nav::Dashboard, Nav::Fans, Nav::Curves, Nav::Profiles]
    }
}

/// Root UI entity: owns connection state, latest snapshots, chart history
/// and the command channel.
pub struct ThermiteApp {
    socket: PathBuf,
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
            IpcEvent::Connected => {
                self.connected = true;
                self.toast = Some(("daemon connected".to_string(), false));
            }
            IpcEvent::Disconnected => {
                self.connected = false;
                self.status = None;
                self.toast = Some((
                    "daemon lost — is thermite.service running?".to_string(),
                    true,
                ));
            }
            IpcEvent::Tick(status) => {
                let telemetry = &status.telemetry;
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
                self.status = Some(status);
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
            .w(px(180.))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .pt_4()
            .gap_1()
            .children(Nav::all().map(|nav| {
                let active = self.nav == nav;
                let label = nav.label();
                div()
                    .id(SharedString::new(format!("nav-{label}")))
                    .px_4()
                    .py_2()
                    .mx_2()
                    .text_sm()
                    .rounded_sm()
                    .when(active, |this| {
                        this.bg(cx.theme().accent)
                            .text_color(cx.theme().accent_foreground)
                    })
                    .when(!active, |this| {
                        this.text_color(cx.theme().foreground)
                            .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
                    })
                    .on_click(cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                        this.nav = nav;
                        cx.notify();
                    }))
                    .child(label.to_string())
            }))
            .child(
                div()
                    .px_4()
                    .py_2()
                    .mx_2()
                    .mt_4()
                    .text_xs()
                    .text_color(if self.connected {
                        cx.theme().green
                    } else {
                        cx.theme().red
                    })
                    .child(if self.connected {
                        "● daemon online"
                    } else {
                        "○ daemon offline"
                    }),
            )
    }
}

/// Small label+value chip; needs the render context, hence a macro.
macro_rules! chip {
    ($cx:expr, $label:expr, $value:expr, $color:expr) => {{
        let __cx = $cx;
        h_flex()
            .gap_2()
            .items_baseline()
            .child(
                div()
                    .text_xs()
                    .text_color(__cx.theme().muted_foreground)
                    .child($label.to_string()),
            )
            .child(div().text_sm().text_color($color).child($value))
    }};
}
impl ThermiteApp {
    fn render_headline(&self, cx: &Context<Self>) -> impl IntoElement {
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
            .gap_6()
            .px_2()
            .py_1()
            .items_center()
            .child(chip!(cx, "profile", profile, cx.theme().accent))
            .child(chip!(cx, "power", power, cx.theme().blue))
            .child(chip!(
                cx,
                "failsafe",
                match phase {
                    FailsafePhase::Normal => "normal".to_string(),
                    FailsafePhase::ForceFull => "FORCE FULL".to_string(),
                    FailsafePhase::ReleasedAuto => "RELEASED TO FIRMWARE".to_string(),
                },
                match phase {
                    FailsafePhase::Normal => cx.theme().green,
                    _ => cx.theme().red,
                }
            ))
            .when(grace, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().yellow)
                        .child("post-resume grace"),
                )
            })
    }

    fn render_chart(
        &self,
        title: &str,
        unit: &str,
        data: &[Sample],
        value_fn: impl Fn(&Sample) -> f64 + 'static,
        color: Hsla,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let owned: Vec<Sample> = data.to_vec();
        let last = owned.last().map(&value_fn).unwrap_or(0.0);
        v_flex()
            .min_h(px(150.))
            .flex_1()
            .gap_1()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .p_2()
            .child(
                h_flex()
                    .justify_between()
                    .px_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(title.to_string()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(color)
                            .child(format!("{last:.0}{unit}")),
                    ),
            )
            .child(
                AreaChart::new(owned)
                    .x(|d: &Sample| format!("{:.0}s", d.t))
                    .y(value_fn)
                    .stroke(color)
                    .fill(linear_gradient(
                        0.,
                        linear_color_stop(color.opacity(0.35), 1.),
                        linear_color_stop(cx.theme().background.opacity(0.05), 0.),
                    ))
                    .tick_margin(12),
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
            .p_4()
            .gap_4()
            .flex_1()
            .child(self.render_headline(cx))
            .child(
                h_flex()
                    .gap_4()
                    .flex_none()
                    .child(self.render_chart(
                        "CPU temperature",
                        "°C",
                        &samples,
                        |d| d.cpu,
                        CPU_COLOR,
                        cx,
                    ))
                    .child(self.render_chart(
                        "GPU temperature",
                        "°C",
                        &samples,
                        |d| d.gpu,
                        GPU_COLOR,
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .gap_4()
                    .flex_none()
                    .child(self.render_chart(
                        "CPU fan",
                        "rpm",
                        &samples,
                        |d| d.cpu_rpm,
                        CPU_RPM_COLOR,
                        cx,
                    ))
                    .child(self.render_chart(
                        "GPU fan",
                        "rpm",
                        &samples,
                        |d| d.gpu_rpm,
                        GPU_RPM_COLOR,
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("sensors (hottest first)"),
                    )
                    .children(temps.iter().take(10).map(|t| {
                        h_flex()
                            .justify_between()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .hover(|this| this.bg(cx.theme().accent.opacity(0.06)))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(format!("{} ({})", t.label, temp_class_str(t.class))),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if t.temp_c > 85.0 {
                                        cx.theme().red
                                    } else if t.temp_c > 70.0 {
                                        cx.theme().yellow
                                    } else {
                                        cx.theme().green
                                    })
                                    .child(format!("{:.1}°C", t.temp_c)),
                            )
                    })),
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
            .border_color(cx.theme().border)
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
                                    .text_color(cx.theme().muted_foreground)
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
                                    .text_color(cx.theme().muted_foreground)
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
                            .border_color(cx.theme().border)
                            .when(active, |this| {
                                this.bg(cx.theme().accent)
                                    .text_color(cx.theme().accent_foreground)
                            })
                            .when(!active, |this| {
                                this.text_color(cx.theme().foreground)
                                    .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
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
                                .text_color(cx.theme().muted_foreground)
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
                                    .border_color(cx.theme().border)
                                    .text_color(cx.theme().foreground)
                                    .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
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
                                    .child(format!("{sym}"))
                            },
                        ))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().accent)
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
                                .border_color(cx.theme().border)
                                .when(active, |this| {
                                    this.bg(cx.theme().accent)
                                        .text_color(cx.theme().accent_foreground)
                                })
                                .when(!active, |this| {
                                    this.text_color(cx.theme().foreground)
                                        .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
                                })
                                .on_click({
                                    let fan = fan;
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
                        .border_color(cx.theme().yellow.opacity(0.5))
                        .text_color(cx.theme().yellow)
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
                            .border_color(cx.theme().border)
                            .when(active, |this| {
                                this.bg(cx.theme().accent)
                                    .text_color(cx.theme().accent_foreground)
                            })
                            .when(!active, |this| {
                                this.text_color(cx.theme().foreground)
                                    .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
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
                                .text_color(cx.theme().muted_foreground)
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
                                .hover(|this| this.bg(cx.theme().accent.opacity(0.06)))
                                .child(
                                    div()
                                        .w(px(64.))
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("{:.0}°C", p.0)),
                                )
                                .child(
                                    div()
                                        .w(px(64.))
                                        .text_sm()
                                        .text_color(cx.theme().accent)
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
                                                .border_color(cx.theme().border)
                                                .hover(|this| {
                                                    this.bg(cx.theme().accent.opacity(0.1))
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
                                            .border_color(cx.theme().border)
                                            .text_color(cx.theme().red)
                                            .hover(|this| this.bg(cx.theme().red.opacity(0.1)))
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
                                        .border_color(cx.theme().border)
                                        .hover(|this| this.bg(cx.theme().accent.opacity(0.1)))
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
                                        .bg(cx.theme().accent)
                                        .text_color(cx.theme().accent_foreground)
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
                        .border_color(cx.theme().yellow.opacity(0.5))
                        .text_color(cx.theme().yellow)
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
                        cx.theme().accent
                    } else {
                        cx.theme().border
                    })
                    .when(is_active, |this| this.bg(cx.theme().accent.opacity(0.12)))
                    .hover(|this| this.bg(cx.theme().accent.opacity(0.08)))
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
                                        .text_color(cx.theme().accent)
                                        .child("active"),
                                )
                            }),
                    )
            }))
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let cap = self
            .status
            .as_ref()
            .map(|s| s.daemon.capability.level)
            .unwrap_or(CapabilityLevel::None);
        let (toast_msg, toast_err) = self.toast.clone().map(|(m, e)| (m, e)).unzip();
        let ok_color = cx.theme().green;
        let err_color = cx.theme().red;
        h_flex()
            .px_4()
            .gap_4()
            .h_8()
            .flex_none()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().tab_bar)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                h_flex()
                    .gap_3()
                    .child(match cap {
                        CapabilityLevel::Full => "full fan control".to_string(),
                        CapabilityLevel::MonitorProfiles => "monitoring + profiles".to_string(),
                        CapabilityLevel::None => "no fan interface".to_string(),
                    })
                    .child(format!("{}", self.socket.display())),
            )
            .child(h_flex().gap_2().when_some(toast_msg, |this, msg| {
                this.text_color(if toast_err.unwrap_or(false) {
                    err_color
                } else {
                    ok_color
                })
                .child(msg)
            }))
    }

    fn render_alerts_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let recent: Vec<Alert> = self.alerts.iter().rev().take(3).cloned().collect();
        v_flex().gap_1().px_4().children(recent.iter().map(|a| {
            div()
                .text_xs()
                .text_color(if a.level == AlertLevel::Critical {
                    cx.theme().red
                } else {
                    cx.theme().yellow
                })
                .child(format!("{:?}: {}", a.kind, a.message))
        }))
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
            .child(
                TitleBar::new().child(
                    div()
                        .ml_2()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child("Thermite"),
                ),
            )
            .bg(cx.theme().background)
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
            .child(self.render_alerts_panel(cx))
            .child(self.render_status_bar(cx))
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
                    tx.send(IpcEvent::Tick(status)).ok();
                }
                if let Ok(Response::Config { config }) = client.call(Request::GetConfig) {
                    tx.send(IpcEvent::Config(Box::new(config))).ok();
                }
                if client.subscribe().is_ok() {
                    loop {
                        match client.read_event() {
                            Ok(Event::Tick { telemetry, control }) => {
                                daemon.uptime_ms = daemon.uptime_ms.saturating_add(1000);
                                tx.send(IpcEvent::Tick(StatusSnapshot {
                                    telemetry,
                                    control,
                                    daemon: daemon.clone(),
                                    recent_alerts: Vec::new(),
                                }))
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
        gpui_kit::open_window(window_options, cx, move |window, cx| {
            window.activate_window();
            window.set_window_title("Thermite");
            Theme::change(ThemeMode::Dark, Some(window), cx);
            cx.new(|cx| ThermiteApp::new(socket_for_window.clone(), window, cx))
        })
        .expect("failed to open window");
    });
}
