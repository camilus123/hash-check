//! SQLite storage.
//!
//! Safety properties:
//! - WAL journal with `synchronous=FULL`: every commit is durable and a
//!   killed process can never leave a half-written database; uncommitted
//!   work is simply rolled back on the next open.
//! - `PRAGMA user_version` holds the schema version. Before any migration
//!   of an existing database a consistent backup copy is written next to
//!   it (`<name>.backup-v<old>-<timestamp>`).
//! - A database written by a newer hash-check is refused, not guessed at.

use crate::hasher::Algo;
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::path::{Path, PathBuf};

/// Schema migrations; entry `i` upgrades version `i` to `i + 1`.
/// Append only. Never edit a released entry.
pub const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE roots (
         id         INTEGER PRIMARY KEY,
         path       TEXT    NOT NULL UNIQUE,
         recursive  INTEGER NOT NULL,
         added_at   INTEGER NOT NULL
     );
     CREATE TABLE files (
         id             INTEGER PRIMARY KEY,
         path           TEXT    NOT NULL UNIQUE,
         root_id        INTEGER REFERENCES roots(id),
         size           INTEGER NOT NULL,
         mtime_ns       INTEGER NOT NULL,
         algo           INTEGER NOT NULL,
         hash           BLOB    NOT NULL,
         first_hashed   INTEGER NOT NULL,
         last_verified  INTEGER NOT NULL,
         last_result    INTEGER NOT NULL,
         last_error     TEXT
     );
     CREATE INDEX files_root   ON files(root_id);
     CREATE INDEX files_result ON files(last_result);",
    // v2: verification runs, and files found under a root that are not tracked yet.
    "ALTER TABLE files ADD COLUMN last_run INTEGER NOT NULL DEFAULT 0;
     CREATE INDEX files_run ON files(last_run);
     CREATE TABLE new_files (
         id        INTEGER PRIMARY KEY,
         path      TEXT    NOT NULL UNIQUE,
         root_id   INTEGER NOT NULL REFERENCES roots(id),
         size      INTEGER NOT NULL,
         mtime_ns  INTEGER NOT NULL,
         found_at  INTEGER NOT NULL
     );
     CREATE INDEX new_files_root ON new_files(root_id);
     CREATE TABLE meta (
         key    TEXT PRIMARY KEY,
         value  TEXT NOT NULL
     );",
];

pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
    NewerSchema { found: i64, supported: i64 },
    BadData(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Sqlite(e) => write!(f, "database error: {e}"),
            DbError::NewerSchema { found, supported } => write!(
                f,
                "the database was written by a newer hash-check (schema {found}, this version supports up to {supported}); it has not been touched"
            ),
            DbError::BadData(s) => write!(f, "unexpected data in database: {s}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sqlite(e)
    }
}

pub type DbResult<T> = Result<T, DbError>;

/// Stored outcome of the last verification of an entry.
/// Numeric ids are stored in the database. Never renumber.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Status {
    Ok,
    Changed,
    PossiblyCorrupted,
    Missing,
    Unreadable,
}

impl Status {
    pub const ALL: [Status; 5] = [
        Status::Ok,
        Status::Changed,
        Status::PossiblyCorrupted,
        Status::Missing,
        Status::Unreadable,
    ];

    pub fn id(self) -> i64 {
        match self {
            Status::Ok => 1,
            Status::Changed => 2,
            Status::PossiblyCorrupted => 3,
            Status::Missing => 4,
            Status::Unreadable => 5,
        }
    }

