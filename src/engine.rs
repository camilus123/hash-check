//! Background jobs. Nothing slow runs on the UI thread: a job owns a worker
//! thread and its own database connection, and publishes progress through
//! [`Shared`], which the UI polls.
//!
//! Results are committed in small batches, so a cancelled or crashed run
//! keeps everything completed up to the last batch.

use crate::appdata::{AppDir, ReportWriter};
use crate::db::{Db, DbResult, NewEntry, Scope, Status};
use crate::fsread::{self, Kind, WalkItem};
use crate::hasher::Algo;
use crate::hashfile::{HashOutcome, hash_file};
use crate::report::{self, Format};
use crate::util::now_unix;
use crate::verify::{Fingerprint, Verdict, classify, volume_root};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Commit after this many files...
const BATCH_FILES: usize = 500;
/// ...or this much time, whichever comes first.
const BATCH_TIME: Duration = Duration::from_secs(1);
/// Problems kept in memory for display; the total is still counted.
const MAX_LISTED_PROBLEMS: usize = 1000;
/// Database rows removed per transaction.
const REMOVE_CHUNK: usize = 5000;
/// How often a paused job checks whether it may continue.
const PAUSE_POLL: Duration = Duration::from_millis(25);

/// What a job does.
#[derive(Clone, Debug)]
pub enum Job {
    /// Hash new files and folders into the database.
    Add {
        inputs: Vec<PathBuf>,
        recursive: bool,
        algo: Algo,
    },
    /// Re-hash existing entries and replace their stored hashes.
    Update { ids: Vec<i64> },
    /// Delete entries from the database (never from disk).
    RemoveEntries { ids: Vec<i64> },
    /// Forget a root and all its entries (never touching disk).
    RemoveRoot { root_id: i64 },
    /// Compare files with their stored hashes and look for untracked files.
    Verify { target: VerifyTarget },
    /// Hash files that Verify found under a root and start tracking them.
    AddNew { new_file_ids: Vec<i64>, algo: Algo },
    /// Write the report of a verification run.
    Export {
        run: i64,
        format: Format,
        dest: ExportDest,
    },
}

/// What a Verify job checks.
#[derive(Clone, Debug)]
pub enum VerifyTarget {
    /// Every entry, plus a scan of every root for new files.
    All,
    /// The entries of one root, plus a scan of it for new files.
    Root(i64),
    /// Just these entries.
    Entries(Vec<i64>),
}

