//! The three supported hash algorithms behind one streaming interface.
//!
//! All three crates pick their fastest implementation at runtime:
//! `blake3` (SSE2/SSE4.1/AVX2/AVX-512), `twox-hash` XXH3 (AVX2/SSE2) and
//! `sha2` (SHA-NI), each with a portable fallback.

use sha2::Digest;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Algo {
    Blake3,
    Xxh3_128,
    Sha256,
}

impl Algo {
    pub const ALL: [Algo; 3] = [Algo::Blake3, Algo::Xxh3_128, Algo::Sha256];
    pub const DEFAULT: Algo = Algo::Blake3;

    /// Stable numeric id stored in the database. Never renumber.
    pub fn id(self) -> i64 {
        match self {
            Algo::Blake3 => 1,
            Algo::Xxh3_128 => 2,
            Algo::Sha256 => 3,
        }
    }

    pub fn from_id(id: i64) -> Option<Algo> {
        Algo::ALL.into_iter().find(|a| a.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Algo::Blake3 => "BLAKE3",
            Algo::Xxh3_128 => "XXH3-128",
            Algo::Sha256 => "SHA-256",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Algo::Blake3 => "BLAKE3 (default: fast, cryptographic, multithreaded)",
            Algo::Xxh3_128 => "XXH3-128 (fastest, not cryptographic)",
            Algo::Sha256 => "SHA-256 (for compatibility)",
        }
    }

    /// Digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            Algo::Blake3 | Algo::Sha256 => 32,
            Algo::Xxh3_128 => 16,
        }
    }
}

/// Inputs at least this large are handed to BLAKE3's multithreaded update.
const BLAKE3_PARALLEL_MIN: usize = 256 * 1024;

pub enum Hasher {
    Blake3(Box<blake3::Hasher>),
    Xxh3(Box<twox_hash::xxhash3_128::Hasher>),
    Sha256(Box<sha2::Sha256>),
}

impl Hasher {
    pub fn new(algo: Algo) -> Hasher {
        match algo {
            Algo::Blake3 => Hasher::Blake3(Box::new(blake3::Hasher::new())),
            Algo::Xxh3_128 => Hasher::Xxh3(Box::new(twox_hash::xxhash3_128::Hasher::new())),
            Algo::Sha256 => Hasher::Sha256(Box::new(sha2::Sha256::new())),
        }
    }

    pub fn algo(&self) -> Algo {
        match self {
            Hasher::Blake3(_) => Algo::Blake3,
            Hasher::Xxh3(_) => Algo::Xxh3_128,
            Hasher::Sha256(_) => Algo::Sha256,
        }
    }

    /// Feed data. `parallel` lets BLAKE3 spread a large block over all cores;
    /// the other two algorithms are inherently sequential and ignore it.
    pub fn update(&mut self, data: &[u8], parallel: bool) {
        match self {
            Hasher::Blake3(h) => {
                if parallel && data.len() >= BLAKE3_PARALLEL_MIN {
                    h.update_rayon(data);
                } else {
                    h.update(data);
                }
            }
            Hasher::Xxh3(h) => h.write(data),
            Hasher::Sha256(h) => h.update(data),
        }
    }

    /// The digest in its canonical byte order (XXH3-128: big-endian, as
    /// printed by `xxhsum -H2`).
    pub fn finalize(self) -> Vec<u8> {
        match self {
            Hasher::Blake3(h) => h.finalize().as_bytes().to_vec(),
            Hasher::Xxh3(h) => h.finish_128().to_be_bytes().to_vec(),
            Hasher::Sha256(h) => h.finalize().as_slice().to_vec(),
        }
    }
}

/// Hash an in-memory buffer in one go.
pub fn hash_bytes(algo: Algo, data: &[u8]) -> Vec<u8> {
    let mut h = Hasher::new(algo);
    h.update(data, false);
    h.finalize()
}

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}
