//! Verify end to end: every classification, new-file detection, reports.
//! All files touched here were created by the test in a temp directory.

use hash_check::db::{Db, Entry, Filter, Scope, Status};
use hash_check::engine::{
    self, ExportDest, Job, JobKind, Phase, Shared, State, VerifyTarget, add_blocking, run_blocking,
};
use hash_check::hasher::{Algo, hash_bytes, to_hex};
use hash_check::report::Format;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

struct Fx {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    app: PathBuf,
    db_path: PathBuf,
}

const NAMES: [&str; 8] = [
    "ok.bin",
    "touched.bin",
    "edited.bin",
    "grown.bin",
    "rotten.bin",
    "gone.bin",
    "becomes-dir.bin",
    "sub/deep ok.bin",
];

fn content(name: &str) -> Vec<u8> {
    format!("original content of {name} ")
        .repeat(40)
        .into_bytes()
}

fn fixture(algo: Algo) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let app = tmp.path().join("app");
    fs::create_dir_all(data.join("sub")).unwrap();
    fs::create_dir_all(&app).unwrap();
    for name in NAMES {
        fs::write(data.join(name), content(name)).unwrap();
    }
    let db_path = app.join("hash-check.db");
    let st = add_blocking(&db_path, &[data.clone()], true, algo);
    assert_eq!(st.added as usize, NAMES.len());
    Fx {
        _tmp: tmp,
        data,
        app,
        db_path,
    }
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

/// Overwrite a file's content but put its modified time back, the way
/// silent corruption (or a careless tool) would leave it.
fn rewrite_keeping_mtime(path: &Path, new_content: &[u8]) {
    let before = mtime(path);
    fs::write(path, new_content).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(before)
        .unwrap();
    assert_eq!(mtime(path), before);
}

fn bump_mtime(path: &Path) {
    let later = mtime(path) + Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(later)
        .unwrap();
}

/// Apply one of each kind of change to the fixture's files.
fn disturb(fx: &Fx) {
    let d = &fx.data;
    // Same content, new timestamp.
    bump_mtime(&d.join("touched.bin"));
    // A normal edit: content and timestamp change, size happens to stay.
    let mut edited = content("edited.bin");
    edited[3] ^= 0xff;
    fs::write(d.join("edited.bin"), &edited).unwrap();
    bump_mtime(&d.join("edited.bin"));
    // Size changes but the timestamp is put back.
    let mut grown = content("grown.bin");
    grown.extend_from_slice(b"extra");
    rewrite_keeping_mtime(&d.join("grown.bin"), &grown);
    // Bit rot: one bit flips, size and timestamp identical.
    let mut rotten = content("rotten.bin");
    rotten[100] ^= 0x01;
    rewrite_keeping_mtime(&d.join("rotten.bin"), &rotten);
    // Deleted.
    fs::remove_file(d.join("gone.bin")).unwrap();
    // Replaced by a folder of the same name.
    fs::remove_file(d.join("becomes-dir.bin")).unwrap();
    fs::create_dir(d.join("becomes-dir.bin")).unwrap();
    // Two files the database has never seen.
    fs::write(d.join("brand new.txt"), b"new").unwrap();
    fs::write(d.join("sub").join("also new.txt"), b"new too").unwrap();
}

/// Entries keyed by their path relative to the fixture's `data` root
/// (entries of other roots keep their full path).
fn by_name(db: &Db) -> BTreeMap<String, Entry> {
    let roots = db.roots().unwrap();
    let root = roots
        .iter()
        .find(|r| r.path.ends_with("data"))
        .map(|r| PathBuf::from(&r.path))
        .unwrap();
    db.all_ids()
        .unwrap()
        .into_iter()
        .map(|id| db.entry(id).unwrap().unwrap())
        .map(|e| {
            let rel = match Path::new(&e.path).strip_prefix(&root) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => e.path.clone(),
            };
            (rel, e)
        })
        .collect()
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.clone(), (fs::read(&p).unwrap(), mtime(&p)));
            }
        }
    }
    out
}

fn verify_all(fx: &Fx) -> State {
    let st = run_blocking(
        &fx.db_path,
        Job::Verify {
            target: VerifyTarget::All,
        },
    );
    assert!(st.fatal.is_none(), "{:?}", st.fatal);
    assert_eq!((st.kind, st.phase), (JobKind::Verify, Phase::Finished));
    st
}

