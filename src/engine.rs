//! Background jobs. Nothing slow runs on the UI thread: a job owns a worker
//! thread and its own database connection, and publishes progress through
//! [`Shared`], which the UI polls.
//!
//! Results are committed in small batches, so a cancelled or crashed run
//! keeps everything completed up to the last batch.

use crate::appdata::{AppDir, ReportWriter};
use crate::db::{Db, DbResult, NewEntry, Scope, Status};
use crate::drives::{Detector, DrivePlan, MAX_THREADS};
use crate::fsread::{self, Kind, WalkItem};
use crate::hasher::Algo;
use crate::hashfile::{HashOpts, HashOutcome, LinkCache, ReadBuf, hash_file};
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
    /// Run SQLite's full integrity check on the database.
    CheckDatabase,
    /// Measure hashing speed on existing files (read-only) and in memory.
    Benchmark {
        inputs: Vec<PathBuf>,
        limit_bytes: u64,
    },
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
    Benchmark,
    CheckDatabase,
}

/// One measured line of a benchmark.
#[derive(Clone, Debug, PartialEq)]
pub struct BenchRow {
    pub label: String,
    pub bytes: u64,
    pub secs: f64,
}

impl BenchRow {
    pub fn mb_per_s(&self) -> f64 {
        if self.secs > 0.0 {
            self.bytes as f64 / 1_048_576.0 / self.secs
        } else {
            0.0
        }
    }
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
    /// Files not read again because they are hard links to a file already
    /// hashed in this run.
    pub hard_links_reused: u64,
    /// A message for the user when the job finishes (database check).
    pub message: Option<String>,
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
    /// Benchmark: results so far.
    pub bench: Vec<BenchRow>,
    /// Benchmark: what was measured (files, drive, CPU).
    pub bench_info: String,
    /// Benchmark: what is being measured right now.
    pub bench_step: String,
    /// How each drive involved is being read, for display.
    pub plans: Vec<String>,
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
                hard_links_reused: 0,
                message: None,
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
                bench: Vec::new(),
                bench_info: String::new(),
                bench_step: String::new(),
                plans: Vec::new(),
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

