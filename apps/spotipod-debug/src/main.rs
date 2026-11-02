//! spotipod-debug: keep it open; it picks up SpotiPod whenever it runs and shows what
//! happens under the hood (CPU / memory, requests, loading, logs, UI state).

mod chart;
mod feed;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use chart::{Chart, Kind, Series, fmt_time, fmt_value};
use eframe::egui::{self, Color32, RichText, Ui};
use feed::{Model, Shared};

const BLUE: Color32 = Color32::from_rgb(80, 150, 255);
const GREEN: Color32 = Color32::from_rgb(90, 200, 120);
const ORANGE: Color32 = Color32::from_rgb(255, 77, 0);
const RED: Color32 = Color32::from_rgb(235, 80, 80);
const GRAY: Color32 = Color32::from_rgb(150, 150, 150);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Performance,
    Network,
    Loading,
    State,
    Logs,
}

const TABS: [(Tab, &str); 6] = [
    (Tab::Overview, "Overview"),
    (Tab::Performance, "Performance"),
    (Tab::Network, "Network"),
    (Tab::Loading, "Loading"),
    (Tab::State, "State"),
    (Tab::Logs, "Logs"),
];

/// Chart windows, seconds (0 = everything).
const WINDOWS: [(f64, &str); 4] = [
    (60.0, "1 min"),
    (300.0, "5 min"),
    (900.0, "15 min"),
    (0.0, "All"),
];

struct App {
    model: Shared,
    tab: Tab,
    window: f64,
    request_filter: String,
    errors_only: bool,
    log_filter: String,
    log_levels: [bool; 5],
    autoscroll: bool,
}

fn main() -> eframe::Result {
    let model: Shared = Arc::new(Mutex::new(Model::default()));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SpotiPod Debug")
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([700.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "SpotiPod Debug",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            feed::start(model.clone(), move || ctx.request_repaint());
            Ok(Box::new(App {
                model,
                tab: Tab::Overview,
                window: 300.0,
                request_filter: String::new(),
                errors_only: false,
                log_filter: String::new(),
                log_levels: [true; 5],
                autoscroll: true,
            }))
        }),
    )
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let model = self.model.clone();
        let mut m = model.lock().unwrap();

        egui::Panel::top("status").show(root, |ui| {
            ui.horizontal(|ui| {
                let (color, text) = match (&m.session, m.connected) {
                    (Some(s), true) => (
                        GREEN,
                        format!(
                            "SpotiPod {}  ·  PID {}  ·  up {}",
                            s.version,
                            s.pid,
                            fmt_time(s.now())
                        ),
                    ),
                    (Some(s), false) => (
                        GRAY,
                        format!("SpotiPod {} stopped (PID {})", s.version, s.pid),
                    ),
                    (None, _) => (
                        GRAY,
                        format!("Waiting for SpotiPod on 127.0.0.1:{}", feed::PORT),
                    ),
                };
                ui.colored_label(color, "●");
                ui.label(text);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Clear").clicked() {
                        m.clear();
                    }
                    for (secs, label) in WINDOWS.iter().rev() {
                        ui.selectable_value(&mut self.window, *secs, *label);
                    }
                    ui.label("Window:");
                });
            });
        });

        egui::Panel::left("tabs")
            .resizable(false)
            .exact_size(130.0)
            .show(root, |ui| {
                ui.add_space(6.0);
                for (tab, label) in TABS {
                    ui.selectable_value(&mut self.tab, tab, label);
                }
            });

        egui::CentralPanel::default().show(root, |ui| {
            let now = m.session.as_ref().map_or(0.0, |s| s.now());
            let x = if self.window > 0.0 {
                ((now - self.window).max(0.0), now.max(self.window))
            } else {
                (0.0, now.max(1.0))
            };
            match self.tab {
                Tab::Overview => {
                    _ = egui::ScrollArea::vertical().show(ui, |ui| overview(ui, &m, x))
                }
                Tab::Performance => {
                    _ = egui::ScrollArea::vertical().show(ui, |ui| performance(ui, &m, x))
                }
                Tab::Network => network(ui, &m, x, &mut self.request_filter, &mut self.errors_only),
                Tab::Loading => _ = egui::ScrollArea::vertical().show(ui, |ui| loading(ui, &m, x)),
                Tab::State => state(ui, &m),
                Tab::Logs => logs(
                    ui,
                    &m,
                    &mut self.log_filter,
                    &mut self.log_levels,
                    &mut self.autoscroll,
                ),
            }
        });

        // Uptime ticks even without new data.
        if m.connected {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
}