    pub fn from_id(id: i64) -> Option<Status> {
        Status::ALL.into_iter().find(|s| s.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Changed => "Changed",
            Status::PossiblyCorrupted => "POSSIBLY CORRUPTED",
            Status::Missing => "Missing",
            Status::Unreadable => "Unreadable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub path: String,
    pub root_id: Option<i64>,
    pub size: u64,
    pub mtime_ns: i64,
    pub algo: Algo,
    pub hash: Vec<u8>,
    pub first_hashed: i64,
    pub last_verified: i64,
    pub status: Status,
    pub last_error: Option<String>,
    /// Id of the verification run that last looked at this entry (0 = never).
    pub last_run: i64,
}

/// A file found under a tracked root that is not in the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewFile {
    pub id: i64,
    pub path: String,
    pub root_id: i64,
    pub size: u64,
    pub mtime_ns: i64,
    pub found_at: i64,
}

/// Which entries a list shows, beyond the [`Scope`].
#[derive(Clone, Debug, Default)]
pub struct Filter<'a> {
    pub scope: Option<Scope>,
    /// Part of the path (case-insensitive for ASCII letters).
    pub search: &'a str,
    pub status: Option<Status>,
    /// Only entries looked at by this verification run.
    pub run: Option<i64>,
}

/// Entries per status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatusCounts {
    pub ok: u64,
    pub changed: u64,
    pub possibly_corrupted: u64,
    pub missing: u64,
    pub unreadable: u64,
}

impl StatusCounts {
    pub fn get(&self, s: Status) -> u64 {
        match s {
            Status::Ok => self.ok,
            Status::Changed => self.changed,
            Status::PossiblyCorrupted => self.possibly_corrupted,
            Status::Missing => self.missing,
            Status::Unreadable => self.unreadable,
        }
    }

    pub fn total(&self) -> u64 {
        self.ok + self.changed + self.possibly_corrupted + self.missing + self.unreadable
    }
}

#[derive(Clone, Debug)]
pub struct NewEntry<'a> {
    pub path: &'a str,
    pub root_id: Option<i64>,
    pub size: u64,
    pub mtime_ns: i64,
    pub algo: Algo,
    pub hash: &'a [u8],
    pub now: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub recursive: bool,
    pub added_at: i64,
}

/// Which entries a list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Root(i64),
    /// Files added individually, not through a folder.
    Loose,
}

/// Bound parameters per statement when working through long id lists.
const ID_CHUNK: usize = 500;

pub struct Db {
    conn: Connection,
    /// Set when opening had to migrate an existing database.
    pub backup_made: Option<PathBuf>,
}

impl Db {
    pub fn open(path: &Path) -> DbResult<Db> {
        Db::open_with(path, MIGRATIONS)
    }

