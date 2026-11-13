//! Hash one file through [`fsread`], detecting changes during the read.

use crate::fsread::{self, FileMeta, Kind};
use crate::hasher::{Algo, Hasher};
use std::io;
use std::path::Path;

/// Read buffer size. Large enough for BLAKE3 to parallelise each block and
/// for sequential disk reads to stay efficient; reused across files.
pub const BUF_SIZE: usize = 4 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum HashOutcome {
    Done {
        hash: Vec<u8>,
        size: u64,
        mtime_ns: i64,
    },
    /// Size or modified time changed while reading: the hash is meaningless
    /// and has been discarded.
    ChangedDuringHash,
    /// `on_bytes` asked to stop.
    Cancelled,
}

/// Hash `path` with `algo`. `on_bytes(n)` is called after every block with
/// the number of bytes just hashed and returns `false` to cancel.
pub fn hash_file(
    path: &Path,
    algo: Algo,
    buf: &mut Vec<u8>,
    parallel: bool,
    on_bytes: &mut dyn FnMut(u64) -> bool,
) -> io::Result<HashOutcome> {
    if buf.len() != BUF_SIZE {
        buf.resize(BUF_SIZE, 0);
    }
    // Refuse links up front: we hash what the entry is, not where it points.
    let pre = fsread::stat(path)?;
    if pre.kind != Kind::File {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut f = fsread::open(path)?;
    let before: FileMeta = f.meta()?;
    let mut hasher = Hasher::new(algo);
    let mut total: u64 = 0;
    loop {
        let n = f.read(buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n], parallel);
        total += n as u64;
        if !on_bytes(n as u64) {
            return Ok(HashOutcome::Cancelled);
        }
    }
    // Compare against both the handle and a fresh look at the path, so a
    // replaced file is caught as well as an in-place write.
    let after = f.meta()?;
    let after_path = fsread::stat(path)?;
    let unchanged = total == before.size
        && after.size == before.size
        && after.mtime_ns == before.mtime_ns
        && after_path.size == before.size
        && after_path.mtime_ns == before.mtime_ns;
    if !unchanged {
        return Ok(HashOutcome::ChangedDuringHash);
    }
    Ok(HashOutcome::Done {
        hash: hasher.finalize(),
        size: before.size,
        mtime_ns: before.mtime_ns,
    })
}
