#![allow(
    clippy::field_reassign_with_default,
    clippy::option_map_unit_fn,
    clippy::type_complexity,
    clippy::too_many_arguments
)]

use std::ffi::OsStr;
use std::hash::Hash;
use std::{
    fs, mem,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use client::{Client, ClientSettings, ClientState};
use crusader_lib::plot::{smooth, LatencySummary};
use crusader_lib::test::timed;
use crusader_lib::{
    file_format::{RawPing, RawResult, TestKind},
    latency,
    plot::{self, float_max, to_rates},
    protocol, remote, serve,
    test::{self, PlotConfig},
    with_time,
};
use eframe::egui::{AboveOrBelow, Label, Layout, TextWrapMode};
use eframe::{
    egui::{
        self, Grid, Id, PopupCloseBehavior, RichText, ScrollArea, TextEdit, TextStyle, Ui, Vec2b,
    },
    emath::Align,
    epaint::Color32,
};
use egui_extras::{Size, Strip, StripBuilder};
use egui_plot::{ColorConflictHandling, Legend, Line, Plot, PlotPoints};

#[cfg(not(target_os = "android"))]
use rfd::FileDialog;

use serde::{Deserialize, Serialize};
use tokio::sync::{
    mpsc::{self, error::TryRecvError},
    oneshot,
};

mod client;

struct Server {
    done: Option<oneshot::Receiver<()>>,
    msgs: Vec<String>,
    rx: mpsc::UnboundedReceiver<String>,
    stop: Option<oneshot::Sender<()>>,
    started: oneshot::Receiver<Result<(), String>>,
}

enum ServerState {
    Stopped(Option<String>),
    Starting,
    Stopping,
    Running,
}

struct Latency {
    done: Option<oneshot::Receiver<Option<Result<(), String>>>>,
    abort: Option<oneshot::Sender<()>>,
}

#[derive(PartialEq, Eq)]
enum Tab {
    Client,
    Server,
    Remote,
    Monitor,
    Result,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct LatencyMonitorSettings {
    pub server: String,
    pub history: f64,
    pub latency_sample_interval: u64,
}

impl Default for LatencyMonitorSettings {
    fn default() -> Self {
        Self {
            server: "".to_owned(),
            history: 60.0,
            latency_sample_interval: 5,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
#[serde(default)]
pub struct Settings {
    pub client: ClientSettings,
    pub latency_monitor: LatencyMonitorSettings,
}

impl Settings {
    fn from_path(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|data| toml::from_str(&data).ok())
            .unwrap_or_default()
    }
}

pub struct Tester {
    tab: Tab,
    settings: Settings,
    settings_path: Option<PathBuf>,
    saved_settings: Settings,
    server_state: ServerState,
    server: Option<Server>,
    remote_state: ServerState,
    remote_server: Option<Server>,
    client_state: ClientState,
    client: Option<Client>,
    result_plot_reset: bool,
    result: Option<TestResult>,
    raw_result_saved: Option<PathBuf>,
    open_result: Vec<PathBuf>,
    result_name: String,
    msgs: Vec<String>,
    msg_scrolled: usize,
    pub file_loader: Option<Box<dyn Fn(&mut Tester)>>,
    pub plot_saver: Option<Box<dyn Fn(&plot::TestResult)>>,
    pub raw_saver: Option<Box<dyn Fn(&RawResult)>>,

    latency_state: ClientState,
    latency: Option<Latency>,
    latency_data: Arc<latency::Data>,
    latency_stop: Duration,
    latency_error: Option<String>,
    latency_plot_reset: bool,
}

pub struct LatencyResult {
    total: Vec<(f64, f64)>,
    max: f64,
    up: Vec<(f64, f64)>,
    down: Vec<(f64, f64)>,
    loss: Vec<(f64, Option<bool>)>,
}
impl LatencyResult {
    fn new(result: &plot::TestResult, pings: &[RawPing]) -> Self {
        let start = result.start.as_secs_f64();
        let total: Vec<_> = pings
            .iter()
            .filter(|p| p.sent >= result.start)
            .filter_map(|p| {
                p.latency.and_then(|latency| {
                    latency
                        .total
                        .map(|total| (p.sent.as_secs_f64() - start, total.as_secs_f64() * 1000.0))
                })
            })
            .collect();

        let up: Vec<_> = pings
            .iter()
            .filter(|p| p.sent >= result.start)
            .filter_map(|p| {
                p.latency.map(|latency| {
                    (
                        p.sent.as_secs_f64() - start,
                        latency.up.as_secs_f64() * 1000.0,
                    )
                })
            })
            .collect();

        let down: Vec<_> = pings
            .iter()
            .filter(|p| p.sent >= result.start)
            .filter_map(|p| {
                p.latency.and_then(|latency| {
                    latency
                        .down()
                        .map(|down| (p.sent.as_secs_f64() - start, down.as_secs_f64() * 1000.0))
                })
            })
            .collect();

        let loss = pings
            .iter()
            .filter(|p| p.sent >= result.start)
            .filter_map(|ping| {
                if ping.latency.and_then(|latency| latency.total).is_none() {
                    let down_loss =
                        (result.raw_result.version >= 2).then_some(ping.latency.is_some());
                    Some((ping.sent.as_secs_f64() - start, down_loss))
                } else {
                    None
                }
            })
            .collect();
        let max = float_max(total.iter().map(|v| v.1));
        LatencyResult {
            total,
            up,
            down,
            loss,
            max,
        }
    }
}

pub struct TestResult {
    result: plot::TestResult,
    download: Option<Vec<(f64, f64)>>,
    download_avg: Option<Vec<(f64, f64)>>,
    upload: Option<Vec<(f64, f64)>>,
    upload_avg: Option<Vec<(f64, f64)>>,
    both_download: Option<Vec<(f64, f64)>>,
    both_download_avg: Option<Vec<(f64, f64)>>,
    both_upload: Option<Vec<(f64, f64)>>,
    both_upload_avg: Option<Vec<(f64, f64)>>,
    both: Option<Vec<(f64, f64)>>,
    both_avg: Option<Vec<(f64, f64)>>,
    local_latency: LatencyResult,
    peer_latency: Option<LatencyResult>,
    throughput_max: f64,
}

impl TestResult {
    fn new(result: plot::TestResult) -> Self {
        let smooth_interval =
            Duration::from_secs_f64(1.0).min(result.raw_result.config.grace_duration);
        let interval = result.raw_result.config.bandwidth_interval;

        let start = result.start.as_secs_f64();

        let download = result
            .download_bytes
            .as_ref()
            .map(|bytes| handle_bytes(bytes, start));
        let download_avg = result
            .download_bytes
            .as_ref()
            .map(|bytes| smooth_bytes(bytes, start, interval, smooth_interval));

        let upload = result
            .upload_bytes
            .as_ref()
            .map(|bytes| handle_bytes(bytes, start));
        let upload_avg = result
            .upload_bytes
            .as_ref()
            .map(|bytes| smooth_bytes(bytes, start, interval, smooth_interval));

        let both_upload = result
            .both_upload_bytes
            .as_ref()
            .map(|bytes| handle_bytes(bytes, start));
        let both_upload_avg = result
            .both_upload_bytes
            .as_ref()
            .map(|bytes| smooth_bytes(bytes, start, interval, smooth_interval));

        let both_download = result
            .both_download_bytes
            .as_ref()
            .map(|bytes| handle_bytes(bytes, start));
        let both_download_avg = result
            .both_download_bytes
            .as_ref()
            .map(|bytes| smooth_bytes(bytes, start, interval, smooth_interval));

        let both = result
            .both_bytes
            .as_ref()
            .map(|bytes| handle_bytes(bytes, start));
        let both_avg = result
            .both_bytes
            .as_ref()
            .map(|bytes| smooth_bytes(bytes, start, interval, smooth_interval));

        let download_max = download
            .as_ref()
            .map(|data| float_max(data.iter().map(|v| v.1)));
        let upload_max = upload
            .as_ref()
            .map(|data| float_max(data.iter().map(|v| v.1)));
        let both_upload_max = both_upload
            .as_ref()
            .map(|data| float_max(data.iter().map(|v| v.1)));
        let both_download_max = both_download
            .as_ref()
            .map(|data| float_max(data.iter().map(|v| v.1)));
        let both_max = both
            .as_ref()
            .map(|data| float_max(data.iter().map(|v| v.1)));
        let throughput_max = float_max(
            [
                download_max,
                upload_max,
                both_upload_max,
                both_download_max,
                both_max,
            ]
            .into_iter()
            .flatten(),
        );

        TestResult {
            download,
            download_avg,
            upload,
            upload_avg,
            both_download,
            both_download_avg,
            both_upload,
            both_upload_avg,
            both,
            both_avg,
            throughput_max,
            local_latency: LatencyResult::new(&result, &result.pings),
            peer_latency: result
                .raw_result
                .peer_pings
                .as_ref()
                .map(|pings| LatencyResult::new(&result, pings)),
            result,
        }
    }
}

pub fn handle_bytes(data: &[(u64, f64)], start: f64) -> Vec<(f64, f64)> {
    to_rates(data)
        .into_iter()
        .map(|(time, speed)| (Duration::from_micros(time).as_secs_f64() - start, speed))
        .collect()
}

pub fn smooth_bytes(
    data: &[(u64, f64)],
    start: f64,
    interval: Duration,
    smoothing_interval: Duration,
) -> Vec<(f64, f64)> {
    smooth(data, interval, smoothing_interval)
        .into_iter()
        .map(|(time, speed)| (Duration::from_micros(time).as_secs_f64() - start, speed))
        .collect()
}

fn hover_popup(
    ui: &mut Ui,
    id_source: impl Hash,
    position: AboveOrBelow,
    popup: impl FnOnce(&mut Ui),
) {
    ui.scope(|ui| {
        let id = ui.make_persistent_id(id_source);

        ui.spacing_mut().interact_size.y = 18.0;
        let active = id.with("active");

        let active_value = ui.memory_mut(|mem| {
            let active = mem.data.get_temp_mut_or_default(active);
            *active
        });

        let style = ui.style_mut();

        style.visuals.widgets.inactive.rounding = 50.0.into();
        style.visuals.widgets.hovered.rounding = 50.0.into();
        style.visuals.widgets.active.rounding = 50.0.into();
        style.visuals.widgets.inactive.fg_stroke.color = Color32::from_gray(140);

        if active_value {
            style.visuals.widgets.inactive.weak_bg_fill = style.visuals.selection.bg_fill;
            style.visuals.widgets.hovered.weak_bg_fill = style.visuals.selection.bg_fill;
        }

        let stats = ui.button("i");
        let popup_id = id.with("popup");

        let contains_pointer =
            if let Some(pointer) = ui.ctx().input(|input| input.pointer.latest_pos()) {
                stats.interact_rect.expand(5.0).contains(pointer)
            } else {
                false
            };

        if stats.hovered() && contains_pointer {
            ui.memory_mut(|mem| {
                if !mem.any_popup_open() {
                    mem.open_popup(popup_id);
                }
            });
        } else if !active_value && !contains_pointer {
            ui.memory_mut(|mem| {
                if mem.is_popup_open(popup_id) {
                    mem.close_popup();
                }
            });
        }

        if stats.clicked() {
            ui.memory_mut(|mem| {
                let active: &mut bool = mem.data.get_temp_mut_or_default(active);
                *active = !*active;
                if *active {
                    mem.open_popup(popup_id);
                } else {
                    mem.close_popup();
                }
            });
        }

        egui::popup::popup_above_or_below_widget(
            ui,
            popup_id,
            &stats,
            position,
            PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                popup(ui);
            },
        );

        ui.memory_mut(|mem| {
            if !mem.is_popup_open(popup_id) {
                let active: &mut bool = mem.data.get_temp_mut_or_default(active);
                if *active {
                    //    ui.ctx().request_repaint();
                }
                *active = false;
            }
        });
    });
}

impl Drop for Tester {
    fn drop(&mut self) {
        self.save_settings();

        // Stop client
        self.client.as_mut().map(|client| {
            mem::take(&mut client.abort).map(|abort| {
                abort.send(()).unwrap();
            });
            mem::take(&mut client.done).map(|done| {
                done.blocking_recv().ok();
            });
        });

        // Stop server
        self.server.as_mut().map(|server| {
            mem::take(&mut server.stop).map(|stop| {
                stop.send(()).unwrap();
            });
            mem::take(&mut server.done).map(|done| {
                done.blocking_recv().ok();
            });
        });

        // Stop latency
        self.latency.as_mut().map(|latency| {
            mem::take(&mut latency.abort).map(|abort| {
                abort.send(()).unwrap();
            });
            mem::take(&mut latency.done).map(|done| {
                done.blocking_recv().ok();
            });
        });
    }
}

impl Tester {
    pub fn new(settings_path: Option<PathBuf>) -> Tester {
        let settings = settings_path
            .as_deref()
            .map_or(Settings::default(), Settings::from_path);
        Tester {
            tab: Tab::Client,
            saved_settings: settings.clone(),
            settings,
            settings_path,
            client_state: ClientState::Stopped,
            client: None,
            result: None,
            result_plot_reset: false,
            raw_result_saved: None,
            result_name: "".to_string(),
            open_result: Vec::new(),
            msgs: Vec::new(),
            msg_scrolled: 0,
            server_state: ServerState::Stopped(None),
            server: None,
            remote_state: ServerState::Stopped(None),
            remote_server: None,
            file_loader: None,
            raw_saver: None,
            plot_saver: None,
            latency_state: ClientState::Stopped,
            latency: None,
            latency_data: Arc::new(latency::Data::new(0, Arc::new(|| {}))),
            latency_stop: Duration::from_secs(0),
            latency_error: None,
            latency_plot_reset: false,
        }
    }

    pub fn set_result(&mut self, result: plot::TestResult) {
        self.result = Some(TestResult::new(result));
        self.result_name = "test".to_owned();
        self.result_plot_reset = true;
        self.raw_result_saved = None;
    }

    pub fn load_file(&mut self, name: PathBuf, raw: RawResult) {
        self.set_result(raw.to_test_result());
        self.raw_result_saved = Some(name);
    }

    pub fn save_raw(&mut self, name: PathBuf) {
        self.raw_result_saved = Some(name);
    }

    fn save_settings(&mut self) {
        if self.settings != self.saved_settings {
            self.settings_path.as_deref().map(|path| {
                toml::ser::to_string_pretty(&self.settings)
                    .map(|data| fs::write(path, data.as_bytes()))
                    .ok();
            });
            self.saved_settings = self.settings.clone();
        }
    }

    fn load_result(&mut self) {
        #[cfg(not(target_os = "android"))]
        {
            FileDialog::new()
                .add_filter("Crusader Raw Result", &["crr"])
                .add_filter("All files", &["*"])
                .pick_file()
                .map(|file| {
                    RawResult::load(&file).map(|raw| {
                        self.load_file(file, raw);
                    })
                });
        }
        let file_loader = self.file_loader.take();
        file_loader.as_ref().map(|loader| loader(self));
        self.file_loader = file_loader;
    }

    fn latency_and_loss(
        &mut self,
        strip: &mut Strip<'_, '_>,
        link: Id,
        reset: bool,
        peer: bool,
        y_axis_size: f32,
    ) {
        let result = self.result.as_ref().unwrap();

        let data = if peer {
            result.peer_latency.as_ref().unwrap()
        } else {
            &result.local_latency
        };

        let latencies = if peer {
            &result.result.peer_latencies
        } else {
            &result.result.latencies
        };

        let duration = result.result.duration.as_secs_f64() * 1.1;

        strip.cell(|ui| {
            ui.horizontal(|ui| {
                let label = if peer { "Peer latency" } else { "Latency" };
                ui.label(label);

                hover_popup(
                    ui,
                    (label, "Popup"),
                    if !peer && result.result.raw_result.idle() {
                        AboveOrBelow::Below
                    } else {
                        AboveOrBelow::Above
                    },
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.spacing_mut().interact_size.y = 10.0;

                        let stats = |ui: &mut Ui, name, color, latency: &LatencySummary| {
                            ui.vertical(|ui| {
                                ui.add_space(5.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{name}: ")).color(color));
                                    ui.label(format!(
                                        "{:.01} ms",
                                        latency.total.as_secs_f64() * 1000.0
                                    ));
                                });
                                ui.horizontal(|ui| {
                                    ui.label(format!(
                                        "\t\t{:.01} ms ",
                                        latency.down.as_secs_f64() * 1000.0
                                    ));
                                    ui.label(
                                        RichText::new("down").color(Color32::from_rgb(95, 145, 62)),
                                    );
                                });
                                ui.horizontal(|ui| {
                                    ui.label(format!(
                                        "\t\t{:.01} ms ",
                                        latency.up.as_secs_f64() * 1000.0
                                    ));
                                    ui.label(
                                        RichText::new("up").color(Color32::from_rgb(37, 83, 169)),
                                    );
                                });
                            });
                        };

                        if let Some(latency) = latencies.latencies.get(&Some(TestKind::Download)) {
                            stats(ui, "Download", Color32::from_rgb(95, 145, 62), latency);
                        }

                        if let Some(latency) = latencies.latencies.get(&Some(TestKind::Upload)) {
                            stats(ui, "Upload", Color32::from_rgb(37, 83, 169), latency);
                        }

                        if let Some(latency) =
                            latencies.latencies.get(&Some(TestKind::Bidirectional))
                        {
                            stats(
                                ui,
                                "Bidirectional",
                                Color32::from_rgb(149, 96, 153),
                                latency,
                            );
                        }

                        if let Some(latency) = latencies.latencies.get(&None) {
                            stats(ui, "Latency", Color32::from_rgb(0, 0, 0), latency);
                        }

                        ui.vertical(|ui| {
                            ui.add_space(5.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("Idle latency: ")
                                        .color(Color32::from_rgb(128, 128, 128)),
                                );
                                ui.label(format!(
                                    "{:.02} ms",
                                    result.result.raw_result.server_latency.as_secs_f64() * 1000.0
                                ));
                            });
                        });

                        ui.vertical(|ui| {
                            ui.add_space(5.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("Latency sample interval: ")
                                        .color(Color32::from_rgb(128, 128, 128)),
                                );
                                ui.label(format!(
                                    "{:.02} ms",
                                    result.result.raw_result.config.ping_interval.as_secs_f64()
                                        * 1000.0
                                ));
                            });
                        });
                    },
                );
            });

            // Latency
            let mut plot = Plot::new((peer, "ping"))
                .legend(Legend::default().insertion_order(true))
                .y_axis_min_width(y_axis_size)
                .link_axis(link, true, false)
                .link_cursor(link, true, false)
                .include_x(0.0)
                .include_x(duration)
                .include_y(0.0)
                .include_y(data.max * 1.1)
                .label_formatter(|_, value| {
                    format!("Latency = {:.2} ms\nTime = {:.2} s", value.y, value.x)
                });

            if reset {
                plot = plot.reset();
            }

            plot.show(ui, |plot_ui| {
                if result.result.raw_result.version >= 1 {
                    let latency = data.up.iter().map(|v| [v.0, v.1]);
                    let latency = Line::new(PlotPoints::from_iter(latency))
                        .color(Color32::from_rgb(37, 83, 169))
                        .name("Up");

                    plot_ui.line(latency);

                    let latency = data.down.iter().map(|v| [v.0, v.1]);
                    let latency = Line::new(PlotPoints::from_iter(latency))
                        .color(Color32::from_rgb(95, 145, 62))
                        .name("Down");

                    plot_ui.line(latency);
                }

                let latency = data.total.iter().map(|v| [v.0, v.1]);
                let latency = Line::new(PlotPoints::from_iter(latency))
                    .color(Color32::from_rgb(50, 50, 50))
                    .name("Round-trip");

                plot_ui.line(latency);
            });
        });

        strip.cell(|ui| {
            ui.horizontal(|ui| {
                let label = if peer {
                    "Peer packet loss"
                } else {
                    "Packet loss"
                };
                ui.label(label);

                hover_popup(ui, (label, "Popup"), AboveOrBelow::Above, |ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.spacing_mut().interact_size.y = 10.0;

                    let stats = |ui: &mut Ui, name, color, (down, up): (f64, f64)| {
                        ui.vertical(|ui| {
                            ui.add_space(5.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{name}: ")).color(color));
                                if down == 0.0 && up == 0.0 {
                                    ui.label("0%");
                                } else {
                                    ui.label(format!(
                                        "{:.1$}% ",
                                        down * 100.0,
                                        if down == 0.0 { 0 } else { 2 }
                                    ));
                                    ui.label(
                                        RichText::new("down").color(Color32::from_rgb(95, 145, 62)),
                                    );
                                    ui.label(format!(
                                        ", {:.1$}% ",
                                        up * 100.0,
                                        if up == 0.0 { 0 } else { 2 }
                                    ));
                                    ui.label(
                                        RichText::new("up").color(Color32::from_rgb(37, 83, 169)),
                                    );
                                }
                            });
                        });
                    };

                    if let Some(loss) = latencies.loss.get(&Some(TestKind::Download)) {
                        stats(ui, "Download", Color32::from_rgb(95, 145, 62), *loss);
                    }

                    if let Some(loss) = latencies.loss.get(&Some(TestKind::Upload)) {
                        stats(ui, "Upload", Color32::from_rgb(37, 83, 169), *loss);
                    }

                    if let Some(loss) = latencies.loss.get(&Some(TestKind::Bidirectional)) {
                        stats(ui, "Bidirectional", Color32::from_rgb(149, 96, 153), *loss);
                    }

                    if let Some(loss) = latencies.loss.get(&None) {
                        stats(ui, "Packet loss", Color32::from_rgb(0, 0, 0), *loss);
                    }
                });
            });

            // Packet loss
            let mut plot = Plot::new((peer, "loss"))
                .legend(Legend::default())
                .show_axes([false, true])
                .show_grid(Vec2b::new(true, false))
                .y_axis_min_width(y_axis_size)
                .y_axis_formatter(|_, _| String::new())
                .link_axis(link, true, false)
                .link_cursor(link, true, false)
                .center_y_axis(true)
                .allow_zoom(false)
                .allow_boxed_zoom(false)
                .include_x(0.0)
                .include_x(duration)
                .include_y(-1.0)
                .include_y(1.0)
                .height(30.0)
                .label_formatter(|_, value| format!("Time = {:.2} s", value.x));

            if reset {
                plot = plot.reset();
            }

            plot.show(ui, |plot_ui| {
                for &(loss, down_loss) in &data.loss {
                    let (color, s, e) = down_loss
                        .map(|down_loss| {
                            if down_loss {
                                (Color32::from_rgb(95, 145, 62), 1.0, 0.0)
                            } else {
                                (Color32::from_rgb(37, 83, 169), -1.0, 0.0)
                            }
                        })
                        .unwrap_or((Color32::from_rgb(193, 85, 85), -1.0, 1.0));

                    plot_ui.line(
                        Line::new(PlotPoints::from_iter(
                            [[loss, s], [loss, e]].iter().copied(),
                        ))
                        .color(color),
                    );

                    if down_loss.is_some() {
                        plot_ui.line(
                            Line::new(PlotPoints::from_iter(
                                [[loss, s], [loss, s - s / 5.0]].iter().copied(),
                            ))
                            .width(3.0)
                            .color(color),
                        );
                    }
                }
            });
        });
    }

    fn load_popup(&mut self, ui: &mut Ui) {
        if cfg!(not(target_os = "android")) {
            ui.add_space(10.0);

            let popup_id = ui.make_persistent_id("Load-Popup");

            let button = ui.button("Open from results");

            if button.clicked() {
                ui.memory_mut(|mem| {
                    mem.toggle_popup(popup_id);
                    if mem.is_popup_open(popup_id) {
                        self.open_result = fs::read_dir("crusader-results")
                            .ok()
                            .map(|dir| {
                                dir.filter_map(|file| {
                                    file.ok()
                                        .map(|file| file.path())
                                        .filter(|path| path.extension() == Some(OsStr::new("crr")))
                                })
                                .collect()
                            })
                            .unwrap_or_default();
                    }
                });
            }

            egui::popup::popup_below_widget(
                ui,
                popup_id,
                &button,
                PopupCloseBehavior::CloseOnClickOutside,
                |ui| {
                    ui.set_min_width(300.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Results available in the");
                        if ui.link("crusader-results").clicked() {
                            open::that("crusader-results").ok();
                        }
                        ui.label("folder:");
                    });

                    ScrollArea::vertical().show(ui, |ui| {
                        ui.with_layout(Layout::top_down_justified(Align::LEFT), |ui| {
                            for file in self.open_result.clone() {
                                if let Some(prefix) =
                                    file.file_name().and_then(|stem| stem.to_str())
                                {
                                    if ui.toggle_value(&mut false, prefix).clicked() {
                                        ui.memory_mut(|mem| mem.close_popup());
                                        RawResult::load(&file).map(|raw| {
                                            self.load_file(file, raw);
                                        });
                                    }
                                }
                            }
                        });
                    });
                },
            );
        }
    }

    fn result(&mut self, _ctx: &egui::Context, ui: &mut Ui) {
        if self.result.is_none() {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open").clicked() {
                    self.load_result();
                }
                self.load_popup(ui);
            });
            ui.separator();
            ui.label("No result.");
            return;
        }

        ui.horizontal_wrapped(|ui| {
            if ui.button("Open").clicked() {
                self.load_result();
            }

            if ui.button("Save").clicked() {
                match self.raw_saver.as_ref() {
                    Some(saver) => {
                        saver(&self.result.as_ref().unwrap().result.raw_result);
                    }
                    None => {
                        #[cfg(not(target_os = "android"))]
                        {
                            FileDialog::new()
                                .add_filter("Crusader Raw Result", &["crr"])
                                .add_filter("All files", &["*"])
                                .set_file_name(&format!("{}.crr", timed("test")))
                                .save_file()
                                .map(|file| {
                                    if self
                                        .result
                                        .as_ref()
                                        .unwrap()
                                        .result
                                        .raw_result
                                        .save(&file)
                                        .is_ok()
                                    {
                                        self.raw_result_saved = Some(file);
                                    }
                                });
                        }
                    }
                }
            }

            self.load_popup(ui);

            if cfg!(not(target_os = "android")) {
                let popup_id = ui.make_persistent_id("Save-Popup");

                let button = ui.button("Save to results");

                if button.clicked() {
                    ui.memory_mut(|mem| {
                        mem.toggle_popup(popup_id);
                    });
                }

                egui::popup::popup_below_widget(
                    ui,
                    popup_id,
                    &button,
                    PopupCloseBehavior::CloseOnClickOutside,
                    |ui| {
                        ui.set_min_width(250.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label("This saves both the data and plot in the");
                            if ui.link("crusader-results").clicked() {
                                open::that("crusader-results").ok();
                            }
                            ui.label("folder.");
                        });
                        ui.horizontal(|ui| {
                            ui.label("Name: ");
                            let mut click = ui
                                .add(
                                    TextEdit::singleline(&mut self.result_name)
                                        .desired_width(175.0),
                                )
                                .lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            click |= ui.button("Save").clicked();
                            if click {
                                let name = timed(&self.result_name);
                                self.raw_result_saved = test::save_raw(
                                    &self.result.as_ref().unwrap().result.raw_result,
                                    &name,
                                    Path::new("crusader-results"),
                                )
                                .ok();
                                plot::save_graph(
                                    &PlotConfig::default(),
                                    &self.result.as_ref().unwrap().result,
                                    &name,
                                    Path::new("crusader-results"),
                                )
                                .ok();
                                ui.memory_mut(|mem| {
                                    mem.close_popup();
                                });
                            }
                        });
                    },
                );
            }

            ui.add_space(10.0);

            if ui.button("Export plot").clicked() {
                match self.plot_saver.as_ref() {
                    Some(saver) => {
                        saver(&self.result.as_ref().unwrap().result);
                    }
                    None => {
                        #[cfg(not(target_os = "android"))]
                        {
                            let name = self
                                .raw_result_saved
                                .as_ref()
                                .and_then(|file| {
                                    file.file_stem()
                                        .unwrap_or_default()
                                        .to_str()
                                        .map(|s| s.to_owned())
                                })
                                .unwrap_or(timed("test"));

                            let mut dialog = FileDialog::new()
                                .add_filter("Portable Network Graphics", &["png"])
                                .add_filter("All files", &["*"])
                                .set_file_name(&format!("{}.png", name));

                            if let Some(file) = self.raw_result_saved.as_ref() {
                                if let Some(parent) = file.parent() {
                                    dialog = dialog.set_directory(parent);
                                }
                            }

                            dialog.save_file().map(|file| {
                                if plot::save_graph_to_path(
                                    &file,
                                    &PlotConfig::default(),
                                    &self.result.as_ref().unwrap().result,
                                )
                                .is_ok()
                                {
                                    file.file_name()
                                        .unwrap_or_default()
                                        .to_str()
                                        .map(|s| s.to_owned());
                                }
                            });
                        }
                    }
                }
            }
        });
        ui.separator();

        self.raw_result_saved
            .as_ref()
            .and_then(|file| {
                file.file_name()
                    .unwrap_or_default()
                    .to_str()
                    .map(|s| s.to_owned())
            })
            .map(|file| {
                ui.label(format!("Saved as: {file}"));
                ui.separator();
            });

        let result = self.result.as_ref().unwrap();

        if result.result.raw_result.server_overload {
            ui.label("Warning: Server overload detected during test. Result should be discarded.");
            ui.separator();
        }

        if result.result.raw_result.load_termination_timeout {
            ui.label("Warning: Load termination timed out. There may be residual untracked traffic in the background.");
            ui.separator();
        }

        let packet_loss_size = 75.0;

        let result = self.result.as_ref().unwrap();

        let link = ui.id().with("result-link");

        let mut strip = StripBuilder::new(ui);

        if result.result.raw_result.streams() > 0 {
            strip = strip.size(Size::remainder());
        }

        for _ in 0..(1 + result.peer_latency.is_some() as u8) {
            strip = strip
                .size(Size::remainder())
                .size(Size::exact(packet_loss_size));
        }

        strip.vertical(|mut strip| {
            let reset = mem::take(&mut self.result_plot_reset);

            let result = self.result.as_ref().unwrap();

            let y_axis_size = 30.0;

            let duration = result.result.duration.as_secs_f64() * 1.1;

            if result.result.raw_result.streams() > 0 {
                strip.cell(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Throughput");

                        hover_popup(ui, "Throughput-Popup", AboveOrBelow::Below, |ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.spacing_mut().interact_size.y = 10.0;

                            if let Some(throughput) = result
                                .result
                                .throughputs
                                .get(&(TestKind::Download, TestKind::Download))
                            {
                                ui.vertical(|ui| {
                                    ui.add_space(5.0);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new("Download: ")
                                                .color(Color32::from_rgb(95, 145, 62)),
                                        );
                                        ui.label(format!("{:.02} Mbps", throughput));
                                    });
                                });
                            }

                            if let Some(throughput) = result
                                .result
                                .throughputs
                                .get(&(TestKind::Upload, TestKind::Upload))
                            {
                                ui.vertical(|ui| {
                                    ui.add_space(5.0);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new("Upload: ")
                                                .color(Color32::from_rgb(37, 83, 169)),
                                        );
                                        ui.label(format!("{:.02} Mbps", throughput));
                                    });
                                });
                            }

                            if let Some(throughput) = result
                                .result
                                .throughputs
                                .get(&(TestKind::Bidirectional, TestKind::Bidirectional))
                            {
                                ui.vertical(|ui| {
                                    ui.add_space(5.0);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new("Bidirectional: ")
                                                .color(Color32::from_rgb(149, 96, 153)),
                                        );
                                        ui.label(format!("{:.02} Mbps ", throughput));
                                    });
                                    if let Some(down) = result
                                        .result
                                        .throughputs
                                        .get(&(TestKind::Bidirectional, TestKind::Download))
                                    {
                                        if let Some(up) = result
                                            .result
                                            .throughputs
                                            .get(&(TestKind::Bidirectional, TestKind::Upload))
                                        {
                                            ui.horizontal(|ui| {
                                                ui.label(format!("\t\t{:.02} Mbps ", down));
                                                ui.label(
                                                    RichText::new("down")
                                                        .color(Color32::from_rgb(95, 145, 62)),
                                                );
                                            });
                                            ui.horizontal(|ui| {
                                                ui.label(format!("\t\t{:.02} Mbps ", up));
                                                ui.label(
                                                    RichText::new("up")
                                                        .color(Color32::from_rgb(37, 83, 169)),
                                                );
                                            });
                                        }
                                    }
                                });
                            }

                            ui.vertical(|ui| {
                                ui.add_space(5.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("Streams: ")
                                            .color(Color32::from_rgb(128, 128, 128)),
                                    );
                                    ui.label(format!("{}", result.result.raw_result.streams()));
                                });
                            });

                            ui.vertical(|ui| {
                                ui.add_space(5.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("Stream Stagger: ")
                                            .color(Color32::from_rgb(128, 128, 128)),
                                    );
                                    ui.label(format!(
                                        "{:.02} seconds",
                                        result.result.raw_result.config.stagger.as_secs_f64()
                                    ));
                                });
                            });

                            ui.vertical(|ui| {
                                ui.add_space(5.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("Throughput sample interval: ")
                                            .color(Color32::from_rgb(128, 128, 128)),
                                    );
                                    ui.label(format!(
                                        "{:.02} ms",
                                        result
                                            .result
                                            .raw_result
                                            .config
                                            .bandwidth_interval
                                            .as_secs_f64()
                                            * 1000.0
                                    ));
                                });
                            });
                        });
                    });

                    // Throughput
                    let mut plot = Plot::new("result")
                        .legend(
                            Legend::default()
                                .color_conflict_handling(ColorConflictHandling::PickFirst)
                                .insertion_order(true),
                        )
                        .y_axis_min_width(y_axis_size)
                        .link_axis(link, true, false)
                        .link_cursor(link, true, false)
                        .include_x(0.0)
                        .include_x(duration)
                        .include_y(0.0)
                        .include_y(result.throughput_max * 1.1)
                        .height(ui.available_height())
                        .label_formatter(|_, value| {
                            format!("Throughput = {:.2} Mbps\nTime = {:.2} s", value.y, value.x)
                        });

                    if reset {
                        plot = plot.reset();
                    }

                    plot.show(ui, |plot_ui| {
                        let width = 1.0;
                        if let Some(data) = result.download.as_ref() {
                            let download = data.iter().map(|v| [v.0, v.1]);
                            let download = Line::new(PlotPoints::from_iter(download))
                                .color(Color32::from_rgb(95, 145, 62))
                                .width(width)
                                .name("Download");

                            plot_ui.line(download);
                        }
                        if let Some(data) = result.upload.as_ref() {
                            let upload = data.iter().map(|v| [v.0, v.1]);
                            let upload = Line::new(PlotPoints::from_iter(upload))
                                .color(Color32::from_rgb(37, 83, 169))
                                .width(width)
                                .name("Upload");

                            plot_ui.line(upload);
                        }
                        if let Some(data) = result.both_download.as_ref() {
                            let download = data.iter().map(|v| [v.0, v.1]);
                            let download = Line::new(PlotPoints::from_iter(download))
                                .color(Color32::from_rgb(95, 145, 62))
                                .width(width)
                                .name("Download");

                            plot_ui.line(download);
                        }
                        if let Some(data) = result.both_upload.as_ref() {
                            let upload = data.iter().map(|v| [v.0, v.1]);
                            let upload = Line::new(PlotPoints::from_iter(upload))
                                .color(Color32::from_rgb(37, 83, 169))
                                .width(width)
                                .name("Upload");

                            plot_ui.line(upload);
                        }
                        if let Some(data) = result.both.as_ref() {
                            let both = data.iter().map(|v| [v.0, v.1]);
                            let both = Line::new(PlotPoints::from_iter(both))
                                .color(Color32::from_rgb(149, 96, 153))
                                .width(width)
                                .name("Aggregate");

                            plot_ui.line(both);
                        }

                        // Average lines
                        let darken = 0.5;
                        let alpha = 0.35;

                        if let Some(data) = result.download_avg.as_ref() {
                            let download = data.iter().map(|v| [v.0, v.1]);
                            let download = Line::new(PlotPoints::from_iter(download))
                                .color(
                                    Color32::from_rgb(95, 145, 62)
                                        .lerp_to_gamma(Color32::BLACK, darken)
                                        .gamma_multiply(alpha),
                                )
                                .allow_hover(false)
                                .width(3.5)
                                .name("Download");

                            plot_ui.line(download);
                        }
                        if let Some(data) = result.upload_avg.as_ref() {
                            let upload = data.iter().map(|v| [v.0, v.1]);
                            let upload = Line::new(PlotPoints::from_iter(upload))
                                .color(
                                    Color32::from_rgb(37, 83, 169)
                                        .lerp_to_gamma(Color32::BLACK, darken)
                                        .gamma_multiply(alpha),
                                )
                                .allow_hover(false)
                                .width(3.5)
                                .name("Upload");

                            plot_ui.line(upload);
                        }
                        if let Some(data) = result.both_download_avg.as_ref() {
                            let download = data.iter().map(|v| [v.0, v.1]);
                            let download = Line::new(PlotPoints::from_iter(download))
                                .color(
                                    Color32::from_rgb(95, 145, 62)
                                        .lerp_to_gamma(Color32::BLACK, darken)
                                        .gamma_multiply(alpha),
                                )
                                .allow_hover(false)
                                .width(3.5)
                                .name("Download");

                            plot_ui.line(download);
                        }
                        if let Some(data) = result.both_upload_avg.as_ref() {
                            let upload = data.iter().map(|v| [v.0, v.1]);
                            let upload = Line::new(PlotPoints::from_iter(upload))
                                .color(
                                    Color32::from_rgb(37, 83, 169)
                                        .lerp_to_gamma(Color32::BLACK, darken)
                                        .gamma_multiply(alpha),
                                )
                                .allow_hover(false)
                                .width(3.5)
                                .name("Upload");

                            plot_ui.line(upload);
                        }
                        if let Some(data) = result.both_avg.as_ref() {
                            let both = data.iter().map(|v| [v.0, v.1]);
                            let both = Line::new(PlotPoints::from_iter(both))
                                .color(
                                    Color32::from_rgb(149, 96, 153)
                                        .lerp_to_gamma(Color32::BLACK, darken)
                                        .gamma_multiply(alpha),
                                )
                                .allow_hover(false)
                                .width(3.5)
                                .name("Aggregate");

                            plot_ui.line(both);
                        }
                    });
                })
            }

            self.latency_and_loss(&mut strip, link, reset, false, y_axis_size);

            let result = self.result.as_ref().unwrap();

            if result.peer_latency.is_some() {
                self.latency_and_loss(&mut strip, link, reset, true, y_axis_size);
            }
        });
    }

    fn server(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        match self.server_state {
            ServerState::Stopped(ref error) => {
                let (server_button, peer_button) = ui
                    .horizontal_wrapped(|ui| (ui.button("Start server"), ui.button("Start peer")))
                    .inner;

                if let Some(error) = error {
                    ui.separator();
                    ui.label(format!("Unable to start server: {}", error));
                }

                if server_button.clicked() || peer_button.clicked() {
                    let ctx = ctx.clone();
                    let ctx_ = ctx.clone();
                    let ctx__ = ctx.clone();
                    let (tx, rx) = mpsc::unbounded_channel();
                    let (signal_started, started) = oneshot::channel();
                    let (signal_done, done) = oneshot::channel();

                    let stop = serve::serve_until(
                        protocol::PORT,
                        peer_button.clicked(),
                        Box::new(move |msg| {
                            tx.send(with_time(msg)).ok();
                            ctx.request_repaint();
                        }),
                        Box::new(move |result| {
                            signal_started.send(result).ok();
                            ctx_.request_repaint();
                        }),
                        Box::new(move || {
                            signal_done.send(()).ok();
                            ctx__.request_repaint();
                        }),
                    )
                    .ok();

                    if let Some(stop) = stop {
                        self.server = Some(Server {
                            done: Some(done),
                            stop: Some(stop),
                            started,
                            rx,
                            msgs: Vec::new(),
                        });
                        self.server_state = ServerState::Starting;
                    }
                };
                ui.separator();
                ui.label(format!(
                    "A server listens on TCP and UDP port {}. It allows clients \
                    to run tests and measure latency against it. It can also act as a latency peer for tests connecting to another server.",
                    protocol::PORT
                ));
            }
            ServerState::Running => {
                let server = self.server.as_mut().unwrap();
                let button = ui.button("Stop server");

                ui.separator();

                loop {
                    match server.rx.try_recv() {
                        Ok(msg) => {
                            println!("[Server] {msg}");
                            server.msgs.push(msg);
                        }
                        Err(TryRecvError::Disconnected) => panic!(),
                        Err(TryRecvError::Empty) => break,
                    }
                }

                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show_rows(
                        ui,
                        ui.text_style_height(&TextStyle::Body),
                        server.msgs.len(),
                        |ui, rows| {
                            for row in rows {
                                ui.label(&server.msgs[row]);
                            }
                        },
                    );

                if button.clicked() {
                    mem::take(&mut server.stop).unwrap().send(()).unwrap();
                    self.server_state = ServerState::Stopping;
                };
            }
            ServerState::Starting => {
                let server = self.server.as_mut().unwrap();

                if let Ok(result) = server.started.try_recv() {
                    if let Err(error) = result {
                        self.server_state = ServerState::Stopped(Some(error));
                        self.server = None;
                    } else {
                        self.server_state = ServerState::Running;
                    }
                }

                ui.add_enabled_ui(false, |ui| {
                    let _ = ui.button("Starting..");
                });
            }
            ServerState::Stopping => {
                if let Ok(()) = self
                    .server
                    .as_mut()
                    .unwrap()
                    .done
                    .as_mut()
                    .unwrap()
                    .try_recv()
                {
                    self.server_state = ServerState::Stopped(None);
                    self.server = None;
                }

                ui.add_enabled_ui(false, |ui| {
                    let _ = ui.button("Stopping..");
                });
            }
        }
    }

    fn remote(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        match self.remote_state {
            ServerState::Stopped(ref error) => {
                let button = ui
                    .vertical(|ui| {
                        let button = ui.button("Start server");
                        if let Some(error) = error {
                            ui.separator();
                            ui.label(format!("Unable to start server: {}", error));
                        }
                        button
                    })
                    .inner;

                if button.clicked() {
                    let ctx = ctx.clone();
                    let ctx_ = ctx.clone();
                    let ctx__ = ctx.clone();
                    let (tx, rx) = mpsc::unbounded_channel();
                    let (signal_started, started) = oneshot::channel();
                    let (signal_done, done) = oneshot::channel();

                    let stop = remote::serve_until(
                        protocol::PORT + 1,
                        Box::new(move |msg| {
                            tx.send(with_time(msg)).ok();
                            ctx.request_repaint();
                        }),
                        Box::new(move |result| {
                            signal_started.send(result).ok();
                            ctx_.request_repaint();
                        }),
                        Box::new(move || {
                            signal_done.send(()).ok();
                            ctx__.request_repaint();
                        }),
                    )
                    .ok();

                    if let Some(stop) = stop {
                        self.remote_server = Some(Server {
                            done: Some(done),
                            stop: Some(stop),
                            started,
                            rx,
                            msgs: Vec::new(),
                        });
                        self.remote_state = ServerState::Starting;
                    }
                };
                ui.separator();
                ui.label(format!(
                    "A remote server runs a web server on TCP port {}. It allows web clients to remotely start \
                    tests against other servers.",
                    protocol::PORT + 1
                ));
            }
            ServerState::Running => {
                let remote_server = self.remote_server.as_mut().unwrap();
                let button = ui.button("Stop server");

                ui.separator();

                loop {
                    match remote_server.rx.try_recv() {
                        Ok(msg) => {
                            println!("[Remote] {msg}");
                            remote_server.msgs.push(msg);
                        }
                        Err(TryRecvError::Disconnected) => panic!(),
                        Err(TryRecvError::Empty) => break,
                    }
                }

                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show_rows(
                        ui,
                        ui.text_style_height(&TextStyle::Body),
                        remote_server.msgs.len(),
                        |ui, rows| {
                            for row in rows {
                                ui.label(&remote_server.msgs[row]);
                            }
                        },
                    );

                if button.clicked() {
                    mem::take(&mut remote_server.stop)
                        .unwrap()
                        .send(())
                        .unwrap();
                    self.remote_state = ServerState::Stopping;
                };
            }
            ServerState::Starting => {
                let remote_server = self.remote_server.as_mut().unwrap();

                if let Ok(result) = remote_server.started.try_recv() {
                    if let Err(error) = result {
                        self.remote_state = ServerState::Stopped(Some(error));
                        self.remote_server = None;
                    } else {
                        self.remote_state = ServerState::Running;
                    }
                }

                ui.add_enabled_ui(false, |ui| {
                    let _ = ui.button("Starting..");
                });
            }
            ServerState::Stopping => {
                if let Ok(()) = self
                    .remote_server
                    .as_mut()
                    .unwrap()
                    .done
                    .as_mut()
                    .unwrap()
                    .try_recv()
                {
                    self.remote_state = ServerState::Stopped(None);
                    self.remote_server = None;
                }

                ui.add_enabled_ui(false, |ui| {
                    let _ = ui.button("Stopping..");
                });
            }
        }
    }

    fn start_monitor(&mut self, ctx: &egui::Context) {
        self.save_settings();

        let (signal_done, done) = oneshot::channel();

        let ctx_ = ctx.clone();
        let data = Arc::new(latency::Data::new(
            ((self.settings.latency_monitor.history * 1000.0)
                / self.settings.latency_monitor.latency_sample_interval as f64)
                .round() as usize,
            Arc::new(move || {
                ctx_.request_repaint();
            }),
        ));

        let ctx_ = ctx.clone();
        let abort = latency::test_callback(
            latency::Config {
                port: protocol::PORT,
                ping_interval: Duration::from_millis(
                    self.settings.latency_monitor.latency_sample_interval,
                ),
            },
            (!self.settings.latency_monitor.server.trim().is_empty())
                .then_some(&self.settings.latency_monitor.server),
            data.clone(),
            Box::new(move |result| {
                signal_done.send(result).map_err(|_| ()).unwrap();
                ctx_.request_repaint();
            }),
        );

        self.latency = Some(Latency {
            done: Some(done),
            abort: Some(abort),
        });
        self.latency_state = ClientState::Running;
        self.latency_data = data;
        self.latency_error = None;
        self.latency_plot_reset = true;
    }

    fn monitor(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let running = self.latency_state != ClientState::Stopped;

        if !running {
            ui.horizontal_wrapped(|ui| {
                ui.label("Server address:");
                let response = ui.add(
                    TextEdit::singleline(&mut self.settings.latency_monitor.server)
                        .hint_text("(Locate local server)"),
                );
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                if ui.button("Start test").clicked() || enter {
                    self.start_monitor(ctx)
                }
            });
        }

        if running {
            ui.horizontal(|ui| {
                match self.latency_state {
                    ClientState::Running => {
                        if ui.button("Stop test").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Space))
                        {
                            let latency = self.latency.as_mut().unwrap();
                            mem::take(&mut latency.abort).unwrap().send(()).unwrap();
                            self.latency_state = ClientState::Stopping;
                        }
                    }
                    ClientState::Stopping => {
                        ui.add_enabled_ui(false, |ui| {
                            let _ = ui.button("Stopping test..");
                        });
                    }
                    ClientState::Stopped => {}
                }

                let state = match *self.latency_data.state.lock() {
                    latency::State::Connecting => "Connecting..".to_owned(),
                    latency::State::Monitoring { ref at } => format!("Connected to {at}"),
                    latency::State::Syncing => "Synchronizing clocks..".to_owned(),
                };
                ui.add(Label::new(state).wrap_mode(TextWrapMode::Truncate));

                let latency = self.latency.as_mut().unwrap();

                if let Ok(result) = latency.done.as_mut().unwrap().try_recv() {
                    self.latency_error = match result {
                        Some(Ok(())) => None,
                        Some(Err(error)) => Some(error),
                        None => Some("Aborted...".to_owned()),
                    };
                    self.latency_stop = self.latency_data.start.elapsed();
                    self.latency = None;
                    self.latency_state = ClientState::Stopped;
                }
            });
        }

        ui.separator();

        ui.add_enabled_ui(!running, |ui| {
            Grid::new("latency-settings-compact").show(ui, |ui| {
                ui.label("History: ");
                ui.add(
                    egui::DragValue::new(&mut self.settings.latency_monitor.history)
                        .range(0..=1000)
                        .speed(0.05),
                );
                ui.label("seconds");
                ui.end_row();
                ui.label("Latency sample interval:");
                ui.add(
                    egui::DragValue::new(
                        &mut self.settings.latency_monitor.latency_sample_interval,
                    )
                    .range(1..=1000)
                    .speed(0.05),
                );
                ui.label("milliseconds");
            });
        });

        ui.separator();

        if let Some(error) = self.latency_error.as_ref() {
            ui.label(format!("Error: {}", error));
            ui.separator();
        }

        self.latency_data(ctx, ui);
    }

    fn latency_data(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        ui.vertical(|ui| {
            let packet_loss_size = 80.0;
            let height = ui.available_height();

            let duration = self.settings.latency_monitor.history;

            let points = self.latency_data.points.blocking_lock().clone();

            let now = if self.latency_state == ClientState::Running {
                ctx.request_repaint();
                self.latency_data.start.elapsed()
            } else {
                self.latency_stop
            }
            .as_secs_f64();

            let reset = mem::take(&mut self.latency_plot_reset);

            let link = ui.id().with("latency-link");

            let y_axis_size = 30.0;

            // Latency
            let mut plot = Plot::new("latency-ping")
                .legend(Legend::default().insertion_order(true))
                .link_axis(link, true, false)
                .link_cursor(link, true, false)
                .include_x(-duration)
                .include_x(0.0)
                .include_x(duration * 0.20)
                .include_y(0.0)
                .include_y(10.0)
                .height(height - packet_loss_size)
                .y_axis_min_width(y_axis_size)
                .auto_bounds(Vec2b::new(false, true))
                .label_formatter(|_, value| {
                    format!("Latency = {:.2} ms\nTime = {:.2} s", value.y, value.x)
                });

            if reset {
                plot = plot.reset();
            }

            ui.label("Latency");
            plot.show(ui, |plot_ui| {
                let latency = points.iter().filter_map(|point| {
                    point.up.map(|up| {
                        let up = if let Some(total) = point.total {
                            up.min(total)
                        } else {
                            up
                        };
                        [point.sent.as_secs_f64() - now, 1000.0 * up.as_secs_f64()]
                    })
                });
                let latency = Line::new(PlotPoints::from_iter(latency))
                    .color(Color32::from_rgb(37, 83, 169))
                    .name("Up");

                plot_ui.line(latency);

                let latency = points.iter().filter_map(|point| {
                    point
                        .up
                        .and_then(|up| point.total.map(|total| total.saturating_sub(up)))
                        .map(|down| [point.sent.as_secs_f64() - now, 1000.0 * down.as_secs_f64()])
                });
                let latency = Line::new(PlotPoints::from_iter(latency))
                    .color(Color32::from_rgb(95, 145, 62))
                    .name("Down");

                plot_ui.line(latency);

                let latency = points.iter().filter_map(|point| {
                    point
                        .total
                        .map(|total| [point.sent.as_secs_f64() - now, 1000.0 * total.as_secs_f64()])
                });
                let latency = Line::new(PlotPoints::from_iter(latency))
                    .color(Color32::from_rgb(50, 50, 50))
                    .name("Round-trip");

                plot_ui.line(latency);
            });

            // Packet loss
            let mut plot = Plot::new("latency-loss")
                .legend(Legend::default())
                .show_axes([false, true])
                .show_grid(Vec2b::new(true, false))
                .y_axis_min_width(y_axis_size)
                .y_axis_formatter(|_, _| String::new())
                .link_axis(link, true, false)
                .link_cursor(link, true, false)
                .center_y_axis(true)
                .allow_zoom(false)
                .allow_boxed_zoom(false)
                .include_x(-duration)
                .include_x(0.0)
                .include_x(duration * 0.15)
                .include_y(-1.0)
                .include_y(1.0)
                .height(30.0)
                .label_formatter(|_, value| format!("Time = {:.2} s", value.x));

            if reset {
                plot = plot.reset();
            }

            ui.label("Packet loss");
            plot.show(ui, |plot_ui| {
                let loss = points
                    .iter()
                    .filter(|point| !point.pending && point.total.is_none());

                for point in loss {
                    let loss = point.sent.as_secs_f64() - now;

                    let (color, s, e) = if point.up.is_some() {
                        (Color32::from_rgb(95, 145, 62), 1.0, 0.0)
                    } else {
                        (Color32::from_rgb(37, 83, 169), -1.0, 0.0)
                    };

                    plot_ui.line(
                        Line::new(PlotPoints::from_iter(
                            [[loss, s], [loss, e]].iter().copied(),
                        ))
                        .color(color),
                    );

                    plot_ui.line(
                        Line::new(PlotPoints::from_iter(
                            [[loss, s], [loss, s - s / 5.0]].iter().copied(),
                        ))
                        .width(3.0)
                        .color(color),
                    );
                }
            });
        });
    }

    pub fn show(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        ctx.input(|input| {
            if let Some(file) = input
                .raw
                .dropped_files
                .first()
                .and_then(|file| file.path.as_deref())
            {
                RawResult::load(file).map(|raw| {
                    self.load_file(file.to_owned(), raw);
                    self.tab = Tab::Result;
                });
            }
        });

        let compact = ui.available_width() < 660.0;
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Client, "Client");
            ui.selectable_value(&mut self.tab, Tab::Server, "Server");
            ui.selectable_value(&mut self.tab, Tab::Remote, "Remote");
            ui.selectable_value(&mut self.tab, Tab::Monitor, "Monitor");
            ui.selectable_value(&mut self.tab, Tab::Result, "Result");
        });
        ui.separator();

        match self.tab {
            Tab::Client => self.client(ctx, ui, compact),
            Tab::Server => self.server(ctx, ui),
            Tab::Remote => self.remote(ctx, ui),
            Tab::Monitor => self.monitor(ctx, ui),
            Tab::Result => self.result(ctx, ui),
        }
    }
}