#[test]
fn untouched_files_verify_ok() {
    let fx = fixture(Algo::Blake3);
    let st = verify_all(&fx);
    assert_eq!(st.verify.ok as usize, NAMES.len());
    assert_eq!(
        (
            st.verify.changed,
            st.verify.possibly_corrupted,
            st.verify.missing,
            st.verify.unreadable,
            st.verify.new
        ),
        (0, 0, 0, 0, 0)
    );
    assert_eq!(st.problem_count, 0);
    assert_eq!(st.run_id, 1);
    let db = Db::open(&fx.db_path).unwrap();
    assert!(
        by_name(&db)
            .values()
            .all(|e| e.status == Status::Ok && e.last_run == 1)
    );
}

#[test]
fn every_kind_of_change_is_classified_correctly() {
    for algo in Algo::ALL {
        let fx = fixture(algo);
        let before = by_name(&Db::open(&fx.db_path).unwrap());
        disturb(&fx);
        let disk_before_verify = snapshot(&fx.data);

        let st = verify_all(&fx);
        let v = &st.verify;
        assert_eq!(
            v.ok,
            3,
            "ok.bin, touched.bin and sub/deep ok.bin ({})",
            algo.name()
        );
        assert_eq!(v.changed, 2, "edited.bin and grown.bin");
        assert_eq!(v.possibly_corrupted, 1, "rotten.bin");
        assert_eq!(v.missing, 1, "gone.bin");
        assert_eq!(v.unreadable, 1, "becomes-dir.bin");
        assert_eq!(v.new, 2);
        assert_eq!(v.unavailable, 0);

        let db = Db::open(&fx.db_path).unwrap();
        let after = by_name(&db);
        let status = |n: &str| after[n].status;
        assert_eq!(status("ok.bin"), Status::Ok);
        assert_eq!(status("sub/deep ok.bin"), Status::Ok);
        assert_eq!(status("touched.bin"), Status::Ok);
        assert_eq!(status("edited.bin"), Status::Changed);
        assert_eq!(status("grown.bin"), Status::Changed);
        assert_eq!(status("rotten.bin"), Status::PossiblyCorrupted);
        assert_eq!(status("gone.bin"), Status::Missing);
        assert_eq!(status("becomes-dir.bin"), Status::Unreadable);

        // Verify never replaces a stored hash, whatever it finds.
        for (name, e) in &after {
            assert_eq!(
                e.hash, before[name].hash,
                "stored hash of {name} must not change"
            );
            assert_eq!(e.hash, hash_bytes(algo, &content(name)));
            assert_eq!(e.algo, algo);
            assert_eq!(e.first_hashed, before[name].first_hashed);
            assert_eq!(e.last_run, 1);
        }
        // A touched file is OK and its stored timestamp follows the file...
        assert_ne!(
            after["touched.bin"].mtime_ns,
            before["touched.bin"].mtime_ns
        );
        // ...but for problems the record stays as it was, so the evidence is kept.
        for name in ["edited.bin", "grown.bin", "rotten.bin", "gone.bin"] {
            assert_eq!(after[name].mtime_ns, before[name].mtime_ns, "{name}");
            assert_eq!(after[name].size, before[name].size, "{name}");
        }
        let detail = |n: &str| after[n].last_error.clone().unwrap_or_default();
        assert!(
            detail("grown.bin").contains("size"),
            "{}",
            detail("grown.bin")
        );
        assert!(
            detail("edited.bin").contains("modified"),
            "{}",
            detail("edited.bin")
        );
        let mut rotten = content("rotten.bin");
        rotten[100] ^= 0x01;
        assert!(
            detail("rotten.bin").contains(&to_hex(&hash_bytes(algo, &rotten))),
            "the current hash of a possibly corrupted file is recorded: {}",
            detail("rotten.bin")
        );
        assert_eq!(after["ok.bin"].last_error, None);

        // New files are listed, not added.
        let new: Vec<String> = db
            .new_file_ids(Scope::All, "")
            .unwrap()
            .into_iter()
            .map(|id| db.new_file(id).unwrap().unwrap().path)
            .collect();
        assert_eq!(new.len(), 2);
        assert!(new.iter().any(|p| p.ends_with("brand new.txt")));
        assert!(new.iter().any(|p| p.ends_with("also new.txt")));
        assert_eq!(db.count_files().unwrap() as usize, NAMES.len());

        // Filters see the same picture.
        let c = db.status_counts(None, Some(1)).unwrap();
        assert_eq!(
            (
                c.ok,
                c.changed,
                c.possibly_corrupted,
                c.missing,
                c.unreadable
            ),
            (3, 2, 1, 1, 1)
        );
        let corrupted = db
            .ids_matching(&Filter {
                status: Some(Status::PossiblyCorrupted),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(corrupted, vec![after["rotten.bin"].id]);
        drop(db);

        // THE RULE: verifying changed nothing on disk.
        assert_eq!(
            snapshot(&fx.data),
            disk_before_verify,
            "verify must not touch user files"
        );

        // Verifying again gives the same answers: a corrupted file stays flagged.
        let again = verify_all(&fx);
        assert_eq!(again.verify, st.verify);
        assert_eq!(again.run_id, 2);
    }
}

#[test]
fn verifying_selected_entries_or_one_root_leaves_the_rest_alone() {
    let fx = fixture(Algo::Xxh3_128);
    // A second root.
    let other = fx.data.parent().unwrap().join("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("x.bin"), b"x").unwrap();
    add_blocking(&fx.db_path, &[other.clone()], true, Algo::Sha256);
    disturb(&fx);
    fs::write(other.join("new in other.bin"), b"n").unwrap();

    let db = Db::open(&fx.db_path).unwrap();
    let names = by_name(&db);
    let picked = vec![names["rotten.bin"].id, names["ok.bin"].id];
    drop(db);

    // Selected entries: only those, and no scan for new files.
    let st = run_blocking(
        &fx.db_path,
        Job::Verify {
            target: VerifyTarget::Entries(picked.clone()),
        },
    );
    assert_eq!(
        (st.verify.ok, st.verify.possibly_corrupted, st.verify.new),
        (1, 1, 0)
    );
    assert_eq!(st.files_total, 2);
    let db = Db::open(&fx.db_path).unwrap();
    let c = db.status_counts(None, None).unwrap();
    assert_eq!(
        (c.possibly_corrupted, c.changed, c.missing),
        (1, 0, 0),
        "unselected entries keep their old status"
    );
    assert_eq!(
        db.ids_matching(&Filter {
            run: Some(1),
            ..Default::default()
        })
        .unwrap()
        .len(),
        2
    );
    assert_eq!(db.count_new_files(Scope::All).unwrap(), 0);
    let other_root = db
        .roots()
        .unwrap()
        .into_iter()
        .find(|r| r.path.ends_with("other"))
        .unwrap();
    drop(db);

    // One root: its entries and its new files only.
    let st = run_blocking(
        &fx.db_path,
        Job::Verify {
            target: VerifyTarget::Root(other_root.id),
        },
    );
    assert_eq!((st.verify.ok, st.verify.new, st.files_total), (1, 1, 1));
    let db = Db::open(&fx.db_path).unwrap();
    assert_eq!(db.count_new_files(Scope::Root(other_root.id)).unwrap(), 1);
    assert_eq!(db.count_new_files(Scope::All).unwrap(), 1);
    assert_eq!(
        db.status_counts(None, None).unwrap().missing,
        0,
        "the first root was not verified"
    );
}

#[test]
fn new_files_can_be_added_and_update_clears_a_flag() {
    let fx = fixture(Algo::Blake3);
    disturb(&fx);
    verify_all(&fx);
    let db = Db::open(&fx.db_path).unwrap();
    let new_ids = db.new_file_ids(Scope::All, "").unwrap();
    let rotten = by_name(&db)["rotten.bin"].clone();
    drop(db);

    // Add the new files, with a different algorithm than the rest.
    let st = run_blocking(
        &fx.db_path,
        Job::AddNew {
            new_file_ids: new_ids,
            algo: Algo::Sha256,
        },
    );
    assert_eq!((st.kind, st.added, st.problem_count), (JobKind::Add, 2, 0));
    let db = Db::open(&fx.db_path).unwrap();
    assert_eq!(
        db.count_new_files(Scope::All).unwrap(),
        0,
        "added files leave the New list"
    );
    let names = by_name(&db);
    assert_eq!(
        names["brand new.txt"].hash,
        hash_bytes(Algo::Sha256, b"new")
    );
    assert_eq!(names["brand new.txt"].algo, Algo::Sha256);
    assert_eq!(
        names["brand new.txt"].root_id, rotten.root_id,
        "they belong to the root they were found in"
    );
    drop(db);

    // The user decides the "rotten" content is right after all and updates it.
    let st = run_blocking(
        &fx.db_path,
        Job::Update {
            ids: vec![rotten.id],
        },
    );
    assert_eq!(st.updated, 1);
    let st = verify_all(&fx);
    assert_eq!((st.verify.possibly_corrupted, st.verify.new), (0, 0));
    assert_eq!(
        st.verify.ok, 6,
        "3 intact + 2 newly added + the updated one"
    );
    assert_eq!(
        by_name(&Db::open(&fx.db_path).unwrap())["rotten.bin"].status,
        Status::Ok
    );
}

#[test]
fn restored_file_goes_back_to_ok() {
    let fx = fixture(Algo::Blake3);
    disturb(&fx);
    verify_all(&fx);
    // Restore from "backup": original content, any timestamp.
    fs::write(fx.data.join("rotten.bin"), content("rotten.bin")).unwrap();
    fs::write(fx.data.join("gone.bin"), content("gone.bin")).unwrap();
    let st = verify_all(&fx);
    assert_eq!(
        (
            st.verify.possibly_corrupted,
            st.verify.missing,
            st.verify.ok
        ),
        (0, 0, 5)
    );
    let names = by_name(&Db::open(&fx.db_path).unwrap());
    assert_eq!(names["rotten.bin"].status, Status::Ok);
    assert_eq!(names["rotten.bin"].last_error, None);
    assert_eq!(names["gone.bin"].status, Status::Ok);
}

#[cfg(unix)]
#[test]
fn permission_denied_is_unreadable_with_the_os_error() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture(Algo::Blake3);
    let locked = fx.data.join("ok.bin");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&locked).is_ok() {
        eprintln!("running as root, permissions are not enforced; skipping");
        return;
    }
    let st = verify_all(&fx);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(st.verify.unreadable, 1);
    let e = by_name(&Db::open(&fx.db_path).unwrap())["ok.bin"].clone();
    assert_eq!(e.status, Status::Unreadable);
    assert!(
        e.last_error.unwrap().to_lowercase().contains("permission"),
        "the OS error is shown"
    );
}

/// A file another program holds open without sharing is Unreadable, and
/// hash-check must not disturb that program.
#[cfg(windows)]
#[test]
fn file_locked_by_another_program_is_unreadable() {
    use std::os::windows::fs::OpenOptionsExt;
    let fx = fixture(Algo::Blake3);
    let path = fx.data.join("ok.bin");
    let exclusive = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let st = verify_all(&fx);
    assert_eq!(st.verify.unreadable, 1, "{:?}", st.verify);
    let e = by_name(&Db::open(&fx.db_path).unwrap())["ok.bin"].clone();
    assert_eq!(e.status, Status::Unreadable);
    assert!(!e.last_error.unwrap().is_empty(), "the OS error is shown");
    drop(exclusive);
    // Once released it verifies fine again.
    assert_eq!(verify_all(&fx).verify.unreadable, 0);
}

/// hash-check's own handles must not block other programs: while a file is
/// open through fsread, others can still write, rename and delete it.
#[cfg(windows)]
#[test]
fn reading_a_file_does_not_block_other_programs() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("shared.bin");
    fs::write(&path, b"content").unwrap();
    let mut ours = hash_check::fsread::open(&path).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(ours.read(&mut buf).unwrap(), 4);
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("others can open for writing");
    let renamed = tmp.path().join("renamed.bin");
    fs::rename(&path, &renamed).expect("others can rename");
    fs::remove_file(&renamed).expect("others can delete");
}

