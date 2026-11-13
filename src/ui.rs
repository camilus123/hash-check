//! The egui front end. It only reads the database and starts jobs; hashing
//! and all database changes happen on the job thread (see `engine`).

use crate::appdata::AppDir;
use crate::db::{Db, Entry, Filter, NewFile, Root, Scope, Status, StatusCounts};
use crate::engine::{
    self, ExportDest, Job, JobKind, Phase, Shared, State, VerifyCounts, VerifyTarget,
};
use crate::hasher::{Algo, to_hex};
use crate::report::Format;
use crate::util::{
    format_duration, format_unix_nanos, format_unix_secs, group_digits, human_bytes,
};
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const WINDOW_TITLE: &str = concat!("hash-check ", env!("CARGO_PKG_VERSION"));
const ROW_HEIGHT: f32 = 20.0;
/// How often the list is refreshed while a job is changing the database.
const LIST_REFRESH: Duration = Duration::from_secs(2);
/// Window over which the "current" speed is measured.
const SPEED_WINDOW: Duration = Duration::from_millis(1500);
const CACHE_LIMIT: usize = 20_000;

const RED: egui::Color32 = egui::Color32::from_rgb(230, 60, 60);

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
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([820.0, 440.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    let result = eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|cc| {
            // Rows are selected by clicking, so label text must not swallow
            // clicks; paths and hashes are copied from the right-click menu.
            cc.egui_ctx
                .global_style_mut(|s| s.interaction.selectable_labels = false);
            crate::fonts::install(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    if let Err(e) = result {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title(WINDOW_TITLE)
            .set_description(format!("The window could not be created:\n\n{e}"))
            .show();
        std::process::exit(1);
    }
}

const ORANGE: egui::Color32 = egui::Color32::from_rgb(210, 110, 50);
const AMBER: egui::Color32 = egui::Color32::from_rgb(200, 150, 40);
const GREEN: egui::Color32 = egui::Color32::from_rgb(70, 160, 90);
const BLUE: egui::Color32 = egui::Color32::from_rgb(70, 130, 220);

fn status_color(status: Status) -> egui::Color32 {
    match status {
        Status::Ok => GREEN,
        Status::Changed => AMBER,
        Status::PossiblyCorrupted => RED,
        Status::Missing | Status::Unreadable => ORANGE,
    }
}

/// Which results the list shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Show {
    All,
    Status(Status),
    /// Files found under a root that are not in the database.
    New,
}

/// Summary of the last verification, kept on screen until dismissed.
struct VerifySummary {
    counts: VerifyCounts,
    unavailable: Vec<(String, u64)>,
    cancelled: bool,
    changed_during: u64,
}

/// The "Export report" dialog.
struct ExportDialog {
    format: Format,
}

/// A question the user must answer before a job starts.
enum Confirm {
    RemoveEntries {
        ids: Vec<i64>,
    },
    RemoveRoot {
        root: Root,
        entries: u64,
    },
    Update {
        ids: Vec<i64>,
        possibly_corrupted: u64,
        acknowledged: bool,
    },
}

struct App {
    appdir: AppDir,
    db: Db,

    // What the list shows.
    scope: Scope,
    search: String,
    ids: Vec<i64>,
    cache: HashMap<i64, Entry>,
    selected: HashSet<i64>,
    /// Row index a Shift+click extends from.
    anchor: Option<usize>,
    show: Show,
    /// Limit the list to entries the last verification looked at.
    last_run_only: bool,
    /// Rows of the "New" view (`ids` then holds new-file ids).
    new_cache: HashMap<i64, NewFile>,
    /// Entries per status for the chips (within scope and "last run only").
    counts: StatusCounts,
    new_count: u64,
    /// Entries flagged possibly corrupted in the whole database.
    corrupted_total: u64,
    corrupted_banner_dismissed: bool,
    last_run: i64,
    verify_summary: Option<VerifySummary>,
    export: Option<ExportDialog>,
    info: Option<String>,

    // Overview for the left panel.
    roots: Vec<Root>,
    root_counts: HashMap<i64, u64>,
    loose_count: u64,
    total_count: u64,
    total_size: u64,

    algo: Algo,
    recursive: bool,

    job: Option<Arc<Shared>>,
    /// Summary of the last finished job, shown until the next one starts.
    last_job: Option<State>,
    last_list_refresh: Instant,
    speed_samples: VecDeque<(Instant, u64)>,

    confirm: Option<Confirm>,
    show_about: bool,
    show_problems: bool,
    error: Option<String>,
}

impl App {
    fn new() -> Result<App, String> {
        let appdir = AppDir::locate()?;
        appdir.check_writable()?;
        let db = Db::open(&appdir.db_path()).map_err(|e| {
            format!(
                "The database could not be opened:\n\n{}\n\n{e}",
                appdir.db_path().display()
            )
        })?;
        let mut app = App {
            appdir,
            db,
            scope: Scope::All,
            search: String::new(),
            ids: Vec::new(),
            cache: HashMap::new(),
            selected: HashSet::new(),
            anchor: None,
            show: Show::All,
            last_run_only: false,
            new_cache: HashMap::new(),
            counts: StatusCounts::default(),
            new_count: 0,
            corrupted_total: 0,
            corrupted_banner_dismissed: false,
            last_run: 0,
            verify_summary: None,
            export: None,
            info: None,
            roots: Vec::new(),
            root_counts: HashMap::new(),
            loose_count: 0,
            total_count: 0,
            total_size: 0,
            algo: Algo::DEFAULT,
            recursive: true,
            job: None,
            last_job: None,
            last_list_refresh: Instant::now(),
            speed_samples: VecDeque::new(),
            confirm: None,
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
        app.refresh();
        Ok(app)
    }

    /// Reload everything shown from the database.
    fn refresh(&mut self) {
        let loaded = (|| {
            let roots = self.db.roots()?;
            // A root removed elsewhere must not leave the view pointing at nothing.
            if let Scope::Root(id) = self.scope
                && !roots.iter().any(|r| r.id == id)
            {
                self.scope = Scope::All;
            }
            let (root_counts, loose) = self.db.root_counts()?;
            let last_run = self.db.last_run()?;
            let run = (self.last_run_only && last_run > 0).then_some(last_run);
            let search = self.search.trim();
            let ids = match self.show {
                Show::New => self.db.new_file_ids(self.scope, search)?,
                Show::All => self.db.ids_matching(&Filter {
                    scope: Some(self.scope),
                    search,
                    status: None,
                    run,
                })?,
                Show::Status(status) => self.db.ids_matching(&Filter {
                    scope: Some(self.scope),
                    search,
                    status: Some(status),
                    run,
                })?,
            };
            Ok::<_, crate::db::DbError>((
                roots,
                root_counts,
                loose,
                ids,
                self.db.count_files()?,
                self.db.total_size()?,
                self.db.status_counts(Some(self.scope), run)?,
                self.db.count_new_files(self.scope)?,
                self.db.status_counts(None, None)?.possibly_corrupted,
                last_run,
            ))
        })();
        match loaded {
            Ok((
                roots,
                root_counts,
                loose,
                ids,
                count,
                size,
                counts,
                new_count,
                corrupted,
                last_run,
            )) => {
                self.roots = roots;
                self.root_counts = root_counts;
                self.loose_count = loose;
                self.ids = ids;
                self.total_count = count;
                self.total_size = size;
                self.counts = counts;
                self.new_count = new_count;
                self.corrupted_total = corrupted;
                self.last_run = last_run;
                self.cache.clear();
                self.new_cache.clear();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.last_list_refresh = Instant::now();
    }

    fn set_show(&mut self, show: Show) {
        if self.show != show {
            // Entry ids and new-file ids are different things: never carry a selection across.
            if (self.show == Show::New) != (show == Show::New) {
                self.selected.clear();
            }
            self.show = show;
            self.anchor = None;
            self.refresh();
        }
    }

    fn set_scope(&mut self, scope: Scope) {
        if self.scope != scope {
            self.scope = scope;
            self.selected.clear();
            self.anchor = None;
            self.refresh();
        }
    }

    fn start_job(&mut self, ctx: &egui::Context, job: Job) {
        if self.job.is_some() {
            return;
        }
        let ctx = ctx.clone();
        self.speed_samples.clear();
        self.last_job = None;
        self.show_problems = false;
        self.job = Some(engine::start(
            self.appdir.db_path(),
            job,
            Box::new(move || ctx.request_repaint()),
        ));
    }

    fn start_add(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>) {
        if !inputs.is_empty() {
            self.start_job(
                ctx,
                Job::Add {
                    inputs,
                    recursive: self.recursive,
                    algo: self.algo,
                },
            );
        }
    }

    /// The selection in list order.
    fn selected_ids(&self) -> Vec<i64> {
        self.ids
            .iter()
            .copied()
            .filter(|id| self.selected.contains(id))
            .collect()
    }

    /// Poll the running job; returns its state, bytes done and current speed in bytes/s.
    fn poll_job(&mut self, ctx: &egui::Context) -> Option<(State, u64, f64)> {
        let job = self.job.clone()?;
        let state = job.snapshot();
        let bytes = job.bytes_done.load(Ordering::Relaxed);
        let now = Instant::now();
        if state.paused {
            self.speed_samples.clear();
        }
        self.speed_samples.push_back((now, bytes));
        while self.speed_samples.len() > 2
            && now.duration_since(self.speed_samples[1].0) >= SPEED_WINDOW
        {
            self.speed_samples.pop_front();
        }
        let (t0, b0) = self.speed_samples[0];
        let dt = now.duration_since(t0).as_secs_f64();
        let speed = if dt > 0.2 {
            bytes.saturating_sub(b0) as f64 / dt
        } else {
            0.0
        };

        if state.phase == Phase::Finished {
            self.job = None;
            if state.kind == JobKind::Remove || self.show == Show::New {
                self.selected.clear();
                self.anchor = None;
            }
            if state.kind == JobKind::Verify {
                self.verify_summary = Some(VerifySummary {
                    counts: state.verify.clone(),
                    unavailable: state.unavailable_volumes.clone(),
                    cancelled: state.cancelled,
                    changed_during: state.changed_during_hash,
                });
                self.corrupted_banner_dismissed = false;
            }
            if state.kind == JobKind::Export
                && state.fatal.is_none()
                && let Some(path) = &state.report_path
            {
                self.info = Some(format!("Report saved:\n{}", path.display()));
            }
            self.last_job = Some(state.clone());
            self.refresh();
        } else {
            if self.last_list_refresh.elapsed() >= LIST_REFRESH && !state.paused {
                self.refresh();
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        Some((state, bytes, speed))
    }

    // ---------------------------------------------------------------- toolbar

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let idle = self.job.is_none();
        let n_sel = self.selected.len();
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(idle, egui::Button::new("Add files…")).clicked()
                && let Some(files) = rfd::FileDialog::new().set_title("Add files to hash-check").pick_files()
            {
                self.start_add(ui.ctx(), files);
            }
            if ui.add_enabled(idle, egui::Button::new("Add folder…")).clicked()
                && let Some(dirs) = rfd::FileDialog::new().set_title("Add folders to hash-check").pick_folders()
            {
                self.start_add(ui.ctx(), dirs);
            }
            ui.add_enabled(idle, egui::Checkbox::new(&mut self.recursive, "Include subfolders"))
                .on_hover_text("When adding a folder, also add everything in the folders below it.");
            ui.label("Algorithm:");
            ui.add_enabled_ui(idle, |ui| {
                egui::ComboBox::from_id_salt("algo")
                    .selected_text(self.algo.name())
                    .show_ui(ui, |ui| {
                        for a in Algo::ALL {
                            ui.selectable_value(&mut self.algo, a, a.description());
                        }
                    })
                    .response
                    .on_hover_text("Used for newly added files. Existing entries keep their own algorithm.");
            });
            ui.separator();

            ui.menu_button("Verify ▾", |ui| {
                ui.set_min_width(260.0);
                if ui
                    .add_enabled(idle && self.total_count > 0, egui::Button::new("Verify everything"))
                    .on_hover_text("Re-hash every tracked file and look for new files in every root.")
                    .clicked()
                {
                    self.start_job(ui.ctx(), Job::Verify { target: VerifyTarget::All });
                    ui.close();
                }
                let root = match self.scope {
                    Scope::Root(id) => self.roots.iter().find(|r| r.id == id).cloned(),
                    _ => None,
                };
                let root_label = match &root {
                    Some(r) => format!("Verify root {}", r.path),
                    None => "Verify the selected root".to_string(),
                };
                if ui
                    .add_enabled(idle && root.is_some(), egui::Button::new(root_label).truncate())
                    .on_disabled_hover_text("Click a root in the left panel first.")
                    .clicked()
                    && let Some(r) = root
                {
                    self.start_job(ui.ctx(), Job::Verify { target: VerifyTarget::Root(r.id) });
                    ui.close();
                }
                let can_sel = idle && n_sel > 0 && self.show != Show::New;
                if ui
                    .add_enabled(can_sel, egui::Button::new(if can_sel {
                        format!("Verify {} selected", group_digits(n_sel as u64))
                    } else {
                        "Verify selected entries".to_string()
                    }))
                    .on_disabled_hover_text("Select one or more entries first.")
                    .clicked()
                {
                    let ids = self.selected_ids();
                    self.start_job(ui.ctx(), Job::Verify { target: VerifyTarget::Entries(ids) });
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(idle && self.last_run > 0, egui::Button::new("Export last report…"))
                    .on_disabled_hover_text("Run a verification first.")
                    .clicked()
                {
                    self.export = Some(ExportDialog { format: Format::Csv });
                    ui.close();
                }
            });
            ui.separator();

            let can_act = idle && n_sel > 0 && self.show != Show::New;
            if ui
                .add_enabled(can_act, egui::Button::new("Update…"))
                .on_hover_text("Re-hash the selected entries and replace their stored hashes. Use this for files you know you changed.")
                .on_disabled_hover_text("Select one or more entries first.")
                .clicked()
            {
                let ids = self.selected_ids();
                match self.db.count_with_status(&ids, Status::PossiblyCorrupted) {
                    Ok(bad) => self.confirm = Some(Confirm::Update { ids, possibly_corrupted: bad, acknowledged: false }),
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            if ui
                .add_enabled(can_act, egui::Button::new("Remove…"))
                .on_hover_text("Remove the selected entries from the database. Files on disk are not touched.")
                .on_disabled_hover_text("Select one or more entries first.")
                .clicked()
            {
                self.confirm = Some(Confirm::RemoveEntries { ids: self.selected_ids() });
            }
            ui.separator();
            if ui.button("About").clicked() {
                self.show_about = true;
            }
        });
    }

    // ------------------------------------------------------------- left panel

    fn roots_panel(&mut self, ui: &mut egui::Ui) {
        let idle = self.job.is_none();
        ui.add_space(4.0);
        ui.strong("Tracked");
        ui.add_space(2.0);
        let mut new_scope = None;
        let mut remove_root = None;
        egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            let all = format!("All entries ({})", group_digits(self.total_count));
            if ui.selectable_label(self.scope == Scope::All, all).clicked() {
                new_scope = Some(Scope::All);
            }
            for root in &self.roots {
                let n = self.root_counts.get(&root.id).copied().unwrap_or(0);
                let text = format!("{} ({})", root.path, group_digits(n));
                let hover = format!(
                    "{}\n{} entries · {} · added {}",
                    root.path,
                    group_digits(n),
                    if root.recursive { "with subfolders" } else { "this folder only" },
                    format_unix_secs(root.added_at)
                );
                let resp = ui
                    .add(
                        egui::Button::selectable(self.scope == Scope::Root(root.id), text)
                            .truncate(),
                    )
                    .on_hover_text(hover);
                if resp.clicked() {
                    new_scope = Some(Scope::Root(root.id));
                }
                resp.context_menu(|ui| {
                    if ui.add_enabled(idle, egui::Button::new("Remove this root…")).clicked() {
                        remove_root = Some((root.clone(), n));
                        ui.close();
                    }
                    if ui.button("Copy path").clicked() {
                        ui.ctx().copy_text(root.path.clone());
                        ui.close();
                    }
                });
            }
            if self.loose_count > 0 || self.scope == Scope::Loose {
                let loose = format!("Single files ({})", group_digits(self.loose_count));
                if ui
                    .selectable_label(self.scope == Scope::Loose, loose)
                    .on_hover_text("Files added one by one, not as part of a folder.")
                    .clicked()
                {
                    new_scope = Some(Scope::Loose);
                }
            }
            if let Scope::Root(id) = self.scope
                && let Some(root) = self.roots.iter().find(|r| r.id == id)
            {
                ui.add_space(8.0);
                if ui
                    .add_enabled(idle, egui::Button::new("Remove this root…"))
                    .on_hover_text("Forget this folder and all its entries. Files on disk are not touched.")
                    .clicked()
                {
                    remove_root = Some((root.clone(), self.root_counts.get(&id).copied().unwrap_or(0)));
                }
            }
        });
        if let Some(scope) = new_scope {
            self.set_scope(scope);
        }
        if let Some((root, entries)) = remove_root {
            self.confirm = Some(Confirm::RemoveRoot { root, entries });
        }
    }

    // ------------------------------------------------------------ bottom panel

    fn status_panel(&mut self, ui: &mut egui::Ui, live: Option<(State, u64, f64)>) {
        if let Some((s, bytes, speed)) = live.filter(|(s, _, _)| s.phase != Phase::Finished) {
            ui.horizontal(|ui| {
                let what = match (s.kind, s.phase) {
                    (_, Phase::Removing) => "Removing entries from the database…",
                    (_, Phase::Writing) | (JobKind::Export, _) => "Writing report…",
                    (JobKind::Verify, Phase::Preparing) => "Verifying: looking for new files…",
                    (JobKind::Verify, _) => "Verifying…",
                    (JobKind::Add, Phase::Preparing) => "Scanning for files…",
                    (JobKind::Add, _) => "Adding: hashing new files…",
                    (JobKind::Update, Phase::Preparing) => "Preparing update…",
                    (JobKind::Update, _) => "Updating: re-hashing…",
                    (JobKind::Remove, _) => "Removing…",
                };
                ui.strong(if s.paused { "Paused" } else { what });
                if let Some(job) = &self.job {
                    if !matches!(s.phase, Phase::Removing | Phase::Writing) {
                        let label = if s.paused { "Resume" } else { "Pause" };
                        if ui.button(label).clicked() {
                            job.set_paused(!s.paused);
                        }
                    }
                    if ui
                        .button("Cancel")
                        .on_hover_text("Stop now. Everything finished so far is kept.")
                        .clicked()
                    {
                        job.request_cancel();
                    }
                }
            });
            match s.phase {
                Phase::Preparing if s.kind == JobKind::Verify => {
                    ui.label(format!(
                        "{} new files found so far",
                        group_digits(s.verify.new)
                    ));
                }
                Phase::Preparing => {
                    ui.label(format!(
                        "{} files found so far",
                        group_digits(s.files_total)
                    ));
                }
                Phase::Writing => {
                    ui.spinner();
                }
                Phase::Removing => {
                    if s.files_total > 0 {
                        ui.add(
                            egui::ProgressBar::new(s.files_done as f32 / s.files_total as f32)
                                .show_percentage(),
                        );
                    } else {
                        ui.spinner();
                    }
                }
                _ => {
                    let frac = if s.bytes_total > 0 {
                        bytes as f32 / s.bytes_total as f32
                    } else if s.files_total > 0 {
                        s.files_done as f32 / s.files_total as f32
                    } else {
                        0.0
                    };
                    ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).show_percentage());
                    let active = s.active_hashing_secs();
                    let avg = if active > 0.2 {
                        bytes as f64 / active
                    } else {
                        0.0
                    };
                    let eta = if avg > 0.0 {
                        s.bytes_total.saturating_sub(bytes) as f64 / avg
                    } else {
                        f64::NAN
                    };
                    ui.label(format!(
                        "{} / {} files · {} / {} · now {:.0} MB/s · average {:.0} MB/s · ETA {}",
                        group_digits(s.files_done),
                        group_digits(s.files_total),
                        human_bytes(bytes),
                        human_bytes(s.bytes_total),
                        if s.paused { 0.0 } else { speed / 1_048_576.0 },
                        avg / 1_048_576.0,
                        if s.paused {
                            "paused".to_string()
                        } else {
                            format_duration(eta)
                        },
                    ));
                    if s.kind == JobKind::Verify {
                        ui.horizontal_wrapped(|ui| {
                            let c = &s.verify;
                            for (n, name, color) in [
                                (c.ok, "OK", GREEN),
                                (c.changed, "changed", AMBER),
                                (c.possibly_corrupted, "POSSIBLY CORRUPTED", RED),
                                (c.missing, "missing", ORANGE),
                                (c.unreadable, "unreadable", ORANGE),
                                (c.new, "new", BLUE),
                                (c.unavailable, "on unavailable drives", ORANGE),
                            ] {
                                let text = format!("{} {name}", group_digits(n));
                                if n > 0 {
                                    ui.colored_label(color, egui::RichText::new(text).strong());
                                } else {
                                    ui.weak(text);
                                }
                            }
                        });
                    }
                }
            }
            ui.add(
                egui::Label::new(egui::RichText::new(&s.current).weak())
                    .truncate()
                    .show_tooltip_when_elided(false),
            );
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "{} entries · {} tracked",
                group_digits(self.total_count),
                human_bytes(self.total_size)
            ));
            if let Some(s) = &self.last_job {
                ui.separator();
                ui.label(job_summary(s));
                if s.problem_count > 0 {
                    let label = format!("{} problems", group_digits(s.problem_count));
                    if ui
                        .link(egui::RichText::new(label).color(ui.visuals().warn_fg_color))
                        .clicked()
                    {
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

    // ------------------------------------------------------------------ table

    /// Result filter: one chip per classification, with counts.
    fn chips(&mut self, ui: &mut egui::Ui) {
        let mut want = None;
        let mut toggle_run = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Show:");
            let mut chip = |ui: &mut egui::Ui, show: Show, text: String, color: Option<egui::Color32>, hover: &str| {
                let mut rich = egui::RichText::new(text);
                if let Some(c) = color {
                    rich = rich.color(c);
                }
                if ui.selectable_label(self.show == show, rich).on_hover_text(hover).clicked() {
                    want = Some(show);
                }
            };
            chip(ui, Show::All, format!("All ({})", group_digits(self.counts.total())), None, "Every entry.");
            for (status, hover) in [
                (Status::Ok, "Hash matches the stored one."),
                (Status::Changed, "Content differs, and so does the size and/or modified time. Probably a normal edit; use Update if you made it."),
                (Status::PossiblyCorrupted, "Content differs although size and modified time are unchanged. This is what bit rot looks like."),
                (Status::Missing, "The file no longer exists."),
                (Status::Unreadable, "The file could not be read. A read error on a file that used to be fine is a warning sign."),
            ] {
                let n = self.counts.get(status);
                let color = (n > 0).then(|| status_color(status));
                let mut text = format!("{} ({})", chip_name(status), group_digits(n));
                if status == Status::PossiblyCorrupted && n > 0 {
                    text = format!("⚠ {text}");
                }
                chip(ui, Show::Status(status), text, color, hover);
            }
            chip(
                ui,
                Show::New,
                format!("New ({})", group_digits(self.new_count)),
                (self.new_count > 0).then_some(BLUE),
                "Files inside a tracked folder that are not in the database yet. Found by Verify.",
            );
            if self.last_run > 0 && self.show != Show::New {
                ui.separator();
                if ui
                    .checkbox(&mut self.last_run_only, "Last verification only")
                    .on_hover_text("Show only entries the most recent Verify looked at.")
                    .changed()
                {
                    toggle_run = true;
                }
            }
        });
        if let Some(show) = want {
            self.set_show(show);
        } else if toggle_run {
            self.anchor = None;
            self.refresh();
        }
    }

    /// Loud, persistent warnings and the summary of the last verification.
    fn banners(&mut self, ui: &mut egui::Ui) {
        if self.corrupted_total > 0 && !self.corrupted_banner_dismissed {
            let mut show_them = false;
            let mut dismiss = false;
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(150, 20, 20))
                .inner_margin(8.0)
                .corner_radius(4.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "⚠  {} POSSIBLY CORRUPTED",
                                if self.corrupted_total == 1 {
                                    "1 FILE IS".to_string()
                                } else {
                                    format!("{} FILES ARE", group_digits(self.corrupted_total))
                                }
                            ))
                            .color(egui::Color32::WHITE)
                            .strong()
                            .size(16.0),
                        );
                        if ui.button("Show them").clicked() {
                            show_them = true;
                        }
                        if ui.button("Dismiss").on_hover_text("Hide this banner. The files stay flagged.").clicked() {
                            dismiss = true;
                        }
                    });
                    ui.label(
                        egui::RichText::new(
                            "Their content changed although size and modified time did not. Check the drive's health \
                             and restore these files from a backup. Do not use Update on them unless you are sure the \
                             current content is right.",
                        )
                        .color(egui::Color32::WHITE),
                    );
                });
            ui.add_space(4.0);
            if show_them {
                self.scope = Scope::All;
                self.search.clear();
                self.last_run_only = false;
                self.show = Show::All; // force set_show to refresh
                self.set_show(Show::Status(Status::PossiblyCorrupted));
            }
            if dismiss {
                self.corrupted_banner_dismissed = true;
            }
        }

        let mut close = false;
        if let Some(v) = &self.verify_summary {
            egui::Frame::new()
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .inner_margin(8.0)
                .corner_radius(4.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(if v.cancelled {
                            "Verification cancelled. Checked so far:"
                        } else {
                            "Verification finished:"
                        });
                        let c = &v.counts;
                        for (n, name, color) in [
                            (c.ok, "OK", GREEN),
                            (c.changed, "changed", AMBER),
                            (c.possibly_corrupted, "possibly corrupted", RED),
                            (c.missing, "missing", ORANGE),
                            (c.unreadable, "unreadable", ORANGE),
                            (c.new, "new", BLUE),
                        ] {
                            let text = format!("{} {name}", group_digits(n));
                            if n > 0 {
                                ui.colored_label(color, egui::RichText::new(text).strong());
                            } else {
                                ui.weak(text);
                            }
                        }
                        if ui.button("Export report…").clicked() {
                            self.export = Some(ExportDialog { format: Format::Csv });
                        }
                        if ui.button("✖").on_hover_text("Close this summary").clicked() {
                            close = true;
                        }
                    });
                    for (volume, n) in &v.unavailable {
                        ui.colored_label(
                            ORANGE,
                            format!(
                                "Drive {volume} is not available: {} not checked (they are not marked missing).",
                                entries_word(*n)
                            ),
                        );
                    }
                    if v.changed_during > 0 {
                        ui.colored_label(
                            AMBER,
                            format!(
                                "{} changed while being read and could not be judged; see the problems list.",
                                entries_word(v.changed_during)
                            ),
                        );
                    }
                });
            ui.add_space(4.0);
        }
        if close {
            self.verify_summary = None;
        }
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Search:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("part of a path or file name")
                    .desired_width(260.0),
            );
            let mut changed = resp.changed();
            if !self.search.is_empty() && ui.button("✖").on_hover_text("Clear search").clicked() {
                self.search.clear();
                changed = true;
            }
            if changed {
                self.anchor = None;
                self.refresh();
            }
            ui.separator();
            let mut text = format!("{} shown", group_digits(self.ids.len() as u64));
            if !self.selected.is_empty() {
                text += &format!(" · {} selected", group_digits(self.selected.len() as u64));
            }
            ui.label(text);
            if ui
                .add_enabled(!self.ids.is_empty(), egui::Button::new("Select all"))
                .on_hover_text("Select every entry shown (Ctrl+A)")
                .clicked()
            {
                self.selected = self.ids.iter().copied().collect();
            }
            if ui
                .add_enabled(!self.selected.is_empty(), egui::Button::new("Select none"))
                .clicked()
            {
                self.selected.clear();
                self.anchor = None;
            }
            if self.show == Show::New {
                ui.separator();
                let idle = self.job.is_none();
                let (label, ids) = if self.selected.is_empty() {
                    (
                        format!(
                            "Add all {} to database",
                            group_digits(self.ids.len() as u64)
                        ),
                        self.ids.clone(),
                    )
                } else {
                    (
                        format!(
                            "Add {} selected to database",
                            group_digits(self.selected.len() as u64)
                        ),
                        self.selected_ids(),
                    )
                };
                if ui
                    .add_enabled(idle && !ids.is_empty(), egui::Button::new(label))
                    .on_hover_text(format!(
                        "Hash these files with {} and start tracking them.",
                        self.algo.name()
                    ))
                    .clicked()
                {
                    let job = Job::AddNew {
                        new_file_ids: ids,
                        algo: self.algo,
                    };
                    self.start_job(ui.ctx(), job);
                }
            }
        });
    }

    /// The "New" view: untracked files found under roots.
    fn new_table(&mut self, ui: &mut egui::Ui) {
        if self.ids.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak(
                    "No untracked files are known here. Verify looks for them in tracked folders.",
                );
            });
            return;
        }
        if self.new_cache.len() > CACHE_LIMIT {
            self.new_cache.clear();
        }
        let (ids, cache, db, selected) = (&self.ids, &mut self.new_cache, &self.db, &self.selected);
        let mut db_error = None;
        let mut clicked: Option<(usize, egui::Modifiers)> = None;
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::initial(60.0).at_least(40.0).clip(true))
                    .column(Column::initial(320.0).at_least(60.0).clip(true))
                    .column(Column::initial(360.0).at_least(60.0).clip(true))
                    .column(Column::initial(90.0).at_least(40.0).clip(true))
                    .column(Column::initial(150.0).at_least(40.0).clip(true))
                    .column(Column::remainder().at_least(150.0).clip(true))
                    .header(22.0, |mut h| {
                        for title in ["Status", "Name", "Folder", "Size", "Modified", "Found"] {
                            h.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(ROW_HEIGHT, ids.len(), |mut row| {
                            let index = row.index();
                            let id = ids[index];
                            if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(id)
                            {
                                match db.new_file(id) {
                                    Ok(Some(n)) => {
                                        slot.insert(n);
                                    }
                                    Ok(None) => {}
                                    Err(e) => db_error = Some(e.to_string()),
                                }
                            }
                            let Some(n) = cache.get(&id) else {
                                row.col(|ui| {
                                    ui.weak("…");
                                });
                                return;
                            };
                            row.set_selected(selected.contains(&id));
                            let (folder, name) = split_path(&n.path);
                            row.col(|ui| {
                                ui.colored_label(BLUE, "New");
                            });
                            row.col(|ui| cell(ui, egui::RichText::new(name), &n.path));
                            row.col(|ui| cell(ui, egui::RichText::new(folder), &n.path));
                            row.col(|ui| {
                                let exact = format!("{} bytes", group_digits(n.size));
                                cell(ui, egui::RichText::new(human_bytes(n.size)), &exact);
                            });
                            row.col(|ui| {
                                ui.label(format_unix_nanos(n.mtime_ns));
                            });
                            row.col(|ui| {
                                ui.label(format_unix_secs(n.found_at));
                            });
                            let resp = row.response();
                            if resp.clicked() {
                                clicked = Some((index, resp.ctx.input(|i| i.modifiers)));
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Copy full path").clicked() {
                                    ui.ctx().copy_text(n.path.clone());
                                    ui.close();
                                }
                            });
                        });
                    });
            });
        if let Some(e) = db_error {
            self.error = Some(e);
        }
        if let Some((index, mods)) = clicked {
            self.click_row(index, mods);
        }
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        if self.show == Show::New {
            self.new_table(ui);
            return;
        }
        if self.ids.is_empty() {
            ui.centered_and_justified(|ui| {
                if self.total_count == 0 {
                    ui.weak("Nothing tracked yet. Use “Add files…” or “Add folder…”, or drop files here.");
                } else {
                    ui.weak("No entries match.");
                }
            });
            return;
        }
        if self.cache.len() > CACHE_LIMIT {
            self.cache.clear();
        }
        let (ids, cache, db, selected) = (&self.ids, &mut self.cache, &self.db, &self.selected);
        let mut db_error = None;
        let mut clicked: Option<(usize, egui::Modifiers)> = None;
        let mut context_select: Option<usize> = None;
        // The table scrolls vertically by itself; this adds sideways scrolling
        // (scroll bar, Shift+wheel, touchpad) when the columns are wider than the window.
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::initial(155.0).at_least(40.0).clip(true))
                    .column(Column::initial(280.0).at_least(60.0).clip(true))
                    .column(Column::initial(280.0).at_least(60.0).clip(true))
                    .column(Column::initial(80.0).at_least(40.0).clip(true))
                    .column(Column::initial(140.0).at_least(40.0).clip(true))
                    .column(Column::initial(75.0).at_least(40.0).clip(true))
                    .column(Column::initial(140.0).at_least(40.0).clip(true))
                    .column(Column::initial(150.0).at_least(40.0).clip(true))
                    .column(Column::remainder().at_least(150.0).clip(true))
                    .header(22.0, |mut h| {
                        for title in [
                            "Status",
                            "Name",
                            "Folder",
                            "Size",
                            "Modified",
                            "Algorithm",
                            "Hash",
                            "Last verified",
                            "Details",
                        ] {
                            h.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(ROW_HEIGHT, ids.len(), |mut row| {
                            let index = row.index();
                            let id = ids[index];
                            if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(id)
                            {
                                match db.entry(id) {
                                    Ok(Some(e)) => {
                                        slot.insert(e);
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
                            row.set_selected(selected.contains(&id));
                            let (folder, name) = split_path(&e.path);
                            let hex = to_hex(&e.hash);
                            row.col(|ui| status_label(ui, e));
                            row.col(|ui| cell(ui, egui::RichText::new(name), &e.path));
                            row.col(|ui| cell(ui, egui::RichText::new(folder), &e.path));
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
                                cell(
                                    ui,
                                    egui::RichText::new(&hex[..hex.len().min(16)]).monospace(),
                                    &hex,
                                )
                            });
                            row.col(|ui| {
                                let first =
                                    format!("First hashed {}", format_unix_secs(e.first_hashed));
                                cell(
                                    ui,
                                    egui::RichText::new(format_unix_secs(e.last_verified)),
                                    &first,
                                );
                            });
                            row.col(|ui| {
                                if let Some(detail) = &e.last_error {
                                    cell(ui, egui::RichText::new(detail), detail);
                                }
                            });
                            let resp = row.response();
                            if resp.clicked() {
                                clicked = Some((index, resp.ctx.input(|i| i.modifiers)));
                            }
                            if resp.secondary_clicked() && !selected.contains(&id) {
                                context_select = Some(index);
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Copy full path").clicked() {
                                    ui.ctx().copy_text(e.path.clone());
                                    ui.close();
                                }
                                if ui.button("Copy hash").clicked() {
                                    ui.ctx().copy_text(hex.clone());
                                    ui.close();
                                }
                            });
                        });
                    });
            });
        if let Some(e) = db_error {
            self.error = Some(e);
        }
        if let Some(index) = context_select {
            self.selected.clear();
            self.selected.insert(self.ids[index]);
            self.anchor = Some(index);
        }
        if let Some((index, mods)) = clicked {
            self.click_row(index, mods);
        }
    }

    /// Explorer-style selection: click, Ctrl+click to toggle, Shift+click for a range.
    fn click_row(&mut self, index: usize, mods: egui::Modifiers) {
        let id = self.ids[index];
        if mods.shift
            && let Some(anchor) = self.anchor.filter(|a| *a < self.ids.len())
        {
            if !mods.command {
                self.selected.clear();
            }
            let (lo, hi) = if anchor <= index {
                (anchor, index)
            } else {
                (index, anchor)
            };
            self.selected.extend(self.ids[lo..=hi].iter().copied());
            return;
        }
        if mods.command {
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
        } else {
            self.selected.clear();
            self.selected.insert(id);
        }
        self.anchor = Some(index);
    }

    // ---------------------------------------------------------------- dialogs

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &mut self.confirm else {
            return;
        };
        let mut go = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            let ok_label;
            let mut ok_enabled = true;
            match confirm {
                Confirm::RemoveEntries { ids } => {
                    ui.heading(format!("Remove {} from the database?", entries_word(ids.len() as u64)));
                    ui.add_space(6.0);
                    ui.label("Their stored hashes will be forgotten. The files themselves are not touched.");
                    ok_label = "Remove from database";
                }
                Confirm::RemoveRoot { root, entries } => {
                    ui.heading("Remove this root from the database?");
                    ui.add_space(6.0);
                    ui.monospace(&root.path);
                    ui.add_space(6.0);
                    ui.label(format!(
                        "The folder will no longer be tracked and the stored hashes of its {} will be forgotten. Nothing on disk is touched.",
                        entries_word(*entries)
                    ));
                    ok_label = "Remove from database";
                }
                Confirm::Update { ids, possibly_corrupted, acknowledged } => {
                    ui.heading(format!("Re-hash {} and replace the stored hashes?", entries_word(ids.len() as u64)));
                    ui.add_space(6.0);
                    ui.label(
                        "Do this only for files you know you changed. The old hashes are overwritten, so earlier \
                         damage to these files could no longer be detected.",
                    );
                    if *possibly_corrupted > 0 {
                        ui.add_space(8.0);
                        egui::Frame::new()
                            .stroke(egui::Stroke::new(2.0, RED))
                            .inner_margin(8.0)
                            .corner_radius(4.0)
                            .show(ui, |ui| {
                                ui.colored_label(
                                    RED,
                                    egui::RichText::new(format!(
                                        "WARNING: {} of these {} flagged POSSIBLY CORRUPTED.",
                                        group_digits(*possibly_corrupted),
                                        if *possibly_corrupted == 1 { "is" } else { "are" }
                                    ))
                                    .strong(),
                                );
                                ui.label(
                                    "Their content changed although size and modified time did not, which is the \
                                     signature of bit rot. Updating would accept the damaged data as correct. \
                                     Restore them from a backup first if you can.",
                                );
                                ui.checkbox(acknowledged, "I understand, update them anyway");
                            });
                        ok_enabled = *acknowledged;
                    }
                    ok_label = "Update hashes";
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if ui.add_enabled(ok_enabled, egui::Button::new(ok_label)).clicked() {
                    go = true;
                }
            });
        });
        if go {
            let job = match self.confirm.take() {
                Some(Confirm::RemoveEntries { ids }) => Job::RemoveEntries { ids },
                Some(Confirm::RemoveRoot { root, .. }) => Job::RemoveRoot { root_id: root.id },
                Some(Confirm::Update { ids, .. }) => Job::Update { ids },
                None => return,
            };
            self.start_job(ctx, job);
        } else if cancel || modal.should_close() {
            self.confirm = None;
        }
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
                    ui.weak(format!(
                        "Showing the first {} of {}.",
                        s.problems.len(),
                        s.problem_count
                    ));
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
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
                ui.label(
                    "Detects file changes and bit rot by comparing file hashes with stored ones.",
                );
                ui.add_space(6.0);
                ui.label("hash-check never changes, moves or deletes your files.");
                ui.label("It only writes inside its own folder:");
                ui.monospace(self.appdir.dir().display().to_string());
                ui.add_space(6.0);
                ui.label(format!(
                    "Database schema version {}",
                    crate::db::SCHEMA_VERSION
                ));
                ui.label("Licence: GNU GPL v3");
                ui.hyperlink("https://github.com/camilus123/hash-check");
            });
        self.show_about = open;
    }

    fn export_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.export else {
            return;
        };
        let run = self.last_run;
        let mut dest: Option<ExportDest> = None;
        let mut cancel = false;
        let format = dialog.format;
        let file_name = format!(
            "hash-check-report-{}-run{run}.{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            format.extension()
        );
        let reports_dir = self.appdir.reports_dir();
        let modal = egui::Modal::new(egui::Id::unique("export")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            ui.heading("Export the last verification report");
            ui.add_space(6.0);
            ui.label(
                "The report lists every entry the last verification looked at, problems first, plus new files \
                 and drives that were not available.",
            );
            ui.add_space(6.0);
            for f in [Format::Csv, Format::Text] {
                ui.radio_value(&mut dialog.format, f, f.label());
            }
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if ui
                    .button("Save in hash-check's folder")
                    .on_hover_text(reports_dir.join(&file_name).display().to_string())
                    .clicked()
                {
                    dest = Some(ExportDest::AppDir { file_name: file_name.clone() });
                }
                if ui
                    .button("Choose location…")
                    .on_hover_text("hash-check never overwrites a file: pick a name that does not exist yet.")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Save verification report")
                        .set_file_name(&file_name)
                        .add_filter(format.label(), &[format.extension()])
                        .save_file()
                {
                    dest = Some(ExportDest::Chosen(path));
                }
            });
        });
        if let Some(dest) = dest {
            self.export = None;
            self.start_job(ctx, Job::Export { run, format, dest });
        } else if cancel || modal.should_close() {
            self.export = None;
        }
    }

    fn info_window(&mut self, ctx: &egui::Context) {
        let Some(msg) = self.info.clone() else { return };
        egui::Window::new("Report")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(&msg);
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        self.info = None;
                    }
                    if ui.button("Copy path").clicked()
                        && let Some(path) = msg.lines().nth(1)
                    {
                        ui.ctx().copy_text(path.to_string());
                    }
                });
            });
    }

    fn error_window(&mut self, ctx: &egui::Context) {
        let Some(msg) = self.error.clone() else {
            return;
        };
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
}

