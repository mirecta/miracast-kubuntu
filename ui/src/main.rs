//! Miracast desktop UI (egui). Uses the `miracast` library from the parent crate.

use eframe::egui;
use miracast::{AudioMode, CancellationToken, CastOpts, Status, p2p::Peer};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const APP_ID: &str = "io.github.miracast-rs.ui";

enum Msg {
    Peers(Vec<Peer>),
    ScanDone(Result<(), String>),
    Status(Status),
    CastDone(Result<(), String>),
}

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Searching,
    Connecting,
    Streaming,
    Stopping,
}

struct App {
    rt: tokio::runtime::Runtime,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    ctx: egui::Context,

    peers: Vec<Peer>,
    selected: Option<String>, // MAC
    auto_selected: Option<String>,
    show_all: bool,
    scanning: Option<CancellationToken>,

    extend: bool,
    outputs: Vec<String>,
    mirror_output: usize,
    res: i32,
    fps: i32,
    bitrate: u32,
    audio: AudioMode,
    high: bool,

    phase: Phase,
    status: String,
    log: Vec<String>,
    cast_cancel: Option<CancellationToken>,
    cast_task: Option<tokio::task::JoinHandle<()>>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let (tx, rx) = mpsc::channel();
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().expect("tokio runtime");
        let outputs = miracast::screen::list_outputs().unwrap_or_default();
        let mut app = Self {
            rt,
            tx,
            rx,
            ctx: cc.egui_ctx.clone(),
            peers: vec![],
            selected: None,
            auto_selected: None,
            show_all: false,
            scanning: None,
            extend: true,
            outputs,
            mirror_output: 0,
            res: 1080,
            fps: 30,
            bitrate: 8000,
            audio: AudioMode::Tv,
            high: false,
            phase: Phase::Idle,
            status: "Turn on Screen Mirroring on the TV".into(),
            log: vec![],
            cast_cancel: None,
            cast_task: None,
        };
        app.ensure_kwin_permission();
        app.start_scan();
        app
    }

    fn push_log(&mut self, s: impl Into<String>) {
        let s = s.into();
        // Also keep a log file so problems can be looked at after the window is closed.
        let f = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state/miracast/ui.log");
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
            use std::io::Write;
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let _ = writeln!(fh, "{t} {s}");
        }
        self.log.push(s);
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    /// KWin only lets registered executables create virtual monitors.
    fn ensure_kwin_permission(&mut self) {
        let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else { return };
        let file = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".local/share/applications")
            .join(format!("{APP_ID}.desktop"));
        let ok = std::fs::read_to_string(&file).map(|s| s.contains(&format!("Exec={}\n", exe.display()))).unwrap_or(false);
        if !ok {
            match miracast::screen::install_desktop_file(&exe, APP_ID, Some("Miracast")) {
                Ok(f) => self.push_log(format!("Registered with KWin: {}", f.display())),
                Err(e) => self.push_log(format!("Cannot register with KWin: {e}")),
            }
        }
    }

    fn start_scan(&mut self) {
        if self.scanning.is_some() || self.phase != Phase::Idle {
            return;
        }
        let cancel = CancellationToken::new();
        self.scanning = Some(cancel.clone());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let (tx2, ctx2) = (tx.clone(), ctx.clone());
            let r = miracast::scan(
                30,
                move |peers| {
                    let _ = tx2.send(Msg::Peers(peers.to_vec()));
                    ctx2.request_repaint();
                },
                cancel,
            )
            .await;
            let _ = tx.send(Msg::ScanDone(r.map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn stop_scan(&mut self) {
        if let Some(c) = self.scanning.take() {
            c.cancel();
        }
    }

    fn connect(&mut self) {
        self.stop_scan();
        let opts = CastOpts {
            peer: self.selected.clone(),
            mirror: if self.extend { None } else { Some(self.outputs.get(self.mirror_output).cloned().unwrap_or_default()) },
            res: self.res,
            fps: self.fps,
            bitrate: self.bitrate,
            high: self.high,
            audio: self.audio,
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        self.cast_cancel = Some(cancel.clone());
        self.phase = Phase::Searching;
        self.status = "Starting …".into();
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (tx2, ctx2) = (tx.clone(), ctx.clone());
        let report: miracast::Reporter = Arc::new(move |s| {
            let _ = tx2.send(Msg::Status(s));
            ctx2.request_repaint();
        });
        self.cast_task = Some(self.rt.spawn(async move {
            // small delay so the scan's StopFind doesn't race with the cast's StartFind
            tokio::time::sleep(Duration::from_millis(300)).await;
            let r = miracast::cast(opts, report, cancel).await;
            let _ = tx.send(Msg::CastDone(r.map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        }));
    }

    fn disconnect(&mut self) {
        if let Some(c) = &self.cast_cancel {
            c.cancel();
            self.phase = Phase::Stopping;
            self.status = "Disconnecting …".into();
        }
    }

    fn pump(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Peers(p) => {
                    self.peers = p;
                    // Prefer the TV used last time; otherwise the first display found.
                    let last = miracast::last_tv();
                    let remembered = self.peers.iter().find(|p| Some(p.hw_address.to_lowercase()) == last);
                    let user_picked = self.selected.as_ref().is_some_and(|s| Some(s.to_lowercase()) != self.auto_selected.clone().map(|a| a.to_lowercase()));
                    if !user_picked {
                        if let Some(p) = remembered.or_else(|| self.peers.iter().find(|p| p.is_sink())) {
                            self.selected = Some(p.hw_address.clone());
                            self.auto_selected = self.selected.clone();
                        }
                    }
                }
                Msg::ScanDone(r) => {
                    self.scanning = None;
                    if let Err(e) = r {
                        self.status = format!("Scan failed: {e}");
                        self.push_log(format!("Scan failed: {e}"));
                    }
                }
                Msg::Status(s) => {
                    let text = match &s {
                        Status::Searching => {
                            self.phase = Phase::Searching;
                            "Searching for the TV …".to_string()
                        }
                        Status::Connecting(n) => {
                            self.phase = Phase::Connecting;
                            format!("Connecting to {n} – confirm on the TV if asked")
                        }
                        Status::WaitingForTv => "Waiting for the TV …".to_string(),
                        Status::Streaming(d) => {
                            self.phase = Phase::Streaming;
                            format!("Streaming {d}")
                        }
                        Status::Info(i) => i.clone(),
                    };
                    if !matches!(s, Status::Info(_)) {
                        self.status = text.clone();
                    }
                    self.push_log(text);
                }
                Msg::CastDone(r) => {
                    self.phase = Phase::Idle;
                    self.cast_cancel = None;
                    self.cast_task = None;
                    match r {
                        Ok(()) => self.status = "Disconnected".into(),
                        Err(e) => {
                            self.status = format!("Error: {}", e.lines().next().unwrap_or(""));
                            self.push_log(format!("Error: {e}"));
                        }
                    }
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        let busy = self.phase != Phase::Idle;

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if matches!(self.phase, Phase::Searching | Phase::Connecting | Phase::Stopping) {
                    ui.spinner();
                } else if self.phase == Phase::Streaming {
                    ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "●");
                }
                ui.label(&self.status);
            });
            egui::CollapsingHeader::new("Log").show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(140.0).stick_to_bottom(true).show(ui, |ui| {
                    for l in &self.log {
                        ui.label(egui::RichText::new(l).monospace().small());
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Displays");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.scanning.is_some() {
                        ui.spinner();
                    } else if ui.add_enabled(!busy, egui::Button::new("⟳ Search")).clicked() {
                        self.start_scan();
                    }
                    ui.checkbox(&mut self.show_all, "all devices");
                });
            });
            ui.separator();

            let visible: Vec<&Peer> = self.peers.iter().filter(|p| self.show_all || p.is_sink()).collect();
            egui::ScrollArea::vertical().max_height(160.0).id_salt("peers").show(ui, |ui| {
                if visible.is_empty() {
                    ui.weak(if self.scanning.is_some() {
                        "Searching … (on the TV: Source → Screen Mirroring)"
                    } else {
                        "No displays found. Turn on Screen Mirroring on the TV and press Search."
                    });
                }
                for p in visible {
                    let sel = self.selected.as_deref() == Some(p.hw_address.as_str());
                    let busy_tv = p.wfd.map(|w| !w.session_available).unwrap_or(false);
                    let mut label = format!(
                        "📺 {}   {}%{}",
                        if p.name.is_empty() { &p.hw_address } else { &p.name },
                        p.strength,
                        if busy_tv { "  (busy)" } else { "" },
                    );
                    let extra = format!("{} {}", p.manufacturer, p.model);
                    if !extra.trim().is_empty() {
                        label.push_str(&format!("\n     {}", extra.trim()));
                    }
                    if ui.add_enabled(!busy, egui::Button::selectable(sel, label)).clicked() {
                        self.selected = Some(p.hw_address.clone());
                    }
                }
            });

            ui.add_space(8.0);
            ui.add_enabled_ui(!busy, |ui| {
                egui::Grid::new("opts").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Mode");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.extend, true, "Extend (new monitor)");
                        ui.radio_value(&mut self.extend, false, "Mirror");
                        if !self.extend {
                            let cur = self.outputs.get(self.mirror_output).cloned().unwrap_or_else(|| "first screen".into());
                            egui::ComboBox::from_id_salt("out").selected_text(cur).show_ui(ui, |ui| {
                                for (i, o) in self.outputs.iter().enumerate() {
                                    ui.selectable_value(&mut self.mirror_output, i, o);
                                }
                            });
                        }
                    });
                    ui.end_row();

                    ui.label("Resolution");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.res, 1080, "1080p");
                        ui.radio_value(&mut self.res, 720, "720p");
                        ui.separator();
                        ui.radio_value(&mut self.fps, 30, "30 fps");
                        ui.radio_value(&mut self.fps, 60, "60 fps");
                    });
                    ui.end_row();

                    ui.label("Bitrate");
                    ui.add(egui::Slider::new(&mut self.bitrate, 2000..=20000).step_by(500.0).suffix(" kbit/s"));
                    ui.end_row();

                    ui.label("Sound");
                    egui::ComboBox::from_id_salt("audio")
                        .selected_text(match self.audio {
                            AudioMode::Tv => "TV only",
                            AudioMode::Both => "TV and computer",
                            AudioMode::None => "Off",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.audio, AudioMode::Tv, "TV only");
                            ui.selectable_value(&mut self.audio, AudioMode::Both, "TV and computer");
                            ui.selectable_value(&mut self.audio, AudioMode::None, "Off");
                        });
                    ui.end_row();

                    ui.label("Quality");
                    ui.checkbox(&mut self.high, "H.264 High profile (if the TV supports it)");
                    ui.end_row();
                });
            });

            ui.add_space(12.0);
            ui.vertical_centered_justified(|ui| {
                let big = |t: &str| egui::Button::new(egui::RichText::new(t).size(18.0)).min_size(egui::vec2(0.0, 40.0));
                match self.phase {
                    Phase::Idle => {
                        if ui.add_enabled(self.selected.is_some(), big("Connect")).clicked() {
                            self.connect();
                        }
                    }
                    Phase::Stopping => {
                        ui.add_enabled(false, big("Disconnecting …"));
                    }
                    _ => {
                        if ui.add(big("Disconnect")).clicked() {
                            self.disconnect();
                        }
                    }
                }
            });
        });
    }

    fn on_exit(&mut self) {
        self.stop_scan();
        if let Some(c) = &self.cast_cancel {
            c.cancel();
        }
        if let Some(t) = self.cast_task.take() {
            let _ = self.rt.block_on(async { tokio::time::timeout(Duration::from_secs(3), t).await });
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 600.0])
            .with_min_inner_size([400.0, 480.0])
            .with_app_id(APP_ID)
            .with_title("Miracast"),
        ..Default::default()
    };
    eframe::run_native("Miracast", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
