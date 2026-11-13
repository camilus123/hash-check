//! Hash one file through [`fsread`], detecting changes during the read.

use crate::fsread::{self, FileId, FileMeta, Kind};
use crate::hasher::{Algo, Hasher};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Mutex;

/// Read block size. Large enough for BLAKE3 to spread each block over the
/// cores and for sequential disk reads to stay efficient. A multiple of
/// every common sector size, as direct I/O requires.
pub const BUF_SIZE: usize = 4 * 1024 * 1024;

/// Alignment direct (unbuffered) reads need; covers 512-byte and 4K sectors.
const ALIGN: usize = 4096;

/// A reusable read buffer whose start is aligned for direct I/O.
pub struct ReadBuf {
    raw: Vec<u8>,
    offset: usize,
}

impl ReadBuf {
    pub fn new() -> ReadBuf {
        let raw = vec![0u8; BUF_SIZE + ALIGN];
        let offset = raw.as_ptr().align_offset(ALIGN);
        // align_offset may decline (returning usize::MAX); 0 is then still correct for buffered reads.
        let offset = if offset < ALIGN { offset } else { 0 };
        ReadBuf { raw, offset }
    }

    fn block(&mut self) -> &mut [u8] {
        &mut self.raw[self.offset..self.offset + BUF_SIZE]
    }

    pub fn is_aligned(&self) -> bool {
        (self.raw.as_ptr() as usize + self.offset).is_multiple_of(ALIGN)
    }
}

impl Default for ReadBuf {
    fn default() -> Self {
        ReadBuf::new()
    }
}

/// How to read and hash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HashOpts {
    /// Let BLAKE3 spread large blocks over all cores.
    pub parallel: bool,
    /// Read past the operating system's file cache (direct I/O), so the
    /// data really comes from the drive. Falls back to normal reads where
    /// the file system does not support it.
    pub direct: bool,
}

/// Hashes of files that have several names (hard links), so the data is
/// read once however many of its names are added in one run.
#[derive(Default)]
pub struct LinkCache {
    done: Mutex<HashMap<(FileId, Algo), (Vec<u8>, u64, i64)>>,
}

impl LinkCache {
    pub fn new() -> LinkCache {
        LinkCache::default()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HashOutcome {
    Done {
        hash: Vec<u8>,
        size: u64,
        mtime_ns: i64,
        /// The data was not read again: it is a hard link to a file
        /// already hashed in this run.
        reused: bool,
    },
    /// Size or modified time changed while reading: the hash is meaningless
    /// and has been discarded.
    ChangedDuringHash,
    /// `on_bytes` asked to stop.
    Cancelled,
}

/// Hash `path` with `algo`. `on_bytes(n)` is called after every block with
/// the number of bytes just hashed and returns `false` to cancel. With
/// `links`, a file that is a hard link to one already hashed through the
/// same cache is not read again.
pub fn hash_file(
    path: &Path,
    algo: Algo,
    buf: &mut ReadBuf,
    opts: HashOpts,
    links: Option<&LinkCache>,
    on_bytes: &mut dyn FnMut(u64) -> bool,
) -> io::Result<HashOutcome> {
    // Refuse links up front: we hash what the entry is, not where it points.
    let pre = fsread::stat(path)?;
    if pre.kind != Kind::File {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if pre.cloud_only {
        return Err(io::Error::other(
            "online-only cloud file (not downloaded to this computer); skipped so that it is not downloaded",
        ));
    }
    let mut f = if opts.direct && buf.is_aligned() {
        // Not every file system supports direct reads (network shares,
        // some virtual drives): fall back rather than fail.
        match fsread::open_direct(path) {
            Ok(f) => f,
            Err(_) => fsread::open(path)?,
        }
    } else {
        fsread::open(path)?
    };
    let before: FileMeta = f.meta()?;
    let identity = links.and_then(|_| f.link_identity());
    if let (Some(cache), Some(id)) = (links, identity)
        && let Some((hash, size, mtime_ns)) = cache.done.lock().unwrap().get(&(id, algo))
        && *size == before.size
        && *mtime_ns == before.mtime_ns
    {
        return Ok(HashOutcome::Done {
            hash: hash.clone(),
            size: *size,
            mtime_ns: *mtime_ns,
            reused: true,
        });
    }
    let mut hasher = Hasher::new(algo);
    let mut total: u64 = 0;
    let block = buf.block();
    loop {
        let n = f.read(block)?;
        if n == 0 {
            break;
        }
        hasher.update(&block[..n], opts.parallel);
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
    let hash = hasher.finalize();
    if let (Some(cache), Some(id)) = (links, identity) {
        cache
            .done
            .lock()
            .unwrap()
            .insert((id, algo), (hash.clone(), before.size, before.mtime_ns));
    }
    Ok(HashOutcome::Done {
        hash,
        size: before.size,
        mtime_ns: before.mtime_ns,
        reused: false,
    })
}