fn chip_name(status: Status) -> &'static str {
    match status {
        Status::Ok => "OK",
        Status::Changed => "Changed",
        Status::PossiblyCorrupted => "Possibly corrupted",
        Status::Missing => "Missing",
        Status::Unreadable => "Unreadable",
    }
}

fn entries_word(n: u64) -> String {
    if n == 1 {
        "1 entry".to_string()
    } else {
        format!("{} entries", group_digits(n))
    }
}

fn job_summary(s: &State) -> String {
    let took = s
        .finished_at
        .map(|t| t.duration_since(s.started).as_secs_f64())
        .unwrap_or(0.0);
    let verdict = if s.cancelled { "Cancelled" } else { "Done" };
    let mut text = match s.kind {
        JobKind::Add => format!(
            "{verdict}: {} added in {}",
            group_digits(s.added),
            format_duration(took)
        ),
        JobKind::Update => format!(
            "{verdict}: {} updated in {}",
            group_digits(s.updated),
            format_duration(took)
        ),
        JobKind::Remove => format!(
            "{verdict}: {} removed from the database",
            entries_word(s.removed)
        ),
        JobKind::Verify => format!(
            "{verdict}: {} verified in {}",
            entries_word(s.files_done),
            format_duration(took)
        ),
        JobKind::Export => match &s.report_path {
            Some(p) if s.fatal.is_none() => format!("Report saved: {}", p.display()),
            _ => "Report not saved".to_string(),
        },
    };
    if s.already_tracked > 0 {
        text += &format!(" · {} already tracked", group_digits(s.already_tracked));
    }
    if s.skipped_links > 0 {
        text += &format!(" · {} links not followed", group_digits(s.skipped_links));
    }
    text
}

