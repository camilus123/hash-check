//! Read-only access to user files.
//!
//! THE ONE ABSOLUTE RULE: hash-check never modifies, renames, moves or
//! deletes a user file. This module is the only place in the program that
//! opens or lists user files, and it exposes nothing that can change them:
//! [`ReadOnlyFile`] wraps the OS handle and offers only `read` and `meta`.
//!
//! `tests/readonly_audit.rs` enforces this mechanically: it fails the build
//! if this file contains any call that can alter the file system, or if any
//! other module (except `appdata`, which is confined to the app's own
//! directory) touches file-system APIs at all.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The metadata hash-check cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch (may be negative).
    pub mtime_ns: i64,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Symbolic link, junction or other reparse point that redirects elsewhere.
    Link,
    Other,
}

fn mtime_ns(t: io::Result<SystemTime>) -> i64 {
    match t {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            Err(e) => i64::try_from(e.duration().as_nanos())
                .map(|n| -n)
                .unwrap_or(i64::MIN),
        },
        Err(_) => 0,
    }
}

fn meta_from(m: &fs::Metadata) -> FileMeta {
    let ft = m.file_type();
    let kind = if ft.is_symlink() {
        Kind::Link
    } else if ft.is_file() {
        Kind::File
    } else if ft.is_dir() {
        Kind::Dir
    } else {
        Kind::Other
    };
    FileMeta {
        size: m.len(),
        mtime_ns: mtime_ns(m.modified()),
        kind,
    }
}

/// Metadata of `path` itself; links are reported as links, not followed.
pub fn stat(path: &Path) -> io::Result<FileMeta> {
    fs::symlink_metadata(path).map(|m| meta_from(&m))
}

/// Absolute path with the on-disk spelling, in the form users expect
/// (`C:\dir\file`, not `\\?\C:\dir\file`). Used only for paths the user
/// picked explicitly. Long paths keep working: the standard library adds
/// the long-path prefix itself whenever it opens an absolute path.
pub fn resolve(path: &Path) -> io::Result<PathBuf> {
    let p = fs::canonicalize(path)?;
    Ok(strip_verbatim(p))
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    if cfg!(windows) {
        if let Some(s) = p.to_str() {
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                // Only plain drive paths are safe to shorten.
                let b = rest.as_bytes();
                if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
                    return PathBuf::from(rest);
                }
            }
        }
    }
    p
}

/// A user file opened for reading only.
pub struct ReadOnlyFile {
    f: fs::File,
}

/// Open `path` read-only. On Windows the handle shares read, write and
/// delete access, so hash-check never blocks another program from using
/// the file, and it hints the cache manager that reads are sequential.
pub fn open(path: &Path) -> io::Result<ReadOnlyFile> {
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        o.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        o.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    Ok(ReadOnlyFile { f: o.open(path)? })
}

impl ReadOnlyFile {
    /// Metadata of the open handle.
    pub fn meta(&self) -> io::Result<FileMeta> {
        self.f.metadata().map(|m| meta_from(&m))
    }

    /// Read up to `buf.len()` bytes; 0 means end of file.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.f.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }
}

/// Read a whole file into memory, refusing anything larger than `max`
/// bytes. Used for system fonts, never for files being hashed.
pub fn read_whole(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut f = open(path)?;
    let size = f.meta()?.size;
    if size > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    let mut data = vec![0u8; size as usize];
    let mut filled = 0;
    while filled < data.len() {
        let n = f.read(&mut data[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    data.truncate(filled);
    Ok(data)
}

/// What [`walk`] reports.
pub enum WalkItem<'a> {
    File {
        path: &'a Path,
        meta: FileMeta,
    },
    /// A link/junction that was not followed.
    SkippedLink {
        path: &'a Path,
    },
    /// A directory or entry that could not be read.
    Error {
        path: &'a Path,
        error: io::Error,
    },
}

/// List the regular files in `dir`, descending into subdirectories when
/// `recursive`. Links and junctions are never followed. `visit` returns
/// `false` to stop early.
pub fn walk(dir: &Path, recursive: bool, visit: &mut dyn FnMut(WalkItem<'_>) -> bool) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(error) => {
                if !visit(WalkItem::Error { path: &d, error }) {
                    return;
                }
                continue;
            }
        };
        let mut subdirs = Vec::new();
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(error) => {
                    if !visit(WalkItem::Error { path: &d, error }) {
                        return;
                    }
                    continue;
                }
            };
            let path = entry.path();
            // DirEntry::metadata never follows links.
            let meta = match entry.metadata() {
                Ok(m) => meta_from(&m),
                Err(error) => {
                    if !visit(WalkItem::Error { path: &path, error }) {
                        return;
                    }
                    continue;
                }
            };
            let go_on = match meta.kind {
                Kind::File => visit(WalkItem::File { path: &path, meta }),
                Kind::Link => visit(WalkItem::SkippedLink { path: &path }),
                Kind::Dir => {
                    if recursive {
                        subdirs.push(path);
                    }
                    true
                }
                Kind::Other => true,
            };
            if !go_on {
                return;
            }
        }
        // Depth-first in name order keeps reads on one disk area at a time.
        subdirs.sort();
        stack.extend(subdirs.into_iter().rev());
    }
}