    /// Open with an explicit migration list (lets tests exercise upgrades).
    pub fn open_with(path: &Path, migrations: &[&str]) -> DbResult<Db> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let supported = migrations.len() as i64;
        let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found > supported {
            return Err(DbError::NewerSchema { found, supported });
        }
        let mut backup_made = None;
        if found < supported {
            if found > 0 {
                backup_made = Some(backup_before_migration(&conn, path, found)?);
            }
            for (i, sql) in migrations.iter().enumerate().skip(found as usize) {
                // Schema change and version bump commit together or not at all.
                let tx = conn.transaction()?;
                tx.execute_batch(sql)?;
                tx.pragma_update(None, "user_version", (i + 1) as i64)?;
                tx.commit()?;
            }
        }
        Ok(Db { conn, backup_made })
    }

    pub fn schema_version(&self) -> DbResult<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Full structural check of the database file.
    pub fn integrity_check(&self) -> DbResult<bool> {
        let r: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(r == "ok")
    }

    // ---- transactions (batched writes from the job thread) ----

    pub fn begin(&self) -> DbResult<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub fn commit(&self) -> DbResult<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(&self) -> DbResult<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    // ---- roots ----

    /// Remember a directory root. Returns its id and whether this call
    /// created it (`false` if it was already tracked).
    pub fn add_root(&self, path: &str, recursive: bool, now: i64) -> DbResult<(i64, bool)> {
        let created = self.conn.execute(
            "INSERT OR IGNORE INTO roots(path, recursive, added_at) VALUES (?1, ?2, ?3)",
            params![path, recursive, now],
        )? == 1;
        // A root first added without subfolders becomes recursive if re-added as a tree.
        if recursive && !created {
            self.conn.execute(
                "UPDATE roots SET recursive = 1 WHERE path = ?1",
                params![path],
            )?;
        }
        let id =
            self.conn
                .query_row("SELECT id FROM roots WHERE path = ?1", params![path], |r| {
                    r.get(0)
                })?;
        Ok((id, created))
    }

    /// Forget a root only if it has no entries. Returns whether it was removed.
    pub fn delete_root_if_empty(&self, root_id: i64) -> DbResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM roots WHERE id = ?1
               AND NOT EXISTS (SELECT 1 FROM files WHERE root_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM new_files WHERE root_id = ?1)",
            params![root_id],
        )?;
        Ok(n == 1)
    }

    pub fn roots(&self) -> DbResult<Vec<Root>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id, path, recursive, added_at FROM roots ORDER BY path")?;
        let rows = st.query_map([], |r| {
            Ok(Root {
                id: r.get(0)?,
                path: r.get(1)?,
                recursive: r.get(2)?,
                added_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- files ----

    pub fn file_exists(&self, path: &str) -> DbResult<bool> {
        let mut st = self
            .conn
            .prepare_cached("SELECT 1 FROM files WHERE path = ?1")?;
        Ok(st
            .query_row(params![path], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Insert a freshly hashed file. Returns `false` (and changes nothing)
    /// if the path is already tracked: adding never overwrites a stored hash.
    pub fn insert_entry(&self, e: &NewEntry<'_>) -> DbResult<bool> {
        let size =
            i64::try_from(e.size).map_err(|_| DbError::BadData("file size too large".into()))?;
        let mut st = self.conn.prepare_cached(
            "INSERT OR IGNORE INTO files
                 (path, root_id, size, mtime_ns, algo, hash, first_hashed, last_verified, last_result)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        )?;
        let n = st.execute(params![
            e.path,
            e.root_id,
            size,
            e.mtime_ns,
            e.algo.id(),
            e.hash,
            e.now,
            Status::Ok.id()
        ])?;
        if n == 1 {
            // A tracked file is no longer "new".
            self.conn
                .prepare_cached("DELETE FROM new_files WHERE path = ?1")?
                .execute(params![e.path])?;
        }
        Ok(n == 1)
    }

    pub fn count_files(&self) -> DbResult<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    pub fn total_size(&self) -> DbResult<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM files", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Ids of all entries in path order. Eight bytes per entry, so the UI
    /// can hold this for millions of files and load rows only when visible.
    pub fn all_ids(&self) -> DbResult<Vec<i64>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id FROM files ORDER BY path")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn entry(&self, id: i64) -> DbResult<Option<Entry>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, path, root_id, size, mtime_ns, algo, hash, first_hashed, last_verified,
                    last_result, last_error, last_run
             FROM files WHERE id = ?1",
        )?;
        let mut rows = st.query(params![id])?;
        let Some(r) = rows.next()? else {
            return Ok(None);
        };
        let path: String = r.get(1)?;
        let algo_id: i64 = r.get(5)?;
        let res: i64 = r.get(9)?;
        let algo = Algo::from_id(algo_id).ok_or_else(|| {
            DbError::BadData(format!("unknown hash algorithm id {algo_id} for {path}"))
        })?;
        let status = Status::from_id(res)
            .ok_or_else(|| DbError::BadData(format!("unknown result id {res} for {path}")))?;
        Ok(Some(Entry {
            id: r.get(0)?,
            path,
            root_id: r.get(2)?,
            size: r.get::<_, i64>(3)? as u64,
            mtime_ns: r.get(4)?,
            algo,
            hash: r.get(6)?,
            first_hashed: r.get(7)?,
            last_verified: r.get(8)?,
            status,
            last_error: r.get(10)?,
            last_run: r.get(11)?,
        }))
    }

    /// Ids in path order, limited to `scope` and to paths containing
    /// `search` (case-insensitive for ASCII letters).
    pub fn ids_filtered(&self, scope: Scope, search: &str) -> DbResult<Vec<i64>> {
        self.ids_matching(&Filter {
            scope: Some(scope),
            search,
            ..Default::default()
        })
    }

    /// Ids in path order of the entries matching `filter`.
    pub fn ids_matching(&self, filter: &Filter<'_>) -> DbResult<Vec<i64>> {
        let mut sql = String::from("SELECT id FROM files WHERE 1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        scope_clause(filter.scope, &mut sql, &mut args);
        search_clause(filter.search, &mut sql, &mut args);
        if let Some(status) = filter.status {
            sql += " AND last_result = ?";
            args.push(status.id().into());
        }
        if let Some(run) = filter.run {
            sql += " AND last_run = ?";
            args.push(run.into());
        }
        sql += " ORDER BY path";
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Entries per status within `scope`, optionally only those looked at by `run`.
    pub fn status_counts(&self, scope: Option<Scope>, run: Option<i64>) -> DbResult<StatusCounts> {
        let mut sql = String::from("SELECT last_result, COUNT(*) FROM files WHERE 1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        scope_clause(scope, &mut sql, &mut args);
        if let Some(run) = run {
            sql += " AND last_run = ?";
            args.push(run.into());
        }
        sql += " GROUP BY last_result";
        let mut st = self.conn.prepare(&sql)?;
        let mut rows = st.query(rusqlite::params_from_iter(args))?;
        let mut c = StatusCounts::default();
        while let Some(r) = rows.next()? {
            let n = r.get::<_, i64>(1)? as u64;
            match Status::from_id(r.get(0)?) {
                Some(Status::Ok) => c.ok = n,
                Some(Status::Changed) => c.changed = n,
                Some(Status::PossiblyCorrupted) => c.possibly_corrupted = n,
                Some(Status::Missing) => c.missing = n,
                Some(Status::Unreadable) => c.unreadable = n,
                None => {}
            }
        }
        Ok(c)
    }

    /// Total stored size of the entries in `scope`.
    pub fn size_in_scope(&self, scope: Scope) -> DbResult<u64> {
        let mut sql = String::from("SELECT COALESCE(SUM(size), 0) FROM files WHERE 1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        scope_clause(Some(scope), &mut sql, &mut args);
        let n: i64 = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(args), |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Total stored size of the given entries.
    pub fn size_of(&self, ids: &[i64]) -> DbResult<u64> {
        let mut total = 0u64;
        for chunk in ids.chunks(ID_CHUNK) {
            let sql = format!(
                "SELECT COALESCE(SUM(size), 0) FROM files WHERE id IN ({})",
                placeholders(chunk.len())
            );
            let n: i64 = self
                .conn
                .query_row(&sql, rusqlite::params_from_iter(chunk), |r| r.get(0))?;
            total += n as u64;
        }
        Ok(total)
    }

    // ---- verification ----

    /// Start a new verification run and return its id (1, 2, 3, ...).
    pub fn begin_run(&mut self) -> DbResult<i64> {
        let tx = self.conn.transaction()?;
        let last: i64 = tx
            .query_row("SELECT value FROM meta WHERE key = 'last_run'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let run = last + 1;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('last_run', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![run.to_string()],
        )?;
        tx.commit()?;
        Ok(run)
    }

    /// Id of the most recent verification run (0 if there was none).
    pub fn last_run(&self) -> DbResult<i64> {
        Ok(self
            .meta("last_run")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    pub fn meta(&self, key: &str) -> DbResult<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> DbResult<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Verification found the file intact. Size and modified time are
    /// refreshed (a file can be touched without its content changing); the
    /// stored hash is, as always, left alone.
    pub fn record_verified_ok(
        &self,
        id: i64,
        size: u64,
        mtime_ns: i64,
        now: i64,
        run: i64,
    ) -> DbResult<bool> {
        let size =
            i64::try_from(size).map_err(|_| DbError::BadData("file size too large".into()))?;
        let mut st = self.conn.prepare_cached(
            "UPDATE files SET size = ?2, mtime_ns = ?3, last_verified = ?4, last_result = ?5,
                              last_error = NULL, last_run = ?6
             WHERE id = ?1",
        )?;
        Ok(st.execute(params![id, size, mtime_ns, now, Status::Ok.id(), run])? == 1)
    }

    /// Verification found a problem. Nothing but the result is stored: the
    /// hash, size and modified time on record stay as they were, so the
    /// same check gives the same answer next time.
    pub fn record_verified_problem(
        &self,
        id: i64,
        status: Status,
        detail: &str,
        now: i64,
        run: i64,
    ) -> DbResult<bool> {
        let mut st = self.conn.prepare_cached(
            "UPDATE files SET last_result = ?2, last_error = ?3, last_verified = ?4, last_run = ?5
             WHERE id = ?1",
        )?;
        Ok(st.execute(params![id, status.id(), detail, now, run])? == 1)
    }

    // ---- new (untracked) files ----

    /// Forget earlier findings for a root before it is scanned again.
    pub fn clear_new_files(&self, root_id: i64) -> DbResult<()> {
        self.conn
            .execute("DELETE FROM new_files WHERE root_id = ?1", params![root_id])?;
        Ok(())
    }

    pub fn insert_new_file(
        &self,
        path: &str,
        root_id: i64,
        size: u64,
        mtime_ns: i64,
        now: i64,
    ) -> DbResult<()> {
        let size =
            i64::try_from(size).map_err(|_| DbError::BadData("file size too large".into()))?;
        let mut st = self.conn.prepare_cached(
            "INSERT OR IGNORE INTO new_files(path, root_id, size, mtime_ns, found_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        st.execute(params![path, root_id, size, mtime_ns, now])?;
        Ok(())
    }

    /// Ids of new files in path order, limited to a root and a search text.
    pub fn new_file_ids(&self, scope: Scope, search: &str) -> DbResult<Vec<i64>> {
        let mut sql = String::from("SELECT id FROM new_files WHERE 1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        match scope {
            Scope::All => {}
            Scope::Root(id) => {
                sql += " AND root_id = ?";
                args.push(id.into());
            }
            // New files are only ever found under roots.
            Scope::Loose => sql += " AND 0",
        }
        search_clause(search, &mut sql, &mut args);
        sql += " ORDER BY path";
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn new_file(&self, id: i64) -> DbResult<Option<NewFile>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, path, root_id, size, mtime_ns, found_at FROM new_files WHERE id = ?1",
        )?;
        Ok(st
            .query_row(params![id], |r| {
                Ok(NewFile {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    root_id: r.get(2)?,
                    size: r.get::<_, i64>(3)? as u64,
                    mtime_ns: r.get(4)?,
                    found_at: r.get(5)?,
                })
            })
            .optional()?)
    }

    pub fn count_new_files(&self, scope: Scope) -> DbResult<u64> {
        let n: i64 = match scope {
            Scope::All => self
                .conn
                .query_row("SELECT COUNT(*) FROM new_files", [], |r| r.get(0))?,
            Scope::Root(id) => self.conn.query_row(
                "SELECT COUNT(*) FROM new_files WHERE root_id = ?1",
                params![id],
                |r| r.get(0),
            )?,
            Scope::Loose => 0,
        };
        Ok(n as u64)
    }

    pub fn delete_new_file(&self, id: i64) -> DbResult<()> {
        self.conn
            .prepare_cached("DELETE FROM new_files WHERE id = ?1")?
            .execute(params![id])?;
        Ok(())
    }

    /// Number of entries per root, and the number of loose entries.
    pub fn root_counts(&self) -> DbResult<(std::collections::HashMap<i64, u64>, u64)> {
        let mut st = self
            .conn
            .prepare_cached("SELECT root_id, COUNT(*) FROM files GROUP BY root_id")?;
        let mut per_root = std::collections::HashMap::new();
        let mut loose = 0u64;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let n = r.get::<_, i64>(1)? as u64;
            match r.get::<_, Option<i64>>(0)? {
                Some(id) => {
                    per_root.insert(id, n);
                }
                None => loose = n,
            }
        }
        Ok((per_root, loose))
    }

    /// Delete database entries (never files). Call inside a transaction
    /// for long lists. Returns how many rows were removed.
    pub fn delete_entries(&self, ids: &[i64]) -> DbResult<u64> {
        let mut removed = 0u64;
        for chunk in ids.chunks(ID_CHUNK) {
            let sql = format!(
                "DELETE FROM files WHERE id IN ({})",
                placeholders(chunk.len())
            );
            removed += self.conn.execute(&sql, rusqlite::params_from_iter(chunk))? as u64;
        }
        Ok(removed)
    }

    /// Forget a root and every entry that belongs to it, atomically.
    /// Nothing on disk is touched. Returns the number of entries removed.
    pub fn delete_root(&mut self, root_id: i64) -> DbResult<u64> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM files WHERE root_id = ?1", params![root_id])? as u64;
        tx.execute("DELETE FROM new_files WHERE root_id = ?1", params![root_id])?;
        tx.execute("DELETE FROM roots WHERE id = ?1", params![root_id])?;
        tx.commit()?;
        Ok(n)
    }

    /// "Update": replace the stored hash of an entry with a fresh one. The
    /// entry keeps its algorithm and its first-hashed time, and becomes OK.
    pub fn replace_hash(
        &self,
        id: i64,
        size: u64,
        mtime_ns: i64,
        hash: &[u8],
        now: i64,
    ) -> DbResult<bool> {
        let size =
            i64::try_from(size).map_err(|_| DbError::BadData("file size too large".into()))?;
        let mut st = self.conn.prepare_cached(
            "UPDATE files SET size = ?2, mtime_ns = ?3, hash = ?4, last_verified = ?5,
                              last_result = ?6, last_error = NULL
             WHERE id = ?1",
        )?;
        Ok(st.execute(params![id, size, mtime_ns, hash, now, Status::Ok.id()])? == 1)
    }

    /// Store the outcome of a verification. The stored hash is not changed.
    pub fn record_result(
        &self,
        id: i64,
        status: Status,
        error: Option<&str>,
        now: i64,
    ) -> DbResult<bool> {
        let mut st = self.conn.prepare_cached(
            "UPDATE files SET last_result = ?2, last_error = ?3, last_verified = ?4 WHERE id = ?1",
        )?;
        Ok(st.execute(params![id, status.id(), error, now])? == 1)
    }

    /// How many of `ids` currently have `status` as their last result.
    pub fn count_with_status(&self, ids: &[i64], status: Status) -> DbResult<u64> {
        let mut total = 0u64;
        for chunk in ids.chunks(ID_CHUNK) {
            let sql = format!(
                "SELECT COUNT(*) FROM files WHERE last_result = {} AND id IN ({})",
                status.id(),
                placeholders(chunk.len())
            );
            let n: i64 = self
                .conn
                .query_row(&sql, rusqlite::params_from_iter(chunk), |r| r.get(0))?;
            total += n as u64;
        }
        Ok(total)
    }
}

fn scope_clause(scope: Option<Scope>, sql: &mut String, args: &mut Vec<rusqlite::types::Value>) {
    match scope {
        None | Some(Scope::All) => {}
        Some(Scope::Root(id)) => {
            *sql += " AND root_id = ?";
            args.push(id.into());
        }
        Some(Scope::Loose) => *sql += " AND root_id IS NULL",
    }
}

/// Add a "path contains" condition, taking LIKE wildcards literally.
fn search_clause(search: &str, sql: &mut String, args: &mut Vec<rusqlite::types::Value>) {
    if search.is_empty() {
        return;
    }
    let mut pat = String::with_capacity(search.len() + 2);
    pat.push('%');
    for c in search.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pat.push('\\');
        }
        pat.push(c);
    }
    pat.push('%');
    *sql += " AND path LIKE ? ESCAPE '\\'";
    args.push(pat.into());
}

fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

/// Write a consistent copy of the database before changing its schema.
fn backup_before_migration(conn: &Connection, path: &Path, old_version: i64) -> DbResult<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hash-check.db".into());
    let mut n = 0;
    let target = loop {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let candidate =
            path.with_file_name(format!("{name}.backup-v{old_version}-{stamp}{suffix}"));
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    };
    let target_str = target
        .to_str()
        .ok_or_else(|| DbError::BadData("backup path is not valid Unicode".into()))?;
    // VACUUM INTO produces a complete, defragmented, transactionally
    // consistent copy, including anything still in the WAL.
    conn.execute("VACUUM INTO ?1", params![target_str])?;
    Ok(target)
}