    /// Show `path` as the file being worked on. With several threads this
    /// is simply the most recently started one.
    fn set_current(&self, path: &Path) {
        if let Some(p) = path.to_str() {
            self.with(|s| {
                s.current.clear();
                s.current.push_str(p);
            });
        }
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
            Job::Benchmark { .. } => JobKind::Benchmark,
            Job::CheckDatabase => JobKind::CheckDatabase,
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
    let what = job.describe();
    run_job(sh, db_path, job);
    // Keep a plain-text record of every job next to the database. A log
    // that cannot be written must never fail the job itself.
    if let Some(dir) = db_path.parent() {
        let _ = AppDir::at(dir).append_log(&log_text(&what, &sh.snapshot()));
    }
    sh.with(|s| s.phase = Phase::Finished);
}

fn run_job(sh: &Shared, db_path: &Path, job: Job) {
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
            Job::Verify { target } => run_verify(sh, &mut db, db_path, target),
            Job::AddNew { new_file_ids, algo } => run_add_new(sh, &db, &new_file_ids, algo),
            Job::Export { run, format, dest } => run_export(sh, &db, db_path, run, format, dest),
            Job::Benchmark {
                inputs,
                limit_bytes,
            } => run_benchmark(sh, &db, &inputs, limit_bytes),
            Job::CheckDatabase => run_check_database(sh, &db),
        });
    sh.with(|s| {
        if let Err(msg) = result {
            s.fatal = Some(msg);
        }
        s.cancelled = sh.cancelled();
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

    /// Make sure a transaction is open before writing.
    fn begin(&mut self) -> Result<(), String> {
        if !self.open {
            self.db.begin().map_err(|e| e.to_string())?;
            self.open = true;
            self.writes = 0;
            self.started = Instant::now();
        }
        Ok(())
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
        self.begin()?;
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

    /// Called when no result is waiting: never sit on the write lock while
    /// paused, and do not let a quiet batch grow old.
    fn idle(&mut self, sh: &Shared) -> Result<(), String> {
        if self.open && (sh.is_paused() || self.started.elapsed() >= BATCH_TIME) {
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

// ---------------------------------------------------------------------------
// Per-drive scheduling
// ---------------------------------------------------------------------------

/// Files at least this large are hashed with BLAKE3's multithreaded mode
/// even when several files are being read in parallel.
const BIG_FILE: u64 = 16 * 1024 * 1024;

/// Results waiting for the database writer; workers block when it is full.
const RESULT_QUEUE: usize = 4096;

/// The work for one physical drive.
struct Pool<T> {
    plan: DrivePlan,
    items: Mutex<std::vec::IntoIter<T>>,
}

/// What each worker thread owns.
struct Ctx {
    buf: ReadBuf,
    /// The drive is read one file at a time.
    sequential: bool,
    direct: bool,
    /// Read-only database connection, opened on first use (Verify).
    db: Option<Db>,
}

/// Sorts work into one pool per physical drive, keeping its order.
struct Grouper<'a, T> {
    db: &'a Db,
    detector: Detector,
    /// volume root -> index into `pools`
    by_volume: HashMap<String, usize>,
    /// device key -> index into `pools`
    by_device: HashMap<String, usize>,
    pools: Vec<(DrivePlan, Vec<T>)>,
}

impl<'a, T> Grouper<'a, T> {
    fn new(db: &'a Db) -> Grouper<'a, T> {
        Grouper {
            db,
            detector: Detector::new(),
            by_volume: HashMap::new(),
            by_device: HashMap::new(),
            pools: Vec::new(),
        }
    }

    fn push(&mut self, path: &str, item: T) -> DbResult<()> {
        let volume = volume_root(path).unwrap_or("");
        let index = match self.by_volume.get(volume) {
            Some(i) => *i,
            None => {
                let info = self.detector.drive_of(path).clone();
                let setting = drive_threads_setting(self.db, volume)?;
                let threads = setting
                    .unwrap_or_else(|| info.default_threads())
                    .clamp(1, MAX_THREADS);
                let key = info.device_key();
                let index = match self.by_device.get(&key) {
                    Some(i) => {
                        // Two volumes on one device: honour the more careful setting.
                        let plan = &mut self.pools[*i].0;
                        plan.threads = plan.threads.min(threads);
                        plan.overridden |= setting.is_some();
                        *i
                    }
                    None => {
                        self.pools.push((
                            DrivePlan {
                                info,
                                threads,
                                overridden: setting.is_some(),
                            },
                            Vec::new(),
                        ));
                        self.by_device.insert(key, self.pools.len() - 1);
                        self.pools.len() - 1
                    }
                };
                self.by_volume.insert(volume.to_string(), index);
                index
            }
        };
        self.pools[index].1.push(item);
        Ok(())
    }

    fn finish(self) -> Vec<Pool<T>> {
        self.pools
            .into_iter()
            .map(|(plan, items)| Pool {
                plan,
                items: Mutex::new(items.into_iter()),
            })
            .collect()
    }
}

/// Meta key holding the user's thread count for a volume.
pub fn drive_threads_key(volume: &str) -> String {
    format!("drive_threads:{volume}")
}

/// Meta key: "0" when reads should go through the OS file cache instead of
/// bypassing it (direct I/O, the default).
pub const DIRECT_IO_KEY: &str = "direct_io";

fn drive_threads_setting(db: &Db, volume: &str) -> DbResult<Option<usize>> {
    Ok(db
        .meta(&drive_threads_key(volume))?
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1))
}

fn direct_io_setting(db: &Db) -> DbResult<bool> {
    // On unless switched off: the benchmark showed no speed difference on
    // any drive type, and direct reads guarantee the data comes from the disk.
    Ok(db.meta(DIRECT_IO_KEY)?.as_deref() != Some("0"))
}

/// Run every pool with its own threads, all drives at the same time.
/// `work` runs on worker threads and returns `Ok(None)` when it was
/// cancelled mid-item; `sink` runs on the calling thread (which owns the
/// database connection) for every result; `idle` runs when nothing is
/// waiting.
fn run_pools<T: Send, R: Send>(
    sh: &Shared,
    pools: &[Pool<T>],
    direct: bool,
    work: &(dyn Fn(&mut Ctx, T) -> Result<Option<R>, String> + Sync),
    sink: &mut dyn FnMut(R) -> Result<(), String>,
    idle: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    sh.with(|s| s.plans = pools.iter().map(|p| p.plan.describe()).collect());
    let failure: Mutex<Option<String>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let mut outcome = Ok(());
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<R>(RESULT_QUEUE);
        for pool in pools {
            for _ in 0..pool.plan.threads.max(1) {
                let tx = tx.clone();
                let (failure, stop) = (&failure, &stop);
                scope.spawn(move || {
                    let mut ctx = Ctx {
                        buf: ReadBuf::new(),
                        sequential: pool.plan.sequential(),
                        direct,
                        db: None,
                    };
                    loop {
                        if stop.load(Ordering::Relaxed) || !sh.checkpoint() {
                            break;
                        }
                        let Some(item) = pool.items.lock().unwrap().next() else {
                            break;
                        };
                        match work(&mut ctx, item) {
                            Ok(Some(result)) => {
                                if tx.send(result).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(msg) => {
                                failure.lock().unwrap().get_or_insert(msg);
                                stop.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                });
            }
        }
        drop(tx);
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => {
                    if let Err(e) = sink(result) {
                        outcome = Err(e);
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(e) = idle() {
                        outcome = Err(e);
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // On an error, release the workers: stop them and unblock any that
        // is waiting to hand over a result.
        stop.store(true, Ordering::Relaxed);
        drop(rx);
    });
    outcome?;
    match failure.into_inner().unwrap() {
        Some(msg) => Err(msg),
        None => Ok(()),
    }
}

/// Hash one file with progress, pause and cancel, keeping the byte counter
/// aligned with `planned_size` whatever happens.
fn hash_one(
    sh: &Shared,
    ctx: &mut Ctx,
    path: &Path,
    algo: Algo,
    planned_size: u64,
    links: Option<&LinkCache>,
) -> std::io::Result<HashOutcome> {
    sh.set_current(path);
    let opts = HashOpts {
        // One file at a time: give BLAKE3 all the cores. Many files at
        // once: only large files are worth splitting further.
        parallel: ctx.sequential || planned_size >= BIG_FILE,
        direct: ctx.direct,
    };
    let mut read: u64 = 0;
    let outcome = hash_file(path, algo, &mut ctx.buf, opts, links, &mut |n| {
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

/// What a worker reports for an [`Item`].
enum Hashed<'a> {
    Done {
        item: &'a Item,
        hash: Vec<u8>,
        size: u64,
        mtime_ns: i64,
    },
    ChangedDuringHash(&'a Item),
    Failed(&'a Item, std::io::Error),
}

/// Hash `items`, each drive with its own threads, handing every finished
/// hash to `store` (on this thread) inside batched transactions.
fn hash_items(
    sh: &Shared,
    db: &Db,
    items: &[Item],
    links: Option<&LinkCache>,
    store: &mut dyn FnMut(&Item, &[u8], u64, i64) -> DbResult<()>,
) -> Result<(), String> {
    let mut grouper = Grouper::new(db);
    for item in items {
        grouper.push(&item.path, item).map_err(|e| e.to_string())?;
    }
    let pools = grouper.finish();
    let direct = direct_io_setting(db).map_err(|e| e.to_string())?;
    sh.with(|s| {
        s.phase = Phase::Hashing;
        s.hashing_started = Some(Instant::now());
        s.files_total = items.len() as u64;
        s.bytes_total = items.iter().map(|p| p.size).sum();
    });
    let batch = std::cell::RefCell::new(Batch::new(db));
    let result = run_pools(
        sh,
        &pools,
        direct,
        &|ctx, item: &Item| {
            Ok(
                match hash_one(sh, ctx, Path::new(&item.path), item.algo, item.size, links) {
                    Ok(HashOutcome::Done {
                        hash,
                        size,
                        mtime_ns,
                        reused,
                    }) => {
                        if reused {
                            sh.with(|s| s.hard_links_reused += 1);
                        }
                        Some(Hashed::Done {
                            item,
                            hash,
                            size,
                            mtime_ns,
                        })
                    }
                    Ok(HashOutcome::ChangedDuringHash) => Some(Hashed::ChangedDuringHash(item)),
                    Ok(HashOutcome::Cancelled) => None,
                    Err(e) => Some(Hashed::Failed(item, e)),
                },
            )
        },
        &mut |hashed| {
            match hashed {
                Hashed::Done {
                    item,
                    hash,
                    size,
                    mtime_ns,
                } => {
                    let mut batch = batch.borrow_mut();
                    batch.begin()?;
                    store(item, &hash, size, mtime_ns).map_err(|e| e.to_string())?;
                    batch.wrote()?;
                }
                Hashed::ChangedDuringHash(item) => {
                    sh.with(|s| s.changed_during_hash += 1);
                    sh.problem(
                        Path::new(&item.path),
                        "changed while it was being hashed; result discarded, try again later"
                            .into(),
                    );
                }
                Hashed::Failed(item, e) => sh.problem(Path::new(&item.path), e.to_string()),
            }
            sh.with(|s| s.files_done += 1);
            Ok(())
        },
        &mut || batch.borrow_mut().idle(sh),
    );
    // Cancelled, failed or complete: keep everything hashed so far.
    batch.into_inner().commit()?;
    result
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
        hash_items(
            sh,
            db,
            &items,
            Some(&LinkCache::new()),
            &mut |p, hash, size, mtime_ns| {
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
            },
        )
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
    hash_items(sh, db, &items, None, &mut |p, hash, size, mtime_ns| {
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
    hash_items(
        sh,
        db,
        &items,
        Some(&LinkCache::new()),
        &mut |p, hash, size, mtime_ns| {
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
        },
    )
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

/// What a worker found out about one entry.
struct Checked {
    id: i64,
    finding: Finding,
}

enum Finding {
    /// The entry was removed while the job ran.
    Gone,
    /// The file changed while being read; reported as a problem, record untouched.
    Undecided,
    /// Its drive is not connected; record untouched.
    Unavailable,
    Ok {
        size: u64,
        mtime_ns: i64,
    },
    Problem(Status, String),
}

/// Verify one entry on a worker thread: look, re-hash, classify. Writes
/// nothing; the result goes back to the thread that owns the database.
fn verify_one(
    sh: &Shared,
    ctx: &mut Ctx,
    db_path: &Path,
    id: i64,
    volumes: &Mutex<Volumes>,
) -> Result<Option<Checked>, String> {
    if ctx.db.is_none() {
        ctx.db = Some(Db::open(db_path).map_err(|e| e.to_string())?);
    }
    let db = ctx.db.as_ref().expect("just opened");
    let Some(entry) = db.entry(id).map_err(|e| e.to_string())? else {
        return Ok(Some(Checked {
            id,
            finding: Finding::Gone,
        }));
    };
    let path = Path::new(&entry.path);
    let skip_bytes = || {
        sh.bytes_done.fetch_add(entry.size, Ordering::Relaxed);
    };
    let missing = || Finding::Problem(Status::Missing, "the file no longer exists".into());

    let finding = match fsread::stat(path) {
        Err(err) => {
            skip_bytes();
            let mut vols = volumes.lock().unwrap();
            if !vols.is_available(&entry.path) {
                vols.note_skipped(&entry.path, 1);
                Finding::Unavailable
            } else if err.kind() == std::io::ErrorKind::NotFound {
                missing()
            } else {
                Finding::Problem(Status::Unreadable, err.to_string())
            }
        }
        Ok(meta) if meta.kind != Kind::File => {
            skip_bytes();
            Finding::Problem(
                Status::Unreadable,
                "this is no longer a regular file".into(),
            )
        }
        Ok(_) => match hash_one(sh, ctx, path, entry.algo, entry.size, None) {
            Ok(HashOutcome::Done {
                hash,
                size,
                mtime_ns,
                ..
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
                    Verdict::Ok => Finding::Ok { size, mtime_ns },
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
                        Finding::Problem(
                            Status::Changed,
                            format!("content differs; {}", what.join(", ")),
                        )
                    }
                    Verdict::PossiblyCorrupted => Finding::Problem(
                        Status::PossiblyCorrupted,
                        format!(
                            "content differs although size and modified time are unchanged; current {} {}",
                            entry.algo.name(),
                            crate::hasher::to_hex(&hash)
                        ),
                    ),
                }
            }
            Ok(HashOutcome::ChangedDuringHash) => {
                sh.with(|s| s.changed_during_hash += 1);
                sh.problem(
                    path,
                    "changed while it was being verified; result discarded, verify it again later"
                        .into(),
                );
                Finding::Undecided
            }
            Ok(HashOutcome::Cancelled) => return Ok(None),
            // The file was there a moment ago.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => missing(),
            // A read error on a file that used to be fine is itself a warning sign.
            Err(err) => Finding::Problem(Status::Unreadable, err.to_string()),
        },
    };
    Ok(Some(Checked { id, finding }))
}

fn run_verify(
    sh: &Shared,
    db: &mut Db,
    db_path: &Path,
    target: VerifyTarget,
) -> Result<(), String> {
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

    // 2. Re-hash every entry and compare, each drive with its own threads.
    let mut grouper = Grouper::new(db);
    for id in &ids {
        // Entries removed in the meantime are simply skipped.
        if let Some(path) = db.path_of(*id).map_err(e)? {
            grouper.push(&path, *id).map_err(e)?;
        }
    }
    let pools = grouper.finish();
    let direct = direct_io_setting(db).map_err(e)?;
    sh.with(|s| {
        s.phase = Phase::Hashing;
        s.hashing_started = Some(Instant::now());
        s.files_total = pools
            .iter()
            .map(|p| p.items.lock().unwrap().len() as u64)
            .sum();
        s.bytes_total = bytes_total;
    });
    let volumes = Mutex::new(volumes);
    let batch = std::cell::RefCell::new(batch);
    let result = run_pools(
        sh,
        &pools,
        direct,
        &|ctx, id: i64| verify_one(sh, ctx, db_path, id, &volumes),
        &mut |checked: Checked| {
            let now = now_unix();
            let mut batch = batch.borrow_mut();
            match checked.finding {
                Finding::Gone | Finding::Undecided => {}
                Finding::Unavailable => sh.with(|s| s.verify.unavailable += 1),
                Finding::Ok { size, mtime_ns } => {
                    batch.begin()?;
                    db.record_verified_ok(checked.id, size, mtime_ns, now, run)
                        .map_err(e)?;
                    batch.wrote()?;
                    sh.with(|s| s.verify.ok += 1);
                }
                Finding::Problem(status, detail) => {
                    batch.begin()?;
                    db.record_verified_problem(checked.id, status, &detail, now, run)
                        .map_err(e)?;
                    batch.wrote()?;
                    sh.with(|s| match status {
                        Status::Changed => s.verify.changed += 1,
                        Status::PossiblyCorrupted => s.verify.possibly_corrupted += 1,
                        Status::Missing => s.verify.missing += 1,
                        Status::Unreadable => s.verify.unreadable += 1,
                        Status::Ok => {}
                    });
                }
            }
            sh.with(|s| s.files_done += 1);
            Ok(())
        },
        &mut || batch.borrow_mut().idle(sh),
    );
    batch.into_inner().commit()?;
    result?;
    let volumes = volumes.into_inner().unwrap();
    let completed = !sh.cancelled();

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

fn run_check_database(sh: &Shared, db: &Db) -> Result<(), String> {
    sh.with(|s| s.phase = Phase::Writing);
    let sound = db.integrity_check().map_err(|e| e.to_string())?;
    if !sound {
        return Err(
            "the database FAILED its integrity check. Stop using it and restore a backup copy \
                    (hash-check.db.backup-...) or rebuild it by adding your folders again"
                .into(),
        );
    }
    let entries = db.count_files().map_err(|e| e.to_string())?;
    sh.with(|s| {
        s.message = Some(format!(
            "The database passed the full integrity check.\n{entries} entries, schema version {}.",
            crate::db::SCHEMA_VERSION
        ));
    });
    Ok(())
}

impl Job {
    /// One line for the log.
    fn describe(&self) -> String {
        match self {
            Job::Add {
                inputs,
                recursive,
                algo,
            } => format!(
                "Add ({}, {}): {}",
                algo.name(),
                if *recursive {
                    "with subfolders"
                } else {
                    "without subfolders"
                },
                inputs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Job::Update { ids } => format!("Update {} entries", ids.len()),
            Job::RemoveEntries { ids } => format!("Remove {} entries from the database", ids.len()),
            Job::RemoveRoot { root_id } => format!("Remove root #{root_id} from the database"),
            Job::Verify { target } => match target {
                VerifyTarget::All => "Verify everything".to_string(),
                VerifyTarget::Root(id) => format!("Verify root #{id}"),
                VerifyTarget::Entries(ids) => format!("Verify {} selected entries", ids.len()),
            },
            Job::AddNew { new_file_ids, algo } => {
                format!("Add {} new files ({})", new_file_ids.len(), algo.name())
            }
            Job::Export { run, .. } => format!("Export report of run {run}"),
            Job::Benchmark { inputs, .. } => format!(
                "Benchmark: {}",
                inputs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Job::CheckDatabase => "Check database".to_string(),
        }
    }
}

/// What a finished job writes to the log file.
fn log_text(what: &str, s: &State) -> String {
    let took = s
        .finished_at
        .map(|t| t.duration_since(s.started).as_secs_f64())
        .unwrap_or(0.0);
    let mut out = format!(
        "{}  {what}\n    {} after {:.1} s",
        crate::util::format_unix_secs(now_unix()),
        match (&s.fatal, s.cancelled) {
            (Some(_), _) => "FAILED",
            (None, true) => "cancelled",
            (None, false) => "finished",
        },
        took
    );
    let mut facts: Vec<String> = Vec::new();
    let mut fact = |n: u64, name: &str| {
        if n > 0 {
            facts.push(format!("{n} {name}"));
        }
    };
    fact(s.added, "added");
    fact(s.updated, "updated");
    fact(s.removed, "removed");
    fact(s.already_tracked, "already tracked");
    fact(s.hard_links_reused, "hard links not read twice");
    fact(s.skipped_links, "links not followed");
    fact(s.changed_during_hash, "changed while being read");
    if s.kind == JobKind::Verify {
        let v = &s.verify;
        facts.push(format!(
            "run {}: {} OK, {} changed, {} POSSIBLY CORRUPTED, {} missing, {} unreadable, {} new, {} on unavailable drives",
            s.run_id, v.ok, v.changed, v.possibly_corrupted, v.missing, v.unreadable, v.new, v.unavailable
        ));
    }
    if let Some(p) = &s.report_path {
        facts.push(format!("report {}", p.display()));
    }
    for r in &s.bench {
        facts.push(format!("{}: {:.0} MB/s", r.label, r.mb_per_s()));
    }
    if !facts.is_empty() {
        out += &format!(": {}", facts.join(", "));
    }
    out.push('\n');
    for plan in &s.plans {
        out += &format!("    drive {plan}\n");
    }
    if let Some(f) = &s.fatal {
        out += &format!("    error: {f}\n");
    }
    for p in &s.problems {
        out += &format!("    problem: {}: {}\n", p.path, p.message);
    }
    if s.problem_count as usize > s.problems.len() {
        out += &format!(
            "    ... and {} more problems\n",
            s.problem_count as usize - s.problems.len()
        );
    }
    out
}

/// Amount of in-memory data hashed per algorithm for the CPU-only figures.
const BENCH_MEMORY_BLOCK: usize = 64 * 1024 * 1024;
const BENCH_MEMORY_ROUNDS: usize = 16;

/// Measure how fast existing files can be read and hashed. Nothing is
/// stored in the database and nothing is written anywhere.
fn run_benchmark(sh: &Shared, db: &Db, inputs: &[PathBuf], limit_bytes: u64) -> Result<(), String> {
    // 1. Pick the files: in path order until the size limit is reached.
    let mut items: Vec<Item> = Vec::new();
    let mut total: u64 = 0;
    let full = |total: u64| limit_bytes > 0 && total >= limit_bytes;
    for input in inputs {
        if full(total) || !sh.checkpoint() {
            break;
        }
        let Ok(resolved) = fsread::resolve(input) else {
            sh.problem(input, "cannot be opened".into());
            continue;
        };
        match fsread::stat(&resolved).map(|m| (m.kind, m.size)) {
            Ok((Kind::File, size)) => {
                if let Some(p) = resolved.to_str() {
                    items.push(Item {
                        id: None,
                        path: p.to_string(),
                        size,
                        root_id: None,
                        algo: Algo::Blake3,
                    });
                    total += size;
                }
            }
            Ok((Kind::Dir, _)) => fsread::walk(&resolved, true, &mut |item| {
                if let WalkItem::File { path, meta } = item
                    && meta.size > 0
                    && let Some(p) = path.to_str()
                {
                    items.push(Item {
                        id: None,
                        path: p.to_string(),
                        size: meta.size,
                        root_id: None,
                        algo: Algo::Blake3,
                    });
                    total += meta.size;
                }
                !full(total) && !sh.cancelled()
            }),
            _ => sh.problem(&resolved, "not a regular file or folder".into()),
        }
    }
    if sh.cancelled() {
        return Ok(());
    }
    if items.is_empty() || total == 0 {
        return Err("no readable files were selected for the benchmark".into());
    }

    let disk_passes: [(&str, Algo, bool); 4] = [
        ("BLAKE3, direct reads", Algo::Blake3, true),
        ("XXH3-128, direct reads", Algo::Xxh3_128, true),
        ("SHA-256, direct reads", Algo::Sha256, true),
        (
            "BLAKE3, normal reads (may be served from the file cache)",
            Algo::Blake3,
            false,
        ),
    ];
    sh.with(|s| {
        s.phase = Phase::Hashing;
        s.hashing_started = Some(Instant::now());
        s.files_total = (items.len() * disk_passes.len()) as u64;
        s.bytes_total = total * disk_passes.len() as u64;
    });

    // 2. Read and hash the files once per algorithm.
    for (label, algo, direct) in disk_passes {
        if !sh.checkpoint() {
            return Ok(());
        }
        sh.with(|s| s.bench_step = label.to_string());
        let mut grouper = Grouper::new(db);
        for item in &items {
            grouper.push(&item.path, item).map_err(|e| e.to_string())?;
        }
        let pools = grouper.finish();
        let plans: Vec<String> = pools.iter().map(|p| p.plan.describe()).collect();
        let mut bytes = 0u64;
        let started = Instant::now();
        let paused_before = sh.snapshot().paused_total;
        run_pools(
            sh,
            &pools,
            direct,
            &|ctx, item: &Item| {
                Ok(
                    match hash_one(sh, ctx, Path::new(&item.path), algo, item.size, None) {
                        Ok(HashOutcome::Done { size, .. }) => Some(Ok(size)),
                        Ok(HashOutcome::ChangedDuringHash) => Some(Ok(0)),
                        Ok(HashOutcome::Cancelled) => None,
                        Err(e) => Some(Err((item.path.clone(), e.to_string()))),
                    },
                )
            },
            &mut |r: Result<u64, (String, String)>| {
                match r {
                    Ok(n) => bytes += n,
                    Err((path, e)) => sh.problem(Path::new(&path), e),
                }
                sh.with(|s| s.files_done += 1);
                Ok(())
            },
            &mut || Ok(()),
        )?;
        if sh.cancelled() {
            return Ok(());
        }
        let paused = sh.snapshot().paused_total.saturating_sub(paused_before);
        let secs = started.elapsed().saturating_sub(paused).as_secs_f64();
        sh.with(|s| {
            if s.bench_info.is_empty() {
                s.bench_info = format!(
                    "{} files, {} MB · {} · {} logical CPUs · hash-check {}",
                    items.len(),
                    total / 1_048_576,
                    plans.join("; "),
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(0),
                    crate::VERSION
                );
            }
            s.bench.push(BenchRow {
                label: label.to_string(),
                bytes,
                secs,
            });
        });
    }

    // 3. The same algorithms on data already in memory: what the CPU alone can do.
    let block: Vec<u8> = (0..BENCH_MEMORY_BLOCK)
        .map(|i| (i.wrapping_mul(31) ^ (i >> 7)) as u8)
        .collect();
    let memory_passes: [(&str, Algo, bool); 4] = [
        ("BLAKE3, in memory, all cores", Algo::Blake3, true),
        ("BLAKE3, in memory, 1 thread", Algo::Blake3, false),
        ("XXH3-128, in memory, 1 thread", Algo::Xxh3_128, false),
        ("SHA-256, in memory, 1 thread", Algo::Sha256, false),
    ];
    for (label, algo, parallel) in memory_passes {
        sh.with(|s| {
            s.bench_step = label.to_string();
            s.current.clear();
        });
        let mut hasher = crate::hasher::Hasher::new(algo);
        let started = Instant::now();
        let mut bytes = 0u64;
        for _ in 0..BENCH_MEMORY_ROUNDS {
            if sh.cancelled() {
                return Ok(());
            }
            hasher.update(&block, parallel);
            bytes += block.len() as u64;
        }
        let _ = hasher.finalize();
        let secs = started.elapsed().as_secs_f64();
        sh.with(|s| {
            s.bench.push(BenchRow {
                label: label.to_string(),
                bytes,
                secs,
            })
        });
    }
    sh.with(|s| s.bench_step.clear());
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