// --- Derived numbers ---

/// Per-second buckets of the requests: (count, errors, bytes, latency sum).
fn per_second(m: &Model) -> BTreeMap<u64, (usize, usize, usize, u64)> {
    let mut b: BTreeMap<u64, (usize, usize, usize, u64)> = BTreeMap::new();
    for r in &m.requests {
        let e = b.entry(r.t as u64).or_default();
        e.0 += 1;
        e.1 += usize::from(!r.ok);
        e.2 += r.bytes;
        e.3 += r.ms;
    }
    b
}

fn bytes(n: f64) -> String {
    if n >= 1_048_576.0 {
        format!("{:.1} MB", n / 1_048_576.0)
    } else if n >= 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else {
        format!("{n:.0} B")
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn card(ui: &mut Ui, title: &str, value: String, sub: String) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(170.0);
        ui.set_height(64.0);
        ui.label(RichText::new(title).small().weak());
        ui.label(RichText::new(value).size(20.0).strong());
        ui.label(RichText::new(sub).small().weak());
    });
}

// --- Tabs ---

fn overview(ui: &mut Ui, m: &Model, x: (f64, f64)) {
    let s = m.state.as_ref();
    let get = |path: &[&str]| -> String {
        let mut v = match s {
            Some(v) => v,
            None => return "-".into(),
        };
        for k in path {
            v = &v[*k];
        }
        match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => "-".into(),
            v => v.to_string(),
        }
    };
    let last = m.samples.back();
    let errors = m.requests.iter().filter(|r| !r.ok).count();
    let total_bytes: usize = m.requests.iter().map(|r| r.bytes).sum();
    let recent: Vec<u64> = m
        .requests
        .iter()
        .filter(|r| r.t >= x.1 - 60.0)
        .map(|r| r.ms)
        .collect();
    let avg = if recent.is_empty() {
        0
    } else {
        recent.iter().sum::<u64>() / recent.len() as u64
    };
    let requested: usize = m.resolves.iter().map(|r| r.requested).sum();
    let cached: usize = m.resolves.iter().map(|r| r.cached).sum();
    let fetched: usize = m.resolves.iter().map(|r| r.fetched).sum();
    let lists = m
        .loads
        .iter()
        .filter(|l| l.stage == "open" || l.stage == "warm")
        .count();
    let boot = m
        .loads
        .iter()
        .find(|l| l.stage == "boot" && l.detail == "menus ready")
        .map_or("-".into(), |l| format!("{} ms", l.ms));

    ui.horizontal_wrapped(|ui| {
        card(ui, "Phase", get(&["phase"]), get(&["progress"]));
        card(
            ui,
            "Playback",
            get(&["playback", "status"]),
            get(&["playback", "track", "name"]),
        );
        card(
            ui,
            "CPU",
            last.map_or("-".into(), |s| format!("{:.1} %", s.cpu)),
            "of one core".into(),
        );
        card(
            ui,
            "Memory",
            last.map_or("-".into(), |s| format!("{:.0} MB", s.memory_mb)),
            "resident".into(),
        );
        card(
            ui,
            "Requests",
            m.requests.len().to_string(),
            format!("{errors} failed · {}", bytes(total_bytes as f64)),
        );
        card(
            ui,
            "Latency",
            format!("{avg} ms"),
            "average, last minute".into(),
        );
        card(ui, "Menus ready", boot, "login to menu".into());
        card(
            ui,
            "Lists loaded",
            lists.to_string(),
            format!(
                "{} warm-up running",
                if m.loads
                    .iter()
                    .any(|l| l.stage == "warm" && l.detail == "all lists")
                {
                    "done,"
                } else {
                    "still"
                }
            ),
        );
        card(
            ui,
            "Tracks resolved",
            requested.to_string(),
            format!(
                "{:.0}% cache · {fetched} fetched",
                if requested > 0 {
                    cached as f64 * 100.0 / requested as f64
                } else {
                    0.0
                }
            ),
        );
    });
    ui.add_space(8.0);
    cpu_chart(ui, m, x, 140.0);
    requests_chart(ui, m, x, 140.0);
}

