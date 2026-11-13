//! Per-drive scheduling: results must not depend on how many threads read
//! a drive, on direct I/O, or on the order in which results arrive.

use hash_check::db::{Db, Scope, Status};
use hash_check::drives::{self, DriveKind};
use hash_check::engine::{
    DIRECT_IO_KEY, Job, VerifyTarget, add_blocking, drive_threads_key, run_blocking,
};
use hash_check::hasher::{Algo, hash_bytes};
use hash_check::verify::volume_root;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const FILES: usize = 150;

fn content(i: usize) -> Vec<u8> {
    // Sizes from empty to a few hundred kilobytes, plus two files larger
    // than the read buffer.
    let len = match i {
        0 => 0,
        7 => 9 * 1024 * 1024 + 3,
        8 => 5 * 1024 * 1024,
        _ => (i * 7919) % 300_000,
    };
    (0..len).map(|b| ((b * 31 + i * 17) % 251) as u8).collect()
}

struct Fx {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    db_path: PathBuf,
    volume: String,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    for i in 0..FILES {
        let dir = data.join(format!("d{}", i % 7));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("f{i:03}.bin")), content(i)).unwrap();
    }
    let db_path = tmp.path().join("app").join("hash-check.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    // Paths are stored in canonical form; settings are keyed by their volume.
    let canonical = hash_check::fsread::resolve(&data).unwrap();
    let volume = volume_root(canonical.to_str().unwrap())
        .unwrap()
        .to_string();
    Fx {
        _tmp: tmp,
        data,
        db_path,
        volume,
    }
}

fn configure(fx: &Fx, threads: usize, direct: bool) {
    let db = Db::open(&fx.db_path).unwrap();
    db.set_meta(&drive_threads_key(&fx.volume), &threads.to_string())
        .unwrap();
    db.set_meta(DIRECT_IO_KEY, if direct { "1" } else { "0" })
        .unwrap();
}

fn index_of(path: &str) -> usize {
    let name = Path::new(path).file_stem().unwrap().to_str().unwrap();
    name[1..].parse().unwrap()
}

fn mtime(p: &Path) -> SystemTime {
    fs::metadata(p).unwrap().modified().unwrap()
}

#[test]
fn any_thread_count_and_read_mode_gives_the_same_database() {
    for (threads, direct) in [
        (1, false),
        (2, false),
        (8, false),
        (32, false),
        (1, true),
        (8, true),
    ] {
        let fx = fixture();
        configure(&fx, threads, direct);
        for algo in Algo::ALL {
            let db_path = fx.db_path.with_file_name(format!("t-{}.db", algo.id()));
            fs::copy(&fx.db_path, &db_path).unwrap(); // carries the settings
            let st = add_blocking(&db_path, &[fx.data.clone()], true, algo);
            assert!(st.fatal.is_none(), "{:?}", st.fatal);
            assert_eq!(st.problem_count, 0, "{:?}", st.problems);
            assert_eq!((st.added, st.files_done), (FILES as u64, FILES as u64));
            assert_eq!(st.plans.len(), 1, "one drive, one pool: {:?}", st.plans);
            let expected_mode = if threads == 1 {
                "1 file at a time".to_string()
            } else {
                format!("{threads} files in parallel")
            };
            assert!(
                st.plans[0].contains(&expected_mode),
                "{:?} (threads {threads})",
                st.plans
            );

            let db = Db::open(&db_path).unwrap();
            let ids = db.all_ids().unwrap();
            assert_eq!(ids.len(), FILES);
            for id in ids {
                let e = db.entry(id).unwrap().unwrap();
                let i = index_of(&e.path);
                assert_eq!(
                    e.hash,
                    hash_bytes(algo, &content(i)),
                    "file {i}, {} threads, direct {direct}, {}",
                    threads,
                    algo.name()
                );
                assert_eq!(e.size as usize, content(i).len());
            }
            assert!(db.integrity_check().unwrap());
        }
    }
}

#[test]
fn parallel_verify_classifies_like_sequential_verify() {
    let mut outcomes = Vec::new();
    for threads in [1usize, 8] {
        let fx = fixture();
        configure(&fx, threads, false);
        add_blocking(&fx.db_path, &[fx.data.clone()], true, Algo::Blake3);
        // Disturb a spread of files across the folders.
        for i in (0..FILES).filter(|i| i % 10 == 3) {
            let path = fx
                .data
                .join(format!("d{}", i % 7))
                .join(format!("f{i:03}.bin"));
            match i % 40 {
                3 => fs::remove_file(&path).unwrap(),
                13 => {
                    // Bit rot: flip a bit, restore the timestamp.
                    let before = mtime(&path);
                    let mut c = content(i);
                    c[0] ^= 1;
                    fs::write(&path, c).unwrap();
                    fs::File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_modified(before)
                        .unwrap();
                }
                23 => fs::write(&path, b"edited, different length").unwrap(),
                _ => fs::write(path.with_extension("new"), b"untracked").unwrap(),
            }
        }
        let st = run_blocking(
            &fx.db_path,
            Job::Verify {
                target: VerifyTarget::All,
            },
        );
        assert!(st.fatal.is_none(), "{:?}", st.fatal);
        assert_eq!(st.files_done, FILES as u64);
        let v = st.verify.clone();
        assert_eq!(
            v.ok + v.changed + v.possibly_corrupted + v.missing + v.unreadable,
            FILES as u64
        );
        assert_eq!(
            (v.missing, v.possibly_corrupted, v.changed, v.new),
            (4, 4, 4, 3),
            "threads {threads}"
        );

        let db = Db::open(&fx.db_path).unwrap();
        let c = db.status_counts(None, Some(1)).unwrap();
        assert_eq!(
            (c.ok, c.changed, c.possibly_corrupted, c.missing),
            (v.ok, v.changed, v.possibly_corrupted, v.missing)
        );
        let mut per_file: Vec<(usize, Status)> = db
            .ids_filtered(Scope::All, "")
            .unwrap()
            .into_iter()
            .map(|id| db.entry(id).unwrap().unwrap())
            .map(|e| (index_of(&e.path), e.status))
            .collect();
        per_file.sort_by_key(|(i, s)| (*i, s.id()));
        outcomes.push(per_file);
    }
    assert_eq!(
        outcomes[0], outcomes[1],
        "1 thread and 8 threads must agree file by file"
    );
}