/// Split a stored path into (folder, file name).
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind(['\\', '/']) {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// A table cell with exactly one tooltip. egui adds its own "full text"
/// tooltip to a label that had to be shortened; with ours on top that made
/// two identical popups, so the automatic one is switched off here.
fn cell(ui: &mut egui::Ui, text: egui::RichText, hover: &str) {
    ui.add(
        egui::Label::new(text)
            .truncate()
            .show_tooltip_when_elided(false),
    )
    .on_hover_text(hover);
}

fn status_label(ui: &mut egui::Ui, e: &Entry) {
    let color = match e.status {
        Status::Ok => egui::Color32::from_rgb(70, 160, 90),
        Status::Changed => egui::Color32::from_rgb(200, 150, 40),
        Status::PossiblyCorrupted => RED,
        Status::Missing | Status::Unreadable => egui::Color32::from_rgb(210, 110, 50),
    };
    let mut text = egui::RichText::new(e.status.label()).color(color);
    if e.status == Status::PossiblyCorrupted {
        text = text.strong();
    }
    let label = ui.add(
        egui::Label::new(text)
            .truncate()
            .show_tooltip_when_elided(false),
    );
    if let Some(err) = &e.last_error {
        label.on_hover_text(err);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dialog_open = self.confirm.is_some() || self.error.is_some() || self.export.is_some();

        // Files dropped onto the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if !dropped.is_empty() && !dialog_open {
            self.start_add(&ctx, dropped);
        }

        // Ctrl+A selects everything shown, unless the user is typing.
        if !dialog_open
            && ctx.memory(|m| m.focused().is_none())
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A))
        {
            self.selected = self.ids.iter().copied().collect();
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
        egui::Panel::left("roots")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| {
                self.roots_panel(ui);
            });
        egui::CentralPanel::default().show(ui, |ui| {
            self.banners(ui);
            self.chips(ui);
            self.filter_bar(ui);
            ui.separator();
            self.table(ui);
        });

        self.confirm_dialog(&ctx);
        self.export_dialog(&ctx);
        self.info_window(&ctx);
        self.problems_window(&ctx);
        self.about_window(&ctx);
        self.error_window(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::split_path;

    #[test]
    fn paths_split_into_folder_and_name() {
        assert_eq!(split_path(r"L:\media\film.mkv"), (r"L:\media\", "film.mkv"));
        assert_eq!(split_path("/home/m/a.txt"), ("/home/m/", "a.txt"));
        assert_eq!(split_path(r"\\nas\share\x"), (r"\\nas\share\", "x"));
        assert_eq!(split_path("plain"), ("", "plain"));
    }
}
