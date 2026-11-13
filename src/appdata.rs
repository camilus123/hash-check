//! The application's own directory: the only place hash-check ever writes.
//!
//! Everything the program stores (database, backups, and later settings,
//! logs and reports) lives next to the executable. This is the only module
//! allowed to create or remove files, and every path it touches is built
//! from [`AppDir::dir`].

use std::fs;
use std::path::{Path, PathBuf};

pub const DB_FILE: &str = "hash-check.db";

#[derive(Clone, Debug)]
pub struct AppDir {
    dir: PathBuf,
}

impl AppDir {
    /// The directory containing the running executable.
    pub fn locate() -> Result<AppDir, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("Cannot determine the program's location: {e}"))?;
        let dir = exe
            .parent()
            .ok_or_else(|| "The program's location has no parent directory".to_string())?
            .to_path_buf();
        Ok(AppDir { dir })
    }

    /// Use an explicit directory (tests).
    pub fn at(dir: &Path) -> AppDir {
        AppDir {
            dir: dir.to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join(DB_FILE)
    }

    /// Confirm the directory is writable by creating and removing a probe
    /// file of our own. hash-check refuses to run otherwise, rather than
    /// quietly writing somewhere else.
    pub fn check_writable(&self) -> Result<(), String> {
        let probe = self
            .dir
            .join(format!(".hash-check-write-test-{}", std::process::id()));
        fs::write(&probe, b"hash-check write test")
            .and_then(|_| fs::remove_file(&probe))
            .map_err(|e| {
                format!(
                    "hash-check keeps its database next to the program, but this folder is not writable:\n\n{}\n\n{e}\n\nMove hash-check to a folder you can write to (not Program Files).",
                    self.dir.display()
                )
            })
    }
}