fn cpu_chart(ui: &mut Ui, m: &Model, x: (f64, f64), height: f32) {
    Chart {
        title: "CPU",
        unit: "%",
        kind: Kind::Line,
        height,
        x,
        min_max: 10.0,
    }
    .show(
        ui,
        &[Series {
            name: "CPU",
            color: ORANGE,
            points: m.samples.iter().map(|s| [s.t, s.cpu as f64]).collect(),
        }],
    );
}

fn requests_chart(ui: &mut Ui, m: &Model, x: (f64, f64), height: f32) {
    let b = per_second(m);
    Chart {
        title: "Requests per second",
        unit: "req",
        kind: Kind::Bars,
        height,
        x,
        min_max: 5.0,
    }
    .show(
        ui,
        &[
            Series {
                name: "ok",
                color: BLUE,
                points: b.iter().map(|(t, v)| [*t as f64, v.0 as f64]).collect(),
            },
            Series {
                name: "failed",
                color: RED,
                points: b
                    .iter()
                    .filter(|(_, v)| v.1 > 0)
                    .map(|(t, v)| [*t as f64, v.1 as f64])
                    .collect(),
            },
        ],
    );
}

fn performance(ui: &mut Ui, m: &Model, x: (f64, f64)) {
    cpu_chart(ui, m, x, 220.0);
    ui.add_space(8.0);
    Chart {
        title: "Memory",
        unit: "MB",
        kind: Kind::Line,
        height: 220.0,
        x,
        min_max: 50.0,
    }
    .show(
        ui,
        &[Series {
            name: "Memory",
            color: BLUE,
            points: m.samples.iter().map(|s| [s.t, s.memory_mb]).collect(),
        }],
    );
}

fn network(ui: &mut Ui, m: &Model, x: (f64, f64), filter: &mut String, errors_only: &mut bool) {
    requests_chart(ui, m, x, 120.0);
    let b = per_second(m);
    Chart {
        title: "Latency (average per second)",
        unit: "ms",
        kind: Kind::Line,
        height: 120.0,
        x,
        min_max: 100.0,
    }
    .show(
        ui,
        &[Series {
            name: "latency",
            color: ORANGE,
            points: b
                .iter()
                .map(|(t, v)| [*t as f64, v.3 as f64 / v.0.max(1) as f64])
                .collect(),
        }],
    );
    Chart {
        title: "Data received per second",
        unit: "KB",
        kind: Kind::Bars,
        height: 100.0,
        x,
        min_max: 10.0,
    }
    .show(
        ui,
        &[Series {
            name: "received",
            color: GREEN,
            points: b
                .iter()
                .map(|(t, v)| [*t as f64, v.2 as f64 / 1024.0])
                .collect(),
        }],
    );

    // Per endpoint.
    ui.add_space(6.0);
    ui.label(RichText::new("Endpoints").strong());
    let mut by: BTreeMap<&str, Vec<&feed::Request>> = BTreeMap::new();
    for r in &m.requests {
        by.entry(r.endpoint.as_str()).or_default().push(r);
    }
    egui::Grid::new("endpoints")
        .striped(true)
        .num_columns(6)
        .show(ui, |ui| {
            for h in ["endpoint", "count", "failed", "avg ms", "p95 ms", "data"] {
                ui.label(RichText::new(h).weak());
            }
            ui.end_row();
            for (endpoint, rs) in &by {
                let mut ms: Vec<u64> = rs.iter().map(|r| r.ms).collect();
                ms.sort_unstable();
                let failed = rs.iter().filter(|r| !r.ok).count();
                ui.label(*endpoint);
                ui.label(rs.len().to_string());
                if failed > 0 {
                    ui.colored_label(RED, failed.to_string());
                } else {
                    ui.label("0");
                }
                ui.label((ms.iter().sum::<u64>() / ms.len().max(1) as u64).to_string());
                ui.label(percentile(&ms, 0.95).to_string());
                ui.label(bytes(rs.iter().map(|r| r.bytes).sum::<usize>() as f64));
                ui.end_row();
            }
        });

    // Recent requests, newest first.
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Requests").strong());
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text("filter")
                .desired_width(200.0),
        );
        ui.checkbox(errors_only, "failed only");
    });
    let rows: Vec<&feed::Request> = m
        .requests
        .iter()
        .rev()
        .filter(|r| !*errors_only || !r.ok)
        .filter(|r| filter.is_empty() || r.endpoint.contains(filter.as_str()))
        .collect();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 4.0;
    egui::ScrollArea::vertical()
        .id_salt("requests")
        .auto_shrink(false)
        .show_rows(ui, row_h, rows.len(), |ui, range| {
            for r in &rows[range] {
                let color = if r.ok { ui.visuals().text_color() } else { RED };
                let line = format!(
                    "{:>8}  {:<28} {:>6} ms {:>6} items {:>10}  {}",
                    fmt_time(r.t),
                    r.endpoint,
                    r.ms,
                    r.items,
                    bytes(r.bytes as f64),
                    r.error.as_deref().unwrap_or("")
                );
                ui.label(RichText::new(line).monospace().color(color));
            }
        });
}

