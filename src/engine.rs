//! Background jobs. Hashing never runs on the UI thread: a job owns a worker
//! thread and its own database connection, and publishes progress through
//! [`Shared`], which the UI polls.
//!
//! Results are committed in small batches, so a cancelled or crashed run
//! keeps everything completed up to the last batch.

use crate::db::{Db, NewEntry};
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Scanning,
    Hashing,
    Finished,
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct State {
    pub phase: Phase,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_total: u64,
    pub current: String,
    /// Entries stored by this job.
    pub added: u64,
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
}

pub struct Shared {
    pub cancel: AtomicBool,
    pub bytes_done: AtomicU64,
    pub state: Mutex<State>,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            cancel: AtomicBool::new(false),
            bytes_done: AtomicU64::new(0),
            state: Mutex::new(State {
                phase: Phase::Scanning,
                files_done: 0,
                files_total: 0,
                bytes_total: 0,
                current: String::new(),
                added: 0,
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
            }),
        }
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    fn problem(&self, path: &Path, message: String) {
        self.with(|s| {
            s.problem_count += 1;
            if s.problems.len() < MAX_LISTED_PROBLEMS {
                s.problems.push(Problem { path: path.display().to_string(), message });
            }
        });
    }
}

struct Pending {
    path: String,
    size: u64,
    root_id: Option<i64>,
}

/// Start hashing `inputs` (files and/or directories) into the database at
/// `db_path`. Directories are remembered as roots. `wake` is called when the
/// job finishes so the UI can repaint.
pub fn start_add(
    db_path: PathBuf,
    inputs: Vec<PathBuf>,
    recursive: bool,
    algo: Algo,
    wake: Box<dyn Fn() + Send>,
) -> Arc<Shared> {
    let shared = Arc::new(Shared::new());
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("hash-check-job".into())
        .spawn(move || {
            if let Err(msg) = run_add(&sh, &db_path, &inputs, recursive, algo) {
                sh.with(|s| s.fatal = Some(msg));
            }
            sh.with(|s| {
                s.cancelled = sh.cancelled();
                s.phase = Phase::Finished;
                s.current.clear();
                s.finished_at = Some(Instant::now());
            });
            wake();
        })
        .expect("failed to start the worker thread");
    shared
}

/// Blocking version of [`start_add`] for tests.
pub fn add_blocking(db_path: &Path, inputs: &[PathBuf], recursive: bool, algo: Algo) -> State {
    let shared = Shared::new();
    if let Err(msg) = run_add(&shared, db_path, inputs, recursive, algo) {
        shared.with(|s| s.fatal = Some(msg));
    }
    shared.with(|s| {
        s.phase = Phase::Finished;
        s.finished_at = Some(Instant::now());
    });
    shared.snapshot()
}

fn run_add(sh: &Shared, db_path: &Path, inputs: &[PathBuf], recursive: bool, algo: Algo) -> Result<(), String> {
    let db = Db::open(db_path).map_err(|e| e.to_string())?;
    let pending = scan(sh, &db, inputs, recursive)?;
    if sh.cancelled() {
        return Ok(());
    }
    sh.with(|s| {
        s.phase = Phase::Hashing;
        s.hashing_started = Some(Instant::now());
        s.files_total = pending.len() as u64;
        s.bytes_total = pending.iter().map(|p| p.size).sum();
    });

    let mut buf = Vec::new();
    let mut in_batch = 0usize;
    let mut batch_started = Instant::now();
    db.begin().map_err(|e| e.to_string())?;
    for p in &pending {
        if sh.cancelled() {
            break;
        }
        sh.with(|s| s.current.clone_from(&p.path));
        let path = Path::new(&p.path);
        let mut read: u64 = 0;
        let outcome = hash_file(path, algo, &mut buf, true, &mut |n| {
            read += n;
            sh.bytes_done.fetch_add(n, Ordering::Relaxed);
            !sh.cancelled()
        });
        match outcome {
            Ok(HashOutcome::Done { hash, size, mtime_ns }) => {
                let inserted = db
                    .insert_entry(&NewEntry {
                        path: &p.path,
                        root_id: p.root_id,
                        size,
                        mtime_ns,
                        algo,
                        hash: &hash,
                        now: now_unix(),
                    })
                    .map_err(|e| e.to_string())?;
                sh.with(|s| if inserted { s.added += 1 } else { s.already_tracked += 1 });
                in_batch += 1;
            }
            Ok(HashOutcome::ChangedDuringHash) => {
                sh.with(|s| s.changed_during_hash += 1);
                sh.problem(path, "changed while it was being hashed; result discarded, add it again later".into());
            }
            Ok(HashOutcome::Cancelled) => break,
            Err(e) => sh.problem(path, e.to_string()),
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
            db.begin().map_err(|e| e.to_string())?;
            in_batch = 0;
            batch_started = Instant::now();
        }
    }
    // Cancelled or complete: keep everything hashed so far.
    db.commit().map_err(|e| e.to_string())?;
    Ok(())
}

struct Scanner<'a> {
    sh: &'a Shared,
    db: &'a Db,
    pending: Vec<Pending>,
    seen: HashSet<String>,
    db_err: Option<String>,
    last_publish: Instant,
}

impl Scanner<'_> {
    fn consider(&mut self, path: &Path, size: u64, root_id: Option<i64>) {
        let Some(s) = path.to_str() else {
            self.sh.problem(path, "file name is not valid Unicode; cannot be tracked".into());
            return;
        };
        if self.seen.contains(s) {
            return;
        }
        match self.db.file_exists(s) {
            Ok(true) => self.sh.with(|st| st.already_tracked += 1),
            Ok(false) => {
                self.seen.insert(s.to_string());
                self.pending.push(Pending { path: s.to_string(), size, root_id });
            }
            Err(e) => self.db_err = Some(e.to_string()),
        }
        if self.last_publish.elapsed() >= Duration::from_millis(100) {
            self.last_publish = Instant::now();
            let n = self.pending.len() as u64;
            self.sh.with(|st| st.files_total = n);
        }
    }
}

/// Enumerate the files to hash, registering directory roots on the way.
fn scan(sh: &Shared, db: &Db, inputs: &[PathBuf], recursive: bool) -> Result<Vec<Pending>, String> {
    let mut sc = Scanner {
        sh,
        db,
        pending: Vec::new(),
        seen: HashSet::new(),
        db_err: None,
        last_publish: Instant::now(),
    };
    for input in inputs {
        if sh.cancelled() {
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
                    sh.problem(&resolved, "folder name is not valid Unicode; cannot be tracked".into());
                    continue;
                };
                let root_id = db.add_root(root_str, recursive, now_unix()).map_err(|e| e.to_string())?;
                fsread::walk(&resolved, recursive, &mut |item| {
                    match item {
                        WalkItem::File { path, meta } => sc.consider(path, meta.size, Some(root_id)),
                        WalkItem::SkippedLink { .. } => sh.with(|s| s.skipped_links += 1),
                        WalkItem::Error { path, error } => sh.problem(path, error.to_string()),
                    }
                    sc.db_err.is_none() && !sh.cancelled()
                });
            }
            Kind::Link | Kind::Other => sh.problem(&resolved, "not a regular file or folder".into()),
        }
        if let Some(e) = sc.db_err.take() {
            return Err(e);
        }
    }
    Ok(sc.pending)
}
