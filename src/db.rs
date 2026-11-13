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
            "DELETE FROM roots WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM files WHERE root_id = ?1)",
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
                    last_result, last_error
             FROM files WHERE id = ?1",
        )?;
        let raw = st
            .query_row(params![id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<String>>(10)?,
                ))
            })
            .optional()?;
        let Some((
            id,
            path,
            root_id,
            size,
            mtime_ns,
            algo,
            hash,
            first_hashed,
            last_verified,
            res,
            last_error,
        )) = raw
        else {
            return Ok(None);
        };
        let algo = Algo::from_id(algo).ok_or_else(|| {
            DbError::BadData(format!("unknown hash algorithm id {algo} for {path}"))
        })?;
        let status = Status::from_id(res)
            .ok_or_else(|| DbError::BadData(format!("unknown result id {res} for {path}")))?;
        Ok(Some(Entry {
            id,
            path,
            root_id,
            size: size as u64,
            mtime_ns,
            algo,
            hash,
            first_hashed,
            last_verified,
            status,
            last_error,
        }))
    }

    /// Ids in path order, limited to `scope` and to paths containing
    /// `search` (case-insensitive for ASCII letters).
    pub fn ids_filtered(&self, scope: Scope, search: &str) -> DbResult<Vec<i64>> {
        let mut sql = String::from("SELECT id FROM files WHERE 1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        match scope {
            Scope::All => {}
            Scope::Root(id) => {
                sql += " AND root_id = ?";
                args.push(id.into());
            }
            Scope::Loose => sql += " AND root_id IS NULL",
        }
        if !search.is_empty() {
            let mut pat = String::with_capacity(search.len() + 2);
            pat.push('%');
            for c in search.chars() {
                if matches!(c, '%' | '_' | '\\') {
                    pat.push('\\');
                }
                pat.push(c);
            }
            pat.push('%');
            sql += " AND path LIKE ? ESCAPE '\\'";
            args.push(pat.into());
        }
        sql += " ORDER BY path";
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
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
