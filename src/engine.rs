//! Background jobs. Nothing slow runs on the UI thread: a job owns a worker
//! thread and its own database connection, and publishes progress through
//! [`Shared`], which the UI polls.
//!
//! Results are committed in small batches, so a cancelled or crashed run
//! keeps everything completed up to the last batch.

use crate::db::{Db, DbResult, NewEntry};
use crate::fsread::{self, Kind, WalkItem};
use crate::hasher::Algo;
use crate::hashfile::{HashOutcome, hash_file};
use crate::util::now_unix;
use std::collections::HashSet;
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKind {
    Add,
    Update,
    Remove,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Finding files / loading entries.
    Preparing,
    Hashing,
    Removing,
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
            Job::Add { .. } => JobKind::Add,
            Job::Update { .. } => JobKind::Update,
            Job::RemoveEntries { .. } | Job::RemoveRoot { .. } => JobKind::Remove,
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
    let mut in_batch = 0usize;
    let mut batch_started = Instant::now();
    let mut open = false;
    for p in items {
        // Never sit paused while holding the database write lock.
        if sh.is_paused() && open {
            db.commit().map_err(|e| e.to_string())?;
            open = false;
            in_batch = 0;
        }
        if !sh.checkpoint() {
            break;
        }
        if !open {
            db.begin().map_err(|e| e.to_string())?;
            open = true;
            batch_started = Instant::now();
        }
        sh.with(|s| s.current.clone_from(&p.path));
        let path = Path::new(&p.path);
        let mut read: u64 = 0;
        let outcome = hash_file(path, p.algo, &mut buf, true, &mut |n| {
            read += n;
            sh.bytes_done.fetch_add(n, Ordering::Relaxed);
            sh.checkpoint()
        });
        let mut stop = false;
        match outcome {
            Ok(HashOutcome::Done {
                hash,
                size,
                mtime_ns,
            }) => {
                store(p, &hash, size, mtime_ns).map_err(|e| e.to_string())?;
                in_batch += 1;
            }
            Ok(HashOutcome::ChangedDuringHash) => {
                sh.with(|s| s.changed_during_hash += 1);
                sh.problem(
                    path,
                    "changed while it was being hashed; result discarded, try again later".into(),
                );
            }
            Ok(HashOutcome::Cancelled) => stop = true,
            Err(e) => sh.problem(path, e.to_string()),
        }
        if stop {
            break;
        }
        // Keep the byte counter aligned with the planned total even when a
        // file was shorter, longer or unreadable.
        if read < p.size {
            sh.bytes_done.fetch_add(p.size - read, Ordering::Relaxed);
        } else if read > p.size {
            sh.bytes_done.fetch_sub(read - p.size, Ordering::Relaxed);
        }
        sh.with(|s| s.files_done += 1);

        if in_batch >= BATCH_FILES || (in_batch > 0 && batch_started.elapsed() >= BATCH_TIME) {
            db.commit().map_err(|e| e.to_string())?;
            open = false;
            in_batch = 0;
        }
    }
    // Cancelled or complete: keep everything hashed so far.
    if open {
        db.commit().map_err(|e| e.to_string())?;
    }
    Ok(())
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