fn loading(ui: &mut Ui, m: &Model, x: (f64, f64)) {
    // Cumulative tracks resolved: cache vs. network.
    let mut cached = 0.0;
    let mut fetched = 0.0;
    let mut c = Vec::new();
    let mut f = Vec::new();
    for r in &m.resolves {
        cached += r.cached as f64;
        fetched += r.fetched as f64;
        c.push([r.t, cached]);
        f.push([r.t, fetched]);
    }
    Chart {
        title: "Tracks resolved (cumulative)",
        unit: "",
        kind: Kind::Line,
        height: 160.0,
        x,
        min_max: 10.0,
    }
    .show(
        ui,
        &[
            Series {
                name: "from cache",
                color: GREEN,
                points: c,
            },
            Series {
                name: "from Spotify",
                color: BLUE,
                points: f,
            },
        ],
    );

    let failed: usize = m.resolves.iter().map(|r| r.failed).sum();
    let ms: u64 = m.resolves.iter().map(|r| r.ms).sum();
    ui.label(format!(
        "{} lookups · {} failed · {} ms total",
        m.resolves.len(),
        failed,
        ms
    ));

    ui.add_space(8.0);
    ui.label(RichText::new("Steps").strong());
    egui::Grid::new("loads")
        .striped(true)
        .num_columns(5)
        .show(ui, |ui| {
            for h in ["at", "stage", "what", "items", "took"] {
                ui.label(RichText::new(h).weak());
            }
            ui.end_row();
            for l in m.loads.iter().rev() {
                ui.label(fmt_time(l.t));
                let color = match l.stage.as_str() {
                    "boot" => ORANGE,
                    "open" => BLUE,
                    "warm" => GREEN,
                    _ => ui.visuals().text_color(),
                };
                ui.colored_label(color, &l.stage);
                ui.label(&l.detail);
                ui.label(l.items.to_string());
                ui.label(format!("{} ms", fmt_value(l.ms as f64)));
                ui.end_row();
            }
        });
}

fn state(ui: &mut Ui, m: &Model) {
    let text = m
        .state
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_else(|| "No state yet.".into());
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.label(RichText::new(text).monospace());
    });
}

const LEVELS: [&str; 5] = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

fn logs(
    ui: &mut Ui,
    m: &Model,
    filter: &mut String,
    levels: &mut [bool; 5],
    autoscroll: &mut bool,
) {
    ui.horizontal(|ui| {
        for (i, l) in LEVELS.iter().enumerate() {
            ui.checkbox(&mut levels[i], *l);
        }
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text("search")
                .desired_width(220.0),
        );
        ui.checkbox(autoscroll, "follow");
    });
    let rows: Vec<&feed::Log> = m
        .logs
        .iter()
        .filter(|l| {
            LEVELS
                .iter()
                .position(|x| *x == l.level)
                .is_none_or(|i| levels[i])
        })
        .filter(|l| {
            filter.is_empty()
                || l.message.contains(filter.as_str())
                || l.target.contains(filter.as_str())
        })
        .collect();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 4.0;
    egui::ScrollArea::both()
        .auto_shrink(false)
        .stick_to_bottom(*autoscroll)
        .show_rows(ui, row_h, rows.len(), |ui, range| {
            for l in &rows[range] {
                let color = match l.level.as_str() {
                    "ERROR" => RED,
                    "WARN" => ORANGE,
                    "DEBUG" | "TRACE" => GRAY,
                    _ => ui.visuals().text_color(),
                };
                let line = format!(
                    "{:>8} {:<5} {}: {}",
                    fmt_time(l.t),
                    l.level,
                    l.target,
                    l.message
                );
                ui.label(RichText::new(line).monospace().color(color));
            }
        });
}