/// Entries on a drive that is not connected are reported as unavailable,
/// not as missing, and their records are left exactly as they were.
#[cfg(windows)]
#[test]
fn offline_drive_is_unavailable_not_missing() {
    let fx = fixture(Algo::Blake3);
    let letter = ('D'..='Z')
        .rev()
        .find(|c| !Path::new(&format!("{c}:\\")).exists())
        .expect("a free drive letter");
    let db = Db::open(&fx.db_path).unwrap();
    let (root, _) = db
        .add_root(&format!("{letter}:\\archive"), true, 1)
        .unwrap();
    for i in 0..3 {
        db.insert_entry(&hash_check::db::NewEntry {
            path: &format!("{letter}:\\archive\\file{i}.bin"),
            root_id: Some(root),
            size: 10,
            mtime_ns: 5,
            algo: Algo::Blake3,
            hash: &hash_bytes(Algo::Blake3, b"x"),
            now: 1,
        })
        .unwrap();
    }
    drop(db);

    let st = verify_all(&fx);
    assert_eq!(st.verify.unavailable, 3);
    assert_eq!(
        st.verify.missing, 0,
        "an offline drive must not produce missing files"
    );
    assert_eq!(st.unavailable_volumes, vec![(format!("{letter}:\\"), 3)]);
    assert_eq!(st.verify.ok as usize, NAMES.len());
    let db = Db::open(&fx.db_path).unwrap();
    for id in db.ids_filtered(Scope::Root(root), "").unwrap() {
        let e = db.entry(id).unwrap().unwrap();
        assert_eq!(
            (e.status, e.last_run, e.last_verified),
            (Status::Ok, 0, 1),
            "record untouched"
        );
    }
    drop(db);

    let st = run_blocking(
        &fx.db_path,
        Job::Export {
            run: 1,
            format: Format::Text,
            dest: ExportDest::AppDir {
                file_name: "r.txt".into(),
            },
        },
    );
    let text = fs::read_to_string(st.report_path.unwrap()).unwrap();
    assert!(
        text.contains(&format!(
            "Drive not available: {letter}:\\ (3 entries not checked)"
        )),
        "{text}"
    );
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cancelled_verify_keeps_what_it_checked_and_says_so() {
    let fx = fixture(Algo::Blake3);
    let shared = Arc::new(Shared::new(JobKind::Verify));
    shared.set_paused(true);
    engine::start_on(
        shared.clone(),
        fx.db_path.clone(),
        Job::Verify {
            target: VerifyTarget::All,
        },
        Box::new(|| {}),
    );
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(shared.snapshot().files_done, 0, "paused");
    shared.request_cancel();
    wait_until("the cancelled verify to stop", || {
        shared.snapshot().phase == Phase::Finished
    });
    let s = shared.snapshot();
    assert!(s.cancelled && s.fatal.is_none());
    let db = Db::open(&fx.db_path).unwrap();
    assert_eq!(
        db.ids_matching(&Filter {
            run: Some(1),
            ..Default::default()
        })
        .unwrap()
        .len() as u64,
        s.verify.ok
    );
    assert!(db.integrity_check().unwrap());
    drop(db);

    let st = run_blocking(
        &fx.db_path,
        Job::Export {
            run: 1,
            format: Format::Text,
            dest: ExportDest::AppDir {
                file_name: "c.txt".into(),
            },
        },
    );
    assert!(
        fs::read_to_string(st.report_path.unwrap())
            .unwrap()
            .contains("CANCELLED")
    );
}

#[test]
fn reports_list_problems_first_and_never_overwrite() {
    let fx = fixture(Algo::Sha256);
    // A name that would be trouble in a careless CSV writer (and is legal on Windows).
    fs::write(fx.data.join("=evil, name.txt"), b"q").unwrap();
    add_blocking(&fx.db_path, &[fx.data.clone()], true, Algo::Sha256);
    disturb(&fx);
    verify_all(&fx);

    // CSV into the app folder.
    let st = run_blocking(
        &fx.db_path,
        Job::Export {
            run: 1,
            format: Format::Csv,
            dest: ExportDest::AppDir {
                file_name: "report.csv".into(),
            },
        },
    );
    assert!(st.fatal.is_none(), "{:?}", st.fatal);
    let path = st.report_path.clone().unwrap();
    assert_eq!(
        path,
        fx.app.join("reports").join("report.csv"),
        "reports go into the app folder"
    );
    let csv = fs::read_to_string(&path).unwrap();
    let csv = csv
        .strip_prefix('\u{feff}')
        .expect("UTF-8 byte-order mark for Excel");
    let lines: Vec<&str> = csv.split("\r\n").filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines[0],
        "status,path,size_bytes,modified,algorithm,stored_hash,last_verified,detail"
    );
    assert_eq!(lines.len(), 1 + 9 + 2, "header, 9 entries, 2 new files");
    assert_eq!(st.files_done, 9);
    let status_of = |l: &str| l.split(',').next().unwrap().to_string();
    let order: Vec<String> = lines[1..].iter().map(|l| status_of(l)).collect();
    assert_eq!(
        order,
        [
            "POSSIBLY CORRUPTED",
            "Unreadable",
            "Missing",
            "Changed",
            "Changed",
            "OK",
            "OK",
            "OK",
            "OK",
            "New",
            "New"
        ]
    );
    assert!(lines[1].contains("rotten.bin"));
    assert!(
        lines[1].contains(&to_hex(&hash_bytes(Algo::Sha256, &content("rotten.bin")))),
        "stored hash is listed"
    );
    assert!(lines[1].contains("SHA-256"));
    let evil = lines.iter().find(|l| l.contains("evil")).unwrap();
    assert!(
        evil.starts_with("OK,\""),
        "a path containing a comma is quoted: {evil}"
    );
    assert!(evil.contains("=evil, name.txt\","), "{evil}");

    // Text to a location the "user" picked.
    let chosen = fx.data.parent().unwrap().join("my report.txt");
    let st = run_blocking(
        &fx.db_path,
        Job::Export {
            run: 1,
            format: Format::Text,
            dest: ExportDest::Chosen(chosen.clone()),
        },
    );
    assert!(st.fatal.is_none(), "{:?}", st.fatal);
    let text = fs::read_to_string(&chosen).unwrap();
    assert!(text.contains("verification report"));
    assert!(text.contains("Scope:      everything"));
    let pos = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle} not in report:\n{text}"))
    };
    assert!(pos("POSSIBLY CORRUPTED (1)") < pos("Missing (1)"));
    assert!(pos("Missing (1)") < pos("Changed (2)"));
    assert!(pos("Changed (2)") < pos("OK (4)"));
    assert!(pos("OK (4)") < pos("New: under a tracked folder"));
    assert!(!text.contains("CANCELLED"));

    // Exporting onto an existing file is refused and the file is left alone.
    let precious = fx.data.parent().unwrap().join("precious.txt");
    fs::write(&precious, b"do not overwrite me").unwrap();
    let st = run_blocking(
        &fx.db_path,
        Job::Export {
            run: 1,
            format: Format::Csv,
            dest: ExportDest::Chosen(precious.clone()),
        },
    );
    assert!(
        st.fatal
            .as_deref()
            .unwrap_or("")
            .contains("never overwrites"),
        "{:?}",
        st.fatal
    );
    assert!(st.report_path.is_none());
    assert_eq!(fs::read(&precious).unwrap(), b"do not overwrite me");
    let st = run_blocking(
        &fx.db_path,
        Job::Export {
            run: 1,
            format: Format::Csv,
            dest: ExportDest::AppDir {
                file_name: "report.csv".into(),
            },
        },
    );
    assert!(
        st.fatal.is_some(),
        "the same report name twice is refused too"
    );
    assert_eq!(
        fs::read_to_string(&path)
            .unwrap()
            .trim_start_matches('\u{feff}'),
        csv
    );
}

#[test]
fn removed_root_folder_is_reported_and_its_files_are_missing() {
    let fx = fixture(Algo::Blake3);
    let other = fx.data.parent().unwrap().join("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("x.bin"), b"x").unwrap();
    add_blocking(&fx.db_path, &[other.clone()], true, Algo::Blake3);
    fs::remove_file(other.join("x.bin")).unwrap();
    fs::remove_dir(&other).unwrap();
    let st = verify_all(&fx);
    assert_eq!(st.verify.missing, 1);
    assert_eq!(st.verify.unavailable, 0);
    assert_eq!(st.problem_count, 1, "{:?}", st.problems);
    assert!(st.problems[0].message.contains("no longer exists"));
}