#[test]
fn thread_setting_is_per_volume_and_bounded() {
    let fx = fixture();
    let db = Db::open(&fx.db_path).unwrap();
    // Nonsense values fall back to the automatic choice; huge ones are capped.
    for (value, check) in [
        ("0", None),
        ("banana", None),
        (
            "9999",
            Some(format!("{} files in parallel", drives::MAX_THREADS)),
        ),
    ] {
        db.set_meta(&drive_threads_key(&fx.volume), value).unwrap();
        let st = add_blocking(&fx.db_path, &[fx.data.join("d0")], true, Algo::Xxh3_128);
        assert!(st.fatal.is_none());
        if let Some(text) = check {
            assert!(st.plans[0].contains(&text), "{:?}", st.plans);
        }
        let roots = db.roots().unwrap();
        drop(run_blocking(
            &fx.db_path,
            Job::RemoveRoot {
                root_id: roots[0].id,
            },
        ));
    }
}

/// Drive detection on the machine running the tests: must not fail, and a
/// local temp folder is never a network or optical drive.
#[test]
fn detecting_the_temp_drive_works() {
    let fx = fixture();
    let info = drives::detect(&fx.volume);
    println!(
        "temp drive: {info:?}, default threads {}",
        info.default_threads()
    );
    assert_eq!(info.volume, fx.volume);
    assert!(
        matches!(
            info.kind,
            DriveKind::Ssd | DriveKind::Hdd | DriveKind::Unknown
        ),
        "{info:?}"
    );
    assert!((1..=drives::MAX_THREADS).contains(&info.default_threads()));
    #[cfg(windows)]
    {
        let system = drives::detect(r"C:\");
        println!("system drive: {system:?}");
        assert!(
            system.disk.is_some() || system.kind != DriveKind::Unknown || system.model.is_some(),
            "expected Windows to tell us something about C: {system:?}"
        );
        // A drive letter that does not exist must simply come back unknown.
        let free = ('D'..='Z')
            .rev()
            .find(|c| !Path::new(&format!("{c}:\\")).exists())
            .unwrap();
        assert_eq!(
            drives::detect(&format!("{free}:\\")).kind,
            DriveKind::Unknown
        );
    }
}

#[test]
fn benchmark_measures_every_algorithm_and_changes_nothing() {
    let fx = fixture();
    configure(&fx, 4, false);
    let files_before: Vec<(PathBuf, SystemTime)> = {
        let mut v = Vec::new();
        for d in fs::read_dir(&fx.data).unwrap() {
            for f in fs::read_dir(d.unwrap().path()).unwrap() {
                let p = f.unwrap().path();
                v.push((p.clone(), mtime(&p)));
            }
        }
        v.sort();
        v
    };
    let total: u64 = (0..FILES).map(|i| content(i).len() as u64).sum();

    let st = run_blocking(
        &fx.db_path,
        Job::Benchmark {
            inputs: vec![fx.data.clone()],
            limit_bytes: 0,
        },
    );
    assert!(st.fatal.is_none(), "{:?}", st.fatal);
    assert_eq!(st.problem_count, 0, "{:?}", st.problems);
    for r in &st.bench {
        println!("{:<60} {:>8.0} MB/s", r.label, r.mb_per_s());
    }
    let labels: Vec<&str> = st.bench.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels.len(), 8, "{labels:?}");
    for algo in ["BLAKE3", "XXH3-128", "SHA-256"] {
        assert!(
            labels
                .iter()
                .any(|l| l.starts_with(algo) && l.contains("direct")),
            "{algo} on disk: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.starts_with(algo) && l.contains("in memory")),
            "{algo} in memory"
        );
    }
    for row in &st.bench[..4] {
        assert_eq!(
            row.bytes, total,
            "{}: every pass reads every (non-empty) file once",
            row.label
        );
        assert!(row.secs > 0.0 && row.mb_per_s() > 0.0);
    }
    assert!(
        st.bench_info.contains("4 files in parallel"),
        "{}",
        st.bench_info
    );
    assert_eq!(st.bytes_total, total * 4);

    // Nothing was tracked and nothing on disk changed.
    let db = Db::open(&fx.db_path).unwrap();
    assert_eq!(db.count_files().unwrap(), 0);
    assert!(db.roots().unwrap().is_empty());
    for (p, t) in files_before {
        assert_eq!(mtime(&p), t, "{}", p.display());
    }

    // A size limit stops early; an empty selection is an error, not a hang.
    let st = run_blocking(
        &fx.db_path,
        Job::Benchmark {
            inputs: vec![fx.data.clone()],
            limit_bytes: 1,
        },
    );
    assert!(st.bench[0].bytes < total && st.bench[0].bytes > 0);
    let empty = fx.data.parent().unwrap().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let st = run_blocking(
        &fx.db_path,
        Job::Benchmark {
            inputs: vec![empty],
            limit_bytes: 0,
        },
    );
    assert!(st.fatal.is_some());
}