/// Where a report goes.
#[derive(Clone, Debug)]
pub enum ExportDest {
    /// `reports\<file name>` in the app's own folder.
    AppDir { file_name: String },
    /// A location the user picked. Must not exist yet.
    Chosen(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKind {
    Add,
    Update,
    Remove,
    Verify,
    Export,
}

/// Results of a Verify job so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyCounts {
    pub ok: u64,
    pub changed: u64,
    pub possibly_corrupted: u64,
    pub missing: u64,
    pub unreadable: u64,
    /// Files under a scanned root that are not in the database.
    pub new: u64,
    /// Entries skipped because their drive is not connected.
    pub unavailable: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Finding files / loading entries.
    Preparing,
    Hashing,
    Removing,
    /// Writing a report.
    Writing,
    Finished,
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct State {
    pub kind: JobKind,
    pub phase: Phase,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_total: u64,
    pub current: String,
    /// Entries stored by an Add job.
    pub added: u64,
    /// Entries whose hash an Update job replaced.
    pub updated: u64,
    /// Entries a Remove job deleted from the database.
    pub removed: u64,
    /// Files skipped because they are already in the database.
    pub already_tracked: u64,
    /// Links and junctions that were not followed.
    pub skipped_links: u64,
    /// Files whose size or modified time changed while being hashed.
    pub changed_during_hash: u64,
    pub problem_count: u64,
    pub problems: Vec<Problem>,
    pub cancelled: bool,
    /// The job could not run or had to stop (for example a database error).
    pub fatal: Option<String>,
    pub started: Instant,
    pub hashing_started: Option<Instant>,
    pub finished_at: Option<Instant>,
    pub paused: bool,
    /// Verify: results so far.
    pub verify: VerifyCounts,
    /// Verify: id of this run.
    pub run_id: i64,
    /// Verify: drives that were not connected, with the number of entries skipped.
    pub unavailable_volumes: Vec<(String, u64)>,
    /// Export: where the report was written.
    pub report_path: Option<PathBuf>,
    paused_since: Option<Instant>,
    paused_total: Duration,
}

impl State {
    /// Seconds spent hashing, not counting time spent paused.
    pub fn active_hashing_secs(&self) -> f64 {
        let Some(start) = self.hashing_started else {
            return 0.0;
        };
        let end = self.finished_at.unwrap_or_else(Instant::now);
        let mut paused = self.paused_total;
        if let Some(since) = self.paused_since {
            paused += end.saturating_duration_since(since);
        }
        end.saturating_duration_since(start)
            .saturating_sub(paused)
            .as_secs_f64()
    }
}

pub struct Shared {
    cancel: AtomicBool,
    paused: AtomicBool,
    pub bytes_done: AtomicU64,
    state: Mutex<State>,
}

impl Shared {
    pub fn new(kind: JobKind) -> Shared {
        Shared {
            cancel: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            bytes_done: AtomicU64::new(0),
            state: Mutex::new(State {
                kind,
                phase: Phase::Preparing,
                files_done: 0,
                files_total: 0,
                bytes_total: 0,
                current: String::new(),
                added: 0,
                updated: 0,
                removed: 0,
                already_tracked: 0,
                skipped_links: 0,
                changed_during_hash: 0,
                problem_count: 0,
                problems: Vec::new(),
                cancelled: false,
                fatal: None,
                started: Instant::now(),
                hashing_started: None,
                finished_at: None,
                paused: false,
                verify: VerifyCounts::default(),
                run_id: 0,
                unavailable_volumes: Vec::new(),
                report_path: None,
                paused_since: None,
                paused_total: Duration::ZERO,
            }),
        }
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// Stop as soon as possible; work finished so far is kept.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Pause or resume. A paused job stops reading within one block and
    /// holds no locks on the database.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        self.with(|s| {
            if paused && s.paused_since.is_none() {
                s.paused_since = Some(Instant::now());
            } else if !paused && let Some(since) = s.paused_since.take() {
                s.paused_total += since.elapsed();
            }
            s.paused = paused;
        });
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Block while paused. Returns `false` if the job was cancelled.
    fn checkpoint(&self) -> bool {
        while self.is_paused() && !self.cancelled() {
            std::thread::sleep(PAUSE_POLL);
        }
        !self.cancelled()
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    fn problem(&self, path: &Path, message: String) {
        self.with(|s| {
            s.problem_count += 1;
            if s.problems.len() < MAX_LISTED_PROBLEMS {
                s.problems.push(Problem {
                    path: path.display().to_string(),
                    message,
                });
            }
        });
    }
}

impl Job {
    pub fn kind(&self) -> JobKind {
        match self {
            Job::Update { .. } => JobKind::Update,
            Job::RemoveEntries { .. } | Job::RemoveRoot { .. } => JobKind::Remove,
            Job::Add { .. } | Job::AddNew { .. } => JobKind::Add,
            Job::Verify { .. } => JobKind::Verify,
            Job::Export { .. } => JobKind::Export,
        }
    }
}

/// Start `job` on a worker thread against the database at `db_path`.
/// `wake` is called when the job finishes so the UI can repaint.
pub fn start(db_path: PathBuf, job: Job, wake: Box<dyn Fn() + Send>) -> Arc<Shared> {
    let shared = Arc::new(Shared::new(job.kind()));
    start_on(shared.clone(), db_path, job, wake);
    shared
}

/// Like [`start`], with a caller-supplied control block (lets a job start
/// already paused).
pub fn start_on(shared: Arc<Shared>, db_path: PathBuf, job: Job, wake: Box<dyn Fn() + Send>) {
    std::thread::Builder::new()
        .name("hash-check-job".into())
        .spawn(move || {
            run(&shared, &db_path, job);
            wake();
        })
        .expect("failed to start the worker thread");
}

/// Run `job` on the calling thread and return its final state.
pub fn run_blocking(db_path: &Path, job: Job) -> State {
    let shared = Shared::new(job.kind());
    run(&shared, db_path, job);
    shared.snapshot()
}

/// Convenience for tests: a blocking Add.
pub fn add_blocking(db_path: &Path, inputs: &[PathBuf], recursive: bool, algo: Algo) -> State {
    run_blocking(
        db_path,
        Job::Add {
            inputs: inputs.to_vec(),
            recursive,
            algo,
        },
    )
}

fn run(sh: &Shared, db_path: &Path, job: Job) {
    let result = Db::open(db_path)
        .map_err(|e| e.to_string())
        .and_then(|mut db| match job {
            Job::Add {
                inputs,
                recursive,
                algo,
            } => run_add(sh, &db, &inputs, recursive, algo),
            Job::Update { ids } => run_update(sh, &db, &ids),
            Job::RemoveEntries { ids } => run_remove_entries(sh, &db, &ids),
            Job::RemoveRoot { root_id } => run_remove_root(sh, &mut db, root_id),
            Job::Verify { target } => run_verify(sh, &mut db, target),
            Job::AddNew { new_file_ids, algo } => run_add_new(sh, &db, &new_file_ids, algo),
            Job::Export { run, format, dest } => run_export(sh, &db, db_path, run, format, dest),
        });
    sh.with(|s| {
        if let Err(msg) = result {
            s.fatal = Some(msg);
        }
        s.cancelled = sh.cancelled();
        s.phase = Phase::Finished;
        s.current.clear();
        if let Some(since) = s.paused_since.take() {
            s.paused_total += since.elapsed();
        }
        s.paused = false;
        s.finished_at = Some(Instant::now());
    });
}

/// Batches database writes into short transactions.
struct Batch<'a> {
    db: &'a Db,
    open: bool,
    writes: usize,
    started: Instant,
}

impl<'a> Batch<'a> {
    fn new(db: &'a Db) -> Batch<'a> {
        Batch {
            db,
            open: false,
            writes: 0,
            started: Instant::now(),
        }
    }

    /// Wait out a pause (without holding the write lock), then make sure a
    /// transaction is open. Returns `false` if the job was cancelled.
    fn next(&mut self, sh: &Shared) -> Result<bool, String> {
        if sh.is_paused() {
            self.commit()?;
        }
        if !sh.checkpoint() {
            return Ok(false);
        }
        if !self.open {
            self.db.begin().map_err(|e| e.to_string())?;
            self.open = true;
            self.writes = 0;
            self.started = Instant::now();
        }
        Ok(true)
    }

    /// Count a write and commit if the batch is full or old enough.
    fn wrote(&mut self) -> Result<(), String> {
        self.writes += 1;
        if self.writes >= BATCH_FILES || self.started.elapsed() >= BATCH_TIME {
            self.commit()?;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), String> {
        if self.open {
            self.db.commit().map_err(|e| e.to_string())?;
            self.open = false;
        }
        Ok(())
    }
}

/// Hash one file with progress, pause and cancel, keeping the byte counter
/// aligned with `planned_size` whatever happens.
fn hash_one(
    sh: &Shared,
    buf: &mut Vec<u8>,
    path: &Path,
    algo: Algo,
    planned_size: u64,
) -> std::io::Result<HashOutcome> {
    let mut read: u64 = 0;
    let outcome = hash_file(path, algo, buf, true, &mut |n| {
        read += n;
        sh.bytes_done.fetch_add(n, Ordering::Relaxed);
        sh.checkpoint()
    });
    if !matches!(outcome, Ok(HashOutcome::Cancelled)) {
        if read < planned_size {
            sh.bytes_done
                .fetch_add(planned_size - read, Ordering::Relaxed);
        } else if read > planned_size {
            sh.bytes_done
                .fetch_sub(read - planned_size, Ordering::Relaxed);
        }
    }
    outcome
}

/// One file to hash.
struct Item {
    /// Database id for Update; `None` for a new file.
    id: Option<i64>,
    path: String,
    size: u64,
    root_id: Option<i64>,
    algo: Algo,
}

/// Hash `items` one after another, handing each finished hash to `store`
/// inside batched transactions.
fn hash_items(
    sh: &Shared,
    db: &Db,
    items: &[Item],
    store: &mut dyn FnMut(&Item, &[u8], u64, i64) -> DbResult<()>,
) -> Result<(), String> {
    sh.with(|s| {
        s.phase = Phase::Hashing;
        s.hashing_started = Some(Instant::now());
        s.files_total = items.len() as u64;
        s.bytes_total = items.iter().map(|p| p.size).sum();
    });
    let mut buf = Vec::new();
    let mut batch = Batch::new(db);
    for p in items {
        if !batch.next(sh)? {
            break;
        }
        sh.with(|s| s.current.clone_from(&p.path));
        let path = Path::new(&p.path);
        match hash_one(sh, &mut buf, path, p.algo, p.size) {
            Ok(HashOutcome::Done {
                hash,
                size,
                mtime_ns,
            }) => {
                store(p, &hash, size, mtime_ns).map_err(|e| e.to_string())?;
                batch.wrote()?;
            }
            Ok(HashOutcome::ChangedDuringHash) => {
                sh.with(|s| s.changed_during_hash += 1);
                sh.problem(
                    path,
                    "changed while it was being hashed; result discarded, try again later".into(),
                );
            }
            Ok(HashOutcome::Cancelled) => break,
            Err(e) => sh.problem(path, e.to_string()),
        }
        sh.with(|s| s.files_done += 1);
    }
    // Cancelled or complete: keep everything hashed so far.
    batch.commit()
}

fn run_add(
    sh: &Shared,
    db: &Db,
    inputs: &[PathBuf],
    recursive: bool,
    algo: Algo,
) -> Result<(), String> {
    let mut new_roots = Vec::new();
    let result = scan(sh, db, inputs, recursive, algo, &mut new_roots).and_then(|items| {
        if sh.cancelled() {
            return Ok(());
        }
        hash_items(sh, db, &items, &mut |p, hash, size, mtime_ns| {
            let inserted = db.insert_entry(&NewEntry {
                path: &p.path,
                root_id: p.root_id,
                size,
                mtime_ns,
                algo: p.algo,
                hash,
                now: now_unix(),
            })?;
            sh.with(|s| {
                if inserted {
                    s.added += 1
                } else {
                    s.already_tracked += 1
                }
            });
            Ok(())
        })
    });
    // A run that was stopped must not leave behind a root it created but
    // never stored a file for. (A completed run keeps even an empty folder
    // as a root, so files appearing in it later are noticed.)
    if sh.cancelled() || result.is_err() {
        for id in new_roots {
            let _ = db.delete_root_if_empty(id);
        }
    }
    result
}

fn run_update(sh: &Shared, db: &Db, ids: &[i64]) -> Result<(), String> {
    let mut items = Vec::with_capacity(ids.len());
    for (i, id) in ids.iter().enumerate() {
        if !sh.checkpoint() {
            return Ok(());
        }
        // Entries removed in the meantime are simply skipped.
        if let Some(e) = db.entry(*id).map_err(|e| e.to_string())? {
            // Each entry keeps the algorithm it was created with.
            items.push(Item {
                id: Some(e.id),
                path: e.path,
                size: e.size,
                root_id: e.root_id,
                algo: e.algo,
            });
        }
        if i % 1000 == 0 {
            sh.with(|s| s.files_total = items.len() as u64);
        }
    }
    hash_items(sh, db, &items, &mut |p, hash, size, mtime_ns| {
        let id = p.id.expect("update items carry an id");
        if db.replace_hash(id, size, mtime_ns, hash, now_unix())? {
            sh.with(|s| s.updated += 1);
        }
        Ok(())
    })
}

/// Start tracking files that a verification found under a root.
fn run_add_new(sh: &Shared, db: &Db, new_file_ids: &[i64], algo: Algo) -> Result<(), String> {
    let mut items = Vec::with_capacity(new_file_ids.len());
    for id in new_file_ids {
        if !sh.checkpoint() {
            return Ok(());
        }
        if let Some(n) = db.new_file(*id).map_err(|e| e.to_string())? {
            items.push(Item {
                id: None,
                path: n.path,
                size: n.size,
                root_id: Some(n.root_id),
                algo,
            });
        }
    }
    hash_items(sh, db, &items, &mut |p, hash, size, mtime_ns| {
        // Storing the entry also takes the file off the "new" list.
        let inserted = db.insert_entry(&NewEntry {
            path: &p.path,
            root_id: p.root_id,
            size,
            mtime_ns,
            algo: p.algo,
            hash,
            now: now_unix(),
        })?;
        sh.with(|s| {
            if inserted {
                s.added += 1
            } else {
                s.already_tracked += 1
            }
        });
        Ok(())
    })
}

/// Remembers which volumes are reachable, asking the OS once per volume.
struct Volumes {
    available: HashMap<String, bool>,
    skipped: Vec<(String, u64)>,
}

impl Volumes {
    fn new() -> Volumes {
        Volumes {
            available: HashMap::new(),
            skipped: Vec::new(),
        }
    }

    /// Is the volume holding `path` reachable? Paths without a
    /// recognisable volume count as reachable.
    fn is_available(&mut self, path: &str) -> bool {
        let Some(root) = volume_root(path) else {
            return true;
        };
        if let Some(known) = self.available.get(root) {
            return *known;
        }
        let ok = fsread::stat(Path::new(root)).is_ok();
        self.available.insert(root.to_string(), ok);
        ok
    }

    fn note_skipped(&mut self, path: &str, entries: u64) {
        let root = volume_root(path).unwrap_or(path).to_string();
        match self.skipped.iter_mut().find(|(v, _)| *v == root) {
            Some((_, n)) => *n += entries,
            None => self.skipped.push((root, entries)),
        }
    }
}

fn run_verify(sh: &Shared, db: &mut Db, target: VerifyTarget) -> Result<(), String> {
    let e = |e: crate::db::DbError| e.to_string();
    let run = db.begin_run().map_err(e)?;
    sh.with(|s| s.run_id = run);
    let db: &Db = db;

    let (ids, bytes_total, roots, scope_text) = match &target {
        VerifyTarget::All => (
            db.all_ids().map_err(e)?,
            db.total_size().map_err(e)?,
            db.roots().map_err(e)?,
            "everything".to_string(),
        ),
        VerifyTarget::Root(id) => {
            let roots: Vec<_> = db
                .roots()
                .map_err(e)?
                .into_iter()
                .filter(|r| r.id == *id)
                .collect();
            let text = roots
                .first()
                .map(|r| format!("root {}", r.path))
                .unwrap_or_else(|| "a removed root".into());
            (
                db.ids_filtered(Scope::Root(*id), "").map_err(e)?,
                db.size_in_scope(Scope::Root(*id)).map_err(e)?,
                roots,
                text,
            )
        }
        VerifyTarget::Entries(ids) => (
            ids.clone(),
            db.size_of(ids).map_err(e)?,
            Vec::new(),
            format!("{} selected entries", ids.len()),
        ),
    };
    db.begin().map_err(e)?;
    for (key, value) in [
        (report::meta::RUN_OF_META, run.to_string()),
        (report::meta::SCOPE, scope_text),
        (report::meta::STARTED, now_unix().to_string()),
        (report::meta::FINISHED, String::new()),
        (report::meta::CANCELLED, "1".to_string()), // until it finishes properly
        (report::meta::UNAVAILABLE, String::new()),
    ] {
        db.set_meta(key, &value).map_err(e)?;
    }
    db.commit().map_err(e)?;

    let mut volumes = Volumes::new();
    let mut batch = Batch::new(db);

    // 1. Look for files under the roots that are not tracked yet.
    for root in &roots {
        if !batch.next(sh)? {
            break;
        }
        sh.with(|s| s.current.clone_from(&root.path));
        if fsread::stat(Path::new(&root.path)).is_err() {
            if !volumes.is_available(&root.path) {
                // Offline drive: keep earlier findings, the entries below are skipped too.
                continue;
            }
            sh.problem(
                Path::new(&root.path),
                "tracked folder no longer exists".into(),
            );
            db.clear_new_files(root.id).map_err(e)?;
            batch.wrote()?;
            continue;
        }
        db.clear_new_files(root.id).map_err(e)?;
        let mut db_err = None;
        let now = now_unix();
        fsread::walk(Path::new(&root.path), root.recursive, &mut |item| {
            match item {
                WalkItem::File { path, meta } => match path.to_str() {
                    Some(p) => {
                        let res = db.file_exists(p).and_then(|known| {
                            if !known {
                                db.insert_new_file(p, root.id, meta.size, meta.mtime_ns, now)?;
                                sh.with(|s| s.verify.new += 1);
                            }
                            Ok(())
                        });
                        if let Err(err) = res {
                            db_err = Some(err.to_string());
                        }
                    }
                    None => sh.problem(
                        path,
                        "file name is not valid Unicode; cannot be tracked".into(),
                    ),
                },
                WalkItem::SkippedLink { .. } => sh.with(|s| s.skipped_links += 1),
                WalkItem::Error { path, error } => sh.problem(path, error.to_string()),
            }
            db_err.is_none() && !sh.cancelled()
        });
        if let Some(err) = db_err {
            return Err(err);
        }
        batch.wrote()?;
    }
    batch.commit()?;

    // 2. Re-hash every entry and compare.
    sh.with(|s| {
        s.phase = Phase::Hashing;
        s.hashing_started = Some(Instant::now());
        s.files_total = ids.len() as u64;
        s.bytes_total = bytes_total;
    });
    let mut buf = Vec::new();
    let mut completed = true;
    for id in &ids {
        if !batch.next(sh)? {
            completed = false;
            break;
        }
        // Entries removed in the meantime are simply skipped.
        let Some(entry) = db.entry(*id).map_err(e)? else {
            sh.with(|s| s.files_done += 1);
            continue;
        };
        sh.with(|s| s.current.clone_from(&entry.path));
        let path = Path::new(&entry.path);
        let skip_bytes = || {
            sh.bytes_done.fetch_add(entry.size, Ordering::Relaxed);
        };

        // (status, detail) to record, or None to leave the entry untouched.
        let mut finding: Option<(Status, String)> = None;
        let mut ok: Option<(u64, i64)> = None;
        match fsread::stat(path) {
            Err(err) => {
                skip_bytes();
                if !volumes.is_available(&entry.path) {
                    volumes.note_skipped(&entry.path, 1);
                    sh.with(|s| s.verify.unavailable += 1);
                } else if err.kind() == std::io::ErrorKind::NotFound {
                    finding = Some((Status::Missing, "the file no longer exists".into()));
                } else {
                    finding = Some((Status::Unreadable, err.to_string()));
                }
            }
            Ok(meta) if meta.kind != Kind::File => {
                skip_bytes();
                finding = Some((
                    Status::Unreadable,
                    "this is no longer a regular file".into(),
                ));
            }
            Ok(_) => match hash_one(sh, &mut buf, path, entry.algo, entry.size) {
                Ok(HashOutcome::Done {
                    hash,
                    size,
                    mtime_ns,
                }) => {
                    let stored = Fingerprint {
                        hash: &entry.hash,
                        size: entry.size,
                        mtime_ns: entry.mtime_ns,
                    };
                    let current = Fingerprint {
                        hash: &hash,
                        size,
                        mtime_ns,
                    };
                    match classify(stored, current) {
                        Verdict::Ok => ok = Some((size, mtime_ns)),
                        Verdict::Changed {
                            size_differs,
                            mtime_differs,
                        } => {
                            let mut what = Vec::new();
                            if size_differs {
                                what.push(format!("size {} -> {} bytes", entry.size, size));
                            }
                            if mtime_differs {
                                what.push(format!(
                                    "modified {} -> {}",
                                    crate::util::format_unix_nanos(entry.mtime_ns),
                                    crate::util::format_unix_nanos(mtime_ns)
                                ));
                            }
                            finding = Some((
                                Status::Changed,
                                format!("content differs; {}", what.join(", ")),
                            ));
                        }
                        Verdict::PossiblyCorrupted => {
                            finding = Some((
                                Status::PossiblyCorrupted,
                                format!(
                                    "content differs although size and modified time are unchanged; current {} {}",
                                    entry.algo.name(),
                                    crate::hasher::to_hex(&hash)
                                ),
                            ));
                        }
                    }
                }
                Ok(HashOutcome::ChangedDuringHash) => {
                    sh.with(|s| s.changed_during_hash += 1);
                    sh.problem(
                        path,
                        "changed while it was being verified; result discarded, verify it again later".into(),
                    );
                }
                Ok(HashOutcome::Cancelled) => {
                    completed = false;
                    break;
                }
                // The file was there a moment ago: a read error on a file
                // that used to be fine is itself a warning sign.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    finding = Some((Status::Missing, "the file no longer exists".into()));
                }
                Err(err) => finding = Some((Status::Unreadable, err.to_string())),
            },
        }

        let now = now_unix();
        if let Some((size, mtime_ns)) = ok {
            db.record_verified_ok(entry.id, size, mtime_ns, now, run)
                .map_err(e)?;
            sh.with(|s| s.verify.ok += 1);
            batch.wrote()?;
        } else if let Some((status, detail)) = finding {
            db.record_verified_problem(entry.id, status, &detail, now, run)
                .map_err(e)?;
            sh.with(|s| match status {
                Status::Changed => s.verify.changed += 1,
                Status::PossiblyCorrupted => s.verify.possibly_corrupted += 1,
                Status::Missing => s.verify.missing += 1,
                Status::Unreadable => s.verify.unreadable += 1,
                Status::Ok => {}
            });
            batch.wrote()?;
        }
        sh.with(|s| s.files_done += 1);
    }
    batch.commit()?;

    let unavailable: String = volumes
        .skipped
        .iter()
        .map(|(v, n)| format!("{v}\t{n}\n"))
        .collect();
    sh.with(|s| s.unavailable_volumes.clone_from(&volumes.skipped));
    db.begin().map_err(e)?;
    db.set_meta(report::meta::FINISHED, &now_unix().to_string())
        .map_err(e)?;
    db.set_meta(
        report::meta::CANCELLED,
        if completed && !sh.cancelled() {
            "0"
        } else {
            "1"
        },
    )
    .map_err(e)?;
    db.set_meta(report::meta::UNAVAILABLE, &unavailable)
        .map_err(e)?;
    db.commit().map_err(e)?;
    Ok(())
}

fn run_export(
    sh: &Shared,
    db: &Db,
    db_path: &Path,
    run: i64,
    format: Format,
    dest: ExportDest,
) -> Result<(), String> {
    sh.with(|s| s.phase = Phase::Writing);
    let (path, mut file) = match dest {
        ExportDest::AppDir { file_name } => {
            let dir = db_path
                .parent()
                .ok_or("the database has no parent folder")?;
            AppDir::at(dir)
                .create_report(&file_name)
                .map_err(|e| e.to_string())?
        }
        ExportDest::Chosen(path) => {
            let file =
                ReportWriter::start(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            (path, file)
        }
    };
    sh.with(|s| s.current = path.display().to_string());
    let rows = report::write_report(db, run, format, &mut file).map_err(|e| e.to_string())?;
    file.finish().map_err(|e| e.to_string())?;
    sh.with(|s| {
        s.files_done = rows;
        s.report_path = Some(path);
    });
    Ok(())
}

fn run_remove_entries(sh: &Shared, db: &Db, ids: &[i64]) -> Result<(), String> {
    sh.with(|s| {
        s.phase = Phase::Removing;
        s.files_total = ids.len() as u64;
    });
    for chunk in ids.chunks(REMOVE_CHUNK) {
        if !sh.checkpoint() {
            break;
        }
        db.begin().map_err(|e| e.to_string())?;
        let n = match db.delete_entries(chunk) {
            Ok(n) => n,
            Err(e) => {
                let _ = db.rollback();
                return Err(e.to_string());
            }
        };
        db.commit().map_err(|e| e.to_string())?;
        sh.with(|s| {
            s.removed += n;
            s.files_done += chunk.len() as u64;
        });
    }
    Ok(())
}

fn run_remove_root(sh: &Shared, db: &mut Db, root_id: i64) -> Result<(), String> {
    sh.with(|s| s.phase = Phase::Removing);
    let n = db.delete_root(root_id).map_err(|e| e.to_string())?;
    sh.with(|s| s.removed = n);
    Ok(())
}

struct Scanner<'a> {
    sh: &'a Shared,
    db: &'a Db,
    algo: Algo,
    items: Vec<Item>,
    seen: HashSet<String>,
    db_err: Option<String>,
    last_publish: Instant,
}

impl Scanner<'_> {
    fn consider(&mut self, path: &Path, size: u64, root_id: Option<i64>) {
        let Some(s) = path.to_str() else {
            self.sh.problem(
                path,
                "file name is not valid Unicode; cannot be tracked".into(),
            );
            return;
        };
        if self.seen.contains(s) {
            return;
        }
        match self.db.file_exists(s) {
            Ok(true) => self.sh.with(|st| st.already_tracked += 1),
            Ok(false) => {
                self.seen.insert(s.to_string());
                self.items.push(Item {
                    id: None,
                    path: s.to_string(),
                    size,
                    root_id,
                    algo: self.algo,
                });
            }
            Err(e) => self.db_err = Some(e.to_string()),
        }
        if self.last_publish.elapsed() >= Duration::from_millis(100) {
            self.last_publish = Instant::now();
            let n = self.items.len() as u64;
            self.sh.with(|st| st.files_total = n);
        }
    }
}

/// Enumerate the files to hash, registering directory roots on the way.
/// Ids of roots this call created are appended to `new_roots`.
fn scan(
    sh: &Shared,
    db: &Db,
    inputs: &[PathBuf],
    recursive: bool,
    algo: Algo,
    new_roots: &mut Vec<i64>,
) -> Result<Vec<Item>, String> {
    let mut sc = Scanner {
        sh,
        db,
        algo,
        items: Vec::new(),
        seen: HashSet::new(),
        db_err: None,
        last_publish: Instant::now(),
    };
    for input in inputs {
        if !sh.checkpoint() {
            break;
        }
        sh.with(|s| s.current = input.display().to_string());
        let resolved = match fsread::resolve(input) {
            Ok(p) => p,
            Err(e) => {
                sh.problem(input, e.to_string());
                continue;
            }
        };
        let meta = match fsread::stat(&resolved) {
            Ok(m) => m,
            Err(e) => {
                sh.problem(&resolved, e.to_string());
                continue;
            }
        };
        match meta.kind {
            Kind::File => sc.consider(&resolved, meta.size, None),
            Kind::Dir => {
                let Some(root_str) = resolved.to_str() else {
                    sh.problem(
                        &resolved,
                        "folder name is not valid Unicode; cannot be tracked".into(),
                    );
                    continue;
                };
                let (root_id, created) = db
                    .add_root(root_str, recursive, now_unix())
                    .map_err(|e| e.to_string())?;
                if created {
                    new_roots.push(root_id);
                }
                fsread::walk(&resolved, recursive, &mut |item| {
                    match item {
                        WalkItem::File { path, meta } => {
                            sc.consider(path, meta.size, Some(root_id))
                        }
                        WalkItem::SkippedLink { .. } => sh.with(|s| s.skipped_links += 1),
                        WalkItem::Error { path, error } => sh.problem(path, error.to_string()),
                    }
                    sc.db_err.is_none() && sh.checkpoint()
                });
            }
            Kind::Link | Kind::Other => {
                sh.problem(&resolved, "not a regular file or folder".into())
            }
        }
        if let Some(e) = sc.db_err.take() {
            return Err(e);
        }
    }
    Ok(sc.items)
}
