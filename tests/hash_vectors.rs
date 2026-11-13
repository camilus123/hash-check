//! Hashing correctness: known vectors for each algorithm.
//!
//! The empty-input and "abc" digests are the published vectors. The
//! pattern digests were computed with independent implementations
//! (Python `hashlib`, the reference C XXH3 via `xxhash`, and the official
//! `blake3` bindings), so a bug shared by our code paths cannot hide.

use hash_check::hasher::{Algo, Hasher, hash_bytes, to_hex};
use hash_check::hashfile::{BUF_SIZE, HashOutcome, hash_file};

/// Deterministic test data: byte i is (i * 31 + 7) mod 251.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

const VECTORS: &[(Algo, &str, usize, &str)] = &[
    (
        Algo::Blake3,
        "empty",
        0,
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
    ),
    (
        Algo::Xxh3_128,
        "empty",
        0,
        "99aa06d3014798d86001c324468d497f",
    ),
    (
        Algo::Sha256,
        "empty",
        0,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ),
    (
        Algo::Blake3,
        "pattern",
        1000,
        "beb16c4780ceb84cec6cf34484eb280d8d7faaa5970da29b2aaf08bc2efdd795",
    ),
    (
        Algo::Xxh3_128,
        "pattern",
        1000,
        "e719034ed2c87a3684b0c79e3e1ac40e",
    ),
    (
        Algo::Sha256,
        "pattern",
        1000,
        "008549d94fa71e7a0a483d84380d05a923a4b18e79ba1f8a8ddac923956d32ef",
    ),
    (
        Algo::Blake3,
        "pattern",
        3_000_001,
        "daceb13f0328accf668e885eeeccfebeedda5f1863d7778bc99c5332018fb0fe",
    ),
    (
        Algo::Xxh3_128,
        "pattern",
        3_000_001,
        "923bf7eac1e68f670d8f86900a9ce1ba",
    ),
    (
        Algo::Sha256,
        "pattern",
        3_000_001,
        "88f6072481ae464b10be82bfdae476ef79e6c58e7535044dbb5d93488aa8ff98",
    ),
    (
        Algo::Blake3,
        "pattern",
        9_000_003,
        "4b57b5ac847b252a9ab952b50afac8e364c24877ed9787187be6dfe0df4ac8ee",
    ),
    (
        Algo::Xxh3_128,
        "pattern",
        9_000_003,
        "dc1a1585e62853634612e067b2c72179",
    ),
    (
        Algo::Sha256,
        "pattern",
        9_000_003,
        "e7adf09a0ac95d8ad682c651b5ad1baa13f94e0a4ba75f964fd157fe09beff66",
    ),
];

#[test]
fn published_abc_vectors() {
    assert_eq!(
        to_hex(&hash_bytes(Algo::Blake3, b"abc")),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
    assert_eq!(
        to_hex(&hash_bytes(Algo::Xxh3_128, b"abc")),
        "06b05ab6733a618578af5f94892f3950"
    );
    assert_eq!(
        to_hex(&hash_bytes(Algo::Sha256, b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn one_shot_matches_reference() {
    for &(algo, _, n, expected) in VECTORS {
        let got = hash_bytes(algo, &pattern(n));
        assert_eq!(to_hex(&got), expected, "{} over {n} bytes", algo.name());
        assert_eq!(got.len(), algo.digest_len());
    }
}

#[test]
fn streaming_in_odd_chunks_matches_reference() {
    for &(algo, _, n, expected) in VECTORS {
        let data = pattern(n);
        for chunk in [1usize, 7, 63, 64, 65, 1023, 4096, 65_537, 1_000_003] {
            if n / chunk > 200_000 {
                continue; // keep the test fast
            }
            let mut h = Hasher::new(algo);
            for part in data.chunks(chunk) {
                h.update(part, false);
            }
            assert_eq!(
                to_hex(&h.finalize()),
                expected,
                "{} / {n} bytes / chunks of {chunk}",
                algo.name()
            );
        }
    }
}

#[test]
fn parallel_update_matches_serial() {
    for &(algo, _, n, expected) in VECTORS {
        let data = pattern(n);
        let mut h = Hasher::new(algo);
        for part in data.chunks(2 * 1024 * 1024) {
            h.update(part, true);
        }
        assert_eq!(to_hex(&h.finalize()), expected, "{} parallel", algo.name());
    }
}

#[test]
fn files_hash_to_reference_including_larger_than_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let mut buf = Vec::new();
    for &(algo, _, n, expected) in VECTORS {
        let path = dir.path().join(format!("{}-{n}.bin", algo.id()));
        std::fs::write(&path, pattern(n)).unwrap();
        for parallel in [false, true] {
            let mut seen = 0u64;
            let out = hash_file(&path, algo, &mut buf, parallel, &mut |b| {
                seen += b;
                true
            })
            .unwrap();
            match out {
                HashOutcome::Done { hash, size, .. } => {
                    assert_eq!(to_hex(&hash), expected, "{} file of {n} bytes", algo.name());
                    assert_eq!(size, n as u64);
                    assert_eq!(seen, n as u64);
                }
                other => panic!("unexpected outcome {other:?}"),
            }
        }
    }
    assert!(
        9_000_003 > 2 * BUF_SIZE,
        "largest vector must span several read buffers"
    );
}

#[test]
fn algorithm_ids_are_stable() {
    // These ids are stored in databases; changing them would corrupt meaning.
    assert_eq!(Algo::Blake3.id(), 1);
    assert_eq!(Algo::Xxh3_128.id(), 2);
    assert_eq!(Algo::Sha256.id(), 3);
    for a in Algo::ALL {
        assert_eq!(Algo::from_id(a.id()), Some(a));
    }
    assert_eq!(Algo::from_id(0), None);
}
