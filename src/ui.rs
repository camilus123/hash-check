//! The egui front end. It only reads the database and starts jobs; all
//! hashing happens on the job thread (see `engine`).

use crate::appdata::AppDir;
use crate::db::{Db, Entry, Status};
use crate::engine::{self, Phase, Shared, State};
use crate::hasher::{Algo, to_hex};
use crate::util::{format_duration, format_unix_nanos, format_unix_secs, group_digits, human_bytes};
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const WINDOW_TITLE: &str = concat!("hash-check ", env!("CARGO_PKG_VERSION"));
const ROW_HEIGHT: f32 = 20.0;
/// How often the list is refreshed while a job is adding entries.
const LIST_REFRESH: Duration = Duration::from_secs(2);
/// Window over which the "current" speed is measured.
const SPEED_WINDOW: Duration = Duration::from_millis(1500);
const CACHE_LIMIT: usize = 20_000;

/// Start the application. Shows a message box and exits if the app
/// directory is unusable.
pub fn run() {
    let app = match App::new() {
        Ok(app) => app,
        Err(msg) => {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title(WINDOW_TITLE)
                .set_description(msg)
                .show();
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([760.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(WINDOW_TITLE, options, Box::new(|_cc| Ok(Box::new(app)))) {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title(WINDOW_TITLE)
            .set_description(format!("The window could not be created:\n\n{e}"))
            .show();
        std::process::exit(1);
    }
}

struct App {
    appdir: AppDir,
    db: Db,
    ids: Vec<i64>,
    cache: HashMap<i64, Entry>,
    total_size: u64,
    algo: Algo,
    recursive: bool,
    job: Option<Arc<Shared>>,
    /// Summary of the last finished job, shown until the next one starts.
    last_job: Option<State>,
    last_list_refresh: Instant,
    speed_samples: VecDeque<(Instant, u64)>,
    show_about: bool,
    show_problems: bool,
    error: Option<String>,
}

impl App {
    fn new() -> Result<App, String> {
        let appdir = AppDir::locate()?;
        appdir.check_writable()?;
        let db = Db::open(&appdir.db_path()).map_err(|e| {
            format!("The database could not be opened:\n\n{}\n\n{e}", appdir.db_path().display())
        })?;
        let mut app = App {
            appdir,
            db,
            ids: Vec::new(),
            cache: HashMap::new(),
            total_size: 0,
            algo: Algo::DEFAULT,
            recursive: true,
            job: None,
            last_job: None,
            last_list_refresh: Instant::now(),
            speed_samples: VecDeque::new(),
            show_about: false,
            show_problems: false,
            error: None,
        };
        if let Some(backup) = &app.db.backup_made {
            app.error = Some(format!(
                "The database was upgraded to a new format. A backup of the old one was saved as:\n{}",
                backup.display()
            ));
        }
        app.refresh_list();
        Ok(app)
    }

    fn refresh_list(&mut self) {
        match self.db.all_ids().and_then(|ids| Ok((ids, self.db.total_size()?))) {
            Ok((ids, total)) => {
                self.ids = ids;
                self.total_size = total;
                self.cache.clear();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.last_list_refresh = Instant::now();
    }

    fn start_add(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>) {
        if inputs.is_empty() || self.job.is_some() {
            return;
        }
        let ctx = ctx.clone();
        self.speed_samples.clear();
        self.last_job = None;
        self.show_problems = false;
        self.job = Some(engine::start_add(
            self.appdir.db_path(),
            inputs,
            self.recursive,
            self.algo,
            Box::new(move || ctx.request_repaint()),
        ));
    }

    /// Poll the running job; returns its state and current speed in bytes/s.
    fn poll_job(&mut self, ctx: &egui::Context) -> Option<(State, u64, f64)> {
        let job = self.job.clone()?;
        let state = job.snapshot();
        let bytes = job.bytes_done.load(Ordering::Relaxed);
        let now = Instant::now();
        self.speed_samples.push_back((now, bytes));
        while self.speed_samples.len() > 2
            && now.duration_since(self.speed_samples[1].0) >= SPEED_WINDOW
        {
            self.speed_samples.pop_front();
        }
        let (t0, b0) = self.speed_samples[0];
        let dt = now.duration_since(t0).as_secs_f64();
        let speed = if dt > 0.2 { bytes.saturating_sub(b0) as f64 / dt } else { 0.0 };

        if state.phase == Phase::Finished {
            self.job = None;
            self.last_job = Some(state.clone());
            self.refresh_list();
        } else {
            if self.last_list_refresh.elapsed() >= LIST_REFRESH {
                self.refresh_list();
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        Some((state, bytes, speed))
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let idle = self.job.is_none();
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(idle, egui::Button::new("Add files…")).clicked() {
                if let Some(files) = rfd::FileDialog::new().set_title("Add files to hash-check").pick_files() {
                    self.start_add(ui.ctx(), files);
                }
            }
            if ui.add_enabled(idle, egui::Button::new("Add folder…")).clicked() {
                if let Some(dirs) = rfd::FileDialog::new().set_title("Add folders to hash-check").pick_folders() {
                    self.start_add(ui.ctx(), dirs);
                }
            }
            ui.add_enabled(idle, egui::Checkbox::new(&mut self.recursive, "Include subfolders"))
                .on_hover_text("When adding a folder, also add everything in the folders below it.");
            ui.separator();
            ui.label("Algorithm for new entries:");
            ui.add_enabled_ui(idle, |ui| {
                egui::ComboBox::from_id_salt("algo")
                    .selected_text(self.algo.name())
                    .show_ui(ui, |ui| {
                        for a in Algo::ALL {
                            ui.selectable_value(&mut self.algo, a, a.description());
                        }
                    });
            });
            ui.separator();
            if ui.button("About").clicked() {
                self.show_about = true;
            }
        });
    }

    fn status_panel(&mut self, ui: &mut egui::Ui, live: Option<(State, u64, f64)>) {
        if let Some((s, bytes, speed)) = live.filter(|(s, _, _)| s.phase != Phase::Finished) {
            ui.horizontal(|ui| {
                match s.phase {
                    Phase::Scanning => ui.strong("Scanning…"),
                    _ => ui.strong("Hashing…"),
                };
                if ui.button("Cancel").on_hover_text("Stop now. Everything hashed so far is kept.").clicked() {
                    if let Some(job) = &self.job {
                        job.request_cancel();
                    }
                }
            });
            if s.phase == Phase::Scanning {
                ui.label(format!("{} files found so far", group_digits(s.files_total)));
            } else {
                let frac = if s.bytes_total > 0 {
                    bytes as f32 / s.bytes_total as f32
                } else if s.files_total > 0 {
                    s.files_done as f32 / s.files_total as f32
                } else {
                    0.0
                };
                ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).show_percentage());
                let elapsed = s.hashing_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                let avg = if elapsed > 0.2 { bytes as f64 / elapsed } else { 0.0 };
                let eta = if avg > 0.0 { s.bytes_total.saturating_sub(bytes) as f64 / avg } else { f64::NAN };
                ui.label(format!(
                    "{} / {} files · {} / {} · now {:.0} MB/s · average {:.0} MB/s · ETA {}",
                    group_digits(s.files_done),
                    group_digits(s.files_total),
                    human_bytes(bytes),
                    human_bytes(s.bytes_total),
                    speed / 1_048_576.0,
                    avg / 1_048_576.0,
                    format_duration(eta),
                ));
            }
            ui.add(egui::Label::new(egui::RichText::new(&s.current).weak()).truncate());
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "{} entries · {} tracked",
                group_digits(self.ids.len() as u64),
                human_bytes(self.total_size)
            ));
            if let Some(s) = &self.last_job {
                ui.separator();
                let took = s.finished_at.map(|t| t.duration_since(s.started).as_secs_f64()).unwrap_or(0.0);
                let mut text = format!(
                    "{}: {} added in {}",
                    if s.cancelled { "Cancelled" } else { "Done" },
                    group_digits(s.added),
                    format_duration(took)
                );
                if s.already_tracked > 0 {
                    text += &format!(" · {} already tracked", group_digits(s.already_tracked));
                }
                if s.skipped_links > 0 {
                    text += &format!(" · {} links not followed", group_digits(s.skipped_links));
                }
                ui.label(text);
                if s.problem_count > 0 {
                    let label = format!("{} problems", group_digits(s.problem_count));
                    if ui.link(egui::RichText::new(label).color(ui.visuals().warn_fg_color)).clicked() {
                        self.show_problems = !self.show_problems;
                    }
                }
                if let Some(f) = &s.fatal {
                    ui.colored_label(ui.visuals().error_fg_color, format!("Stopped: {f}"));
                }
            } else {
                ui.separator();
                ui.weak("Drop files or folders anywhere in this window to add them.");
            }
        });
    }

    fn problems_window(&mut self, ctx: &egui::Context) {
        let Some(s) = &self.last_job else { return };
        if !self.show_problems || s.problems.is_empty() {
            return;
        }
        let mut open = true;
        egui::Window::new("Problems in the last run")
            .open(&mut open)
            .default_size([760.0, 320.0])
            .show(ctx, |ui| {
                if s.problem_count as usize > s.problems.len() {
                    ui.weak(format!("Showing the first {} of {}.", s.problems.len(), s.problem_count));
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for p in &s.problems {
                        ui.horizontal_wrapped(|ui| {
                            ui.monospace(&p.path);
                            ui.colored_label(ui.visuals().warn_fg_color, &p.message);
                        });
                    }
                });
            });
        self.show_problems = open;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        egui::Window::new("About hash-check")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.heading(WINDOW_TITLE);
                ui.label("Detects file changes and bit rot by comparing file hashes with stored ones.");
                ui.add_space(6.0);
                ui.label("hash-check never modifies, renames, moves or deletes your files.");
                ui.label("It only writes inside its own folder:");
                ui.monospace(self.appdir.dir().display().to_string());
                ui.add_space(6.0);
                ui.label(format!("Database schema version {}", crate::db::SCHEMA_VERSION));
                ui.label("Licence: GNU GPL v3");
                ui.hyperlink("https://github.com/camilus123/hash-check");
            });
        self.show_about = open;
    }

    fn error_window(&mut self, ctx: &egui::Context) {
        let Some(msg) = self.error.clone() else { return };
        egui::Window::new("hash-check")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(msg);
                if ui.button("OK").clicked() {
                    self.error = None;
                }
            });
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        if self.ids.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak("Nothing tracked yet. Use “Add files…” or “Add folder…”, or drop files here.");
            });
            return;
        }
        if self.cache.len() > CACHE_LIMIT {
            self.cache.clear();
        }
        let (ids, cache, db) = (&self.ids, &mut self.cache, &self.db);
        let mut db_error = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(90.0).at_least(40.0))
            .column(Column::initial(430.0).at_least(80.0).clip(true))
            .column(Column::initial(80.0).at_least(40.0))
            .column(Column::initial(140.0).at_least(40.0))
            .column(Column::initial(75.0).at_least(40.0))
            .column(Column::initial(150.0).at_least(40.0).clip(true))
            .column(Column::remainder().at_least(40.0).clip(true))
            .header(22.0, |mut h| {
                for title in ["Status", "Path", "Size", "Modified", "Algorithm", "Hash", "Last verified"] {
                    h.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, ids.len(), |mut row| {
                    let id = ids[row.index()];
                    if !cache.contains_key(&id) {
                        match db.entry(id) {
                            Ok(Some(e)) => {
                                cache.insert(id, e);
                            }
                            Ok(None) => {}
                            Err(e) => db_error = Some(e.to_string()),
                        }
                    }
                    let Some(e) = cache.get(&id) else {
                        row.col(|ui| {
                            ui.weak("…");
                        });
                        return;
                    };
                    row.col(|ui| status_label(ui, e.status));
                    row.col(|ui| {
                        cell(ui, egui::RichText::new(&e.path), &e.path);
                    });
                    row.col(|ui| {
                        let exact = format!("{} bytes", group_digits(e.size));
                        cell(ui, egui::RichText::new(human_bytes(e.size)), &exact);
                    });
                    row.col(|ui| {
                        ui.label(format_unix_nanos(e.mtime_ns));
                    });
                    row.col(|ui| {
                        ui.label(e.algo.name());
                    });
                    row.col(|ui| {
                        let hex = to_hex(&e.hash);
                        cell(ui, egui::RichText::new(&hex[..hex.len().min(16)]).monospace(), &hex);
                    });
                    row.col(|ui| {
                        ui.label(format_unix_secs(e.last_verified));
                    });
                });
            });
        if let Some(e) = db_error {
            self.error = Some(e);
        }
    }
}

/// A table cell with exactly one tooltip. egui adds its own "full text"
/// tooltip to a label that had to be shortened; with ours on top that made
/// two identical popups, so the automatic one is switched off here.
fn cell(ui: &mut egui::Ui, text: egui::RichText, hover: &str) {
    ui.add(egui::Label::new(text).show_tooltip_when_elided(false)).on_hover_text(hover);
}

fn status_label(ui: &mut egui::Ui, status: Status) {
    let color = match status {
        Status::Ok => egui::Color32::from_rgb(70, 160, 90),
        Status::Changed => egui::Color32::from_rgb(200, 150, 40),
        Status::PossiblyCorrupted => egui::Color32::from_rgb(230, 60, 60),
        Status::Missing | Status::Unreadable => egui::Color32::from_rgb(210, 110, 50),
    };
    ui.colored_label(color, status.label());
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Files dropped onto the window.
        let dropped: Vec<PathBuf> =
            ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if !dropped.is_empty() {
            self.start_add(&ctx, dropped);
        }

        let live = self.poll_job(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            self.status_panel(ui, live);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.table(ui);
        });

        self.problems_window(&ctx);
        self.about_window(&ctx);
        self.error_window(&ctx);
    }
}
