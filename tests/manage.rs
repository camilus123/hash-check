//! Remove, Update, Pause and Cancel, on files this test creates.

use hash_check::db::{Db, Scope, Status};
use hash_check::engine::{self, Job, JobKind, Phase, Shared, add_blocking, run_blocking};
use hash_check::hasher::{Algo, hash_bytes};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    db_path: PathBuf,
}

/// `n` files in two folders (`a/` and `b/`), each tracked as its own root, plus one loose file.
fn fixture(n: usize, algo: Algo) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    for dir in ["a", "b"] {
        fs::create_dir_all(data.join(dir)).unwrap();
        for i in 0..n {
            fs::write(data.join(dir).join(format!("f{i:04}.bin")), format!("{dir}-{i}")).unwrap();
        }
    }
    fs::write(data.join("loose.txt"), b"loose").unwrap();
    let db_path = tmp.path().join("t.db");
    let st = add_blocking(&db_path, &[data.join("a"), data.join("b"), data.join("loose.txt")], true, algo);
    assert_eq!(st.added as usize, 2 * n + 1);
    assert_eq!(st.kind, JobKind::Add);
    Fixture { _tmp: tmp, data, db_path }
}

fn files_on_disk(dir: &Path) -> usize {
    let mut n = 0;
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        n += if p.is_dir() { files_on_disk(&p) } else { 1 };
    }
    n
}

fn entry_for(db: &Db, name: &str) -> hash_check::db::Entry {
    let ids = db.ids_filtered(Scope::All, name).unwrap();
    assert_eq!(ids.len(), 1, "expected exactly one entry matching {name}");
    db.entry(ids[0]).unwrap().unwrap()
}

#[test]
fn scopes_search_and_counts() {
    let fx = fixture(5, Algo::Blake3);
    let db = Db::open(&fx.db_path).unwrap();
    let roots = db.roots().unwrap();
    assert_eq!(roots.len(), 2);
    let (per_root, loose) = db.root_counts().unwrap();
    assert_eq!(loose, 1);
    assert!(roots.iter().all(|r| per_root[&r.id] == 5));
    assert_eq!(db.ids_filtered(Scope::All, "").unwrap().len(), 11);
    assert_eq!(db.ids_filtered(Scope::Root(roots[0].id), "").unwrap().len(), 5);
    assert_eq!(db.ids_filtered(Scope::Loose, "").unwrap().len(), 1);
    assert_eq!(db.ids_filtered(Scope::All, "F0003").unwrap().len(), 2, "search ignores ASCII case");
    assert_eq!(db.ids_filtered(Scope::Root(roots[1].id), "f0003").unwrap().len(), 1);
    // LIKE wildcards in the search text are taken literally.
    assert_eq!(db.ids_filtered(Scope::All, "%").unwrap().len(), 0);
    assert_eq!(db.ids_filtered(Scope::All, "f000_").unwrap().len(), 0);
}

#[test]
fn remove_entries_only_touches_the_database() {
    let fx = fixture(6, Algo::Xxh3_128);
    let before = files_on_disk(&fx.data);
    let db = Db::open(&fx.db_path).unwrap();
    let victims = db.ids_filtered(Scope::All, "f0002").unwrap();
    assert_eq!(victims.len(), 2);
    drop(db);

    let st = run_blocking(&fx.db_path, Job::RemoveEntries { ids: victims.clone() });
    assert_eq!((st.kind, st.phase, st.removed), (JobKind::Remove, Phase::Finished, 2));
    assert!(st.fatal.is_none());

    let db = Db::open(&fx.db_path).unwrap();
    assert_eq!(db.count_files().unwrap(), 11);
    assert!(victims.iter().all(|id| db.entry(*id).unwrap().is_none()));
    assert_eq!(db.roots().unwrap().len(), 2, "roots stay when single entries are removed");
    assert_eq!(files_on_disk(&fx.data), before, "Remove must not delete files");
    assert!(fx.data.join("a").join("f0002.bin").exists());
    assert!(db.integrity_check().unwrap());

    // Removing ids that are already gone is harmless.
    let st = run_blocking(&fx.db_path, Job::RemoveEntries { ids: victims });
    assert_eq!(st.removed, 0);
}

#[test]
fn remove_root_forgets_its_entries_and_nothing_else() {
    let fx = fixture(4, Algo::Sha256);
    let before = files_on_disk(&fx.data);
    let db = Db::open(&fx.db_path).unwrap();
    let roots = db.roots().unwrap();
    let (gone, kept) = (roots[0].clone(), roots[1].clone());
    drop(db);

    let st = run_blocking(&fx.db_path, Job::RemoveRoot { root_id: gone.id });
    assert_eq!(st.removed, 4);

    let db = Db::open(&fx.db_path).unwrap();
    let roots = db.roots().unwrap();
    assert_eq!(roots, vec![kept.clone()]);
    assert_eq!(db.ids_filtered(Scope::Root(kept.id), "").unwrap().len(), 4);
    assert_eq!(db.ids_filtered(Scope::Loose, "").unwrap().len(), 1);
    assert_eq!(db.count_files().unwrap(), 5);
    assert_eq!(files_on_disk(&fx.data), before, "removing a root must not delete files");
    drop(db);

    // The folder can be added again afterwards.
    let st = add_blocking(&fx.db_path, &[PathBuf::from(&gone.path)], true, Algo::Blake3);
    assert_eq!(st.added, 4);
}

#[test]
fn update_replaces_hash_and_keeps_algorithm() {
    for algo in Algo::ALL {
        let fx = fixture(3, algo);
        let target = fx.data.join("a").join("f0001.bin");
        let db = Db::open(&fx.db_path).unwrap();
        let a_root = db.roots().unwrap()[0].id;
        let old = db
            .ids_filtered(Scope::Root(a_root), "f0001")
            .unwrap()
            .into_iter()
            .map(|id| db.entry(id).unwrap().unwrap())
            .next()
            .unwrap();
        let untouched_before = entry_for(&db, "loose.txt");
        // Pretend an earlier verification flagged it.
        db.record_result(old.id, Status::PossiblyCorrupted, Some("hash mismatch"), 77).unwrap();
        assert_eq!(db.count_with_status(&[old.id, untouched_before.id], Status::PossiblyCorrupted).unwrap(), 1);
        drop(db);

        // The user edits the file, then asks for Update.
        fs::write(&target, b"edited on purpose, now longer").unwrap();
        let st = run_blocking(&fx.db_path, Job::Update { ids: vec![old.id] });
        assert_eq!((st.kind, st.updated, st.problem_count), (JobKind::Update, 1, 0));

        let db = Db::open(&fx.db_path).unwrap();
        let new = db.entry(old.id).unwrap().unwrap();
        assert_eq!(new.hash, hash_bytes(algo, b"edited on purpose, now longer"));
        assert_ne!(new.hash, old.hash);
        assert_eq!(new.algo, algo, "Update keeps the entry's algorithm");
        assert_eq!(new.size, 29);
        assert_eq!(new.status, Status::Ok);
        assert_eq!(new.last_error, None);
        assert_eq!(new.first_hashed, old.first_hashed, "first-hashed time is history, not rewritten");
        assert_eq!((new.path.as_str(), new.root_id), (old.path.as_str(), old.root_id));
        assert_eq!(entry_for(&db, "loose.txt"), untouched_before, "unselected entries are not updated");
        assert_eq!(fs::read(&target).unwrap(), b"edited on purpose, now longer");
    }
}

#[test]
fn update_of_a_missing_file_keeps_the_old_hash() {
    let fx = fixture(2, Algo::Blake3);
    // The file disappears (deleted by the test, standing in for the user).
    let own_copy = fx.data.join("loose.txt");
    let db = Db::open(&fx.db_path).unwrap();
    let old = entry_for(&db, "loose.txt");
    drop(db);
    fs::remove_file(&own_copy).unwrap();

    let st = run_blocking(&fx.db_path, Job::Update { ids: vec![old.id, 987_654] });
    assert_eq!(st.updated, 0);
    assert_eq!(st.problem_count, 1, "{:?}", st.problems);
    assert!(st.fatal.is_none());
    let db = Db::open(&fx.db_path).unwrap();
    assert_eq!(db.entry(old.id).unwrap().unwrap(), old, "a failed update must leave the entry as it was");
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn pause_holds_the_job_and_resume_finishes_it() {
    let fx = fixture(40, Algo::Blake3);
    let ids = Db::open(&fx.db_path).unwrap().all_ids().unwrap();

    // Start paused, so the test does not race the worker.
    let shared = Arc::new(Shared::new(JobKind::Update));
    shared.set_paused(true);
    engine::start_on(shared.clone(), fx.db_path.clone(), Job::Update { ids: ids.clone() }, Box::new(|| {}));
    std::thread::sleep(Duration::from_millis(300));
    let s = shared.snapshot();
    assert!(s.paused);
    assert_ne!(s.phase, Phase::Finished);
    assert_eq!((s.files_done, s.updated), (0, 0), "a paused job must not make progress");

    // While paused the database is not locked: another connection can write.
    let db = Db::open(&fx.db_path).unwrap();
    db.begin().unwrap();
    db.record_result(ids[0], Status::Ok, None, 1).unwrap();
    db.commit().unwrap();
    drop(db);

    shared.set_paused(false);
    wait_until("the resumed job to finish", || shared.snapshot().phase == Phase::Finished);
    let s = shared.snapshot();
    assert_eq!(s.updated as usize, ids.len());
    assert!(!s.cancelled && !s.paused);
    assert!(s.active_hashing_secs() < 60.0);
}

#[test]
fn cancel_works_while_paused_and_keeps_finished_work() {
    let fx = fixture(40, Algo::Sha256);
    let extra = fx.data.join("later");
    fs::create_dir_all(&extra).unwrap();
    for i in 0..200 {
        fs::write(extra.join(format!("n{i:03}")), format!("new {i}")).unwrap();
    }
    let before = Db::open(&fx.db_path).unwrap().count_files().unwrap();

    let shared = Arc::new(Shared::new(JobKind::Add));
    shared.set_paused(true);
    engine::start_on(
        shared.clone(),
        fx.db_path.clone(),
        Job::Add { inputs: vec![extra.clone()], recursive: true, algo: Algo::Sha256 },
        Box::new(|| {}),
    );
    std::thread::sleep(Duration::from_millis(100));
    shared.request_cancel();
    wait_until("the cancelled job to stop", || shared.snapshot().phase == Phase::Finished);
    let s = shared.snapshot();
    assert!(s.cancelled);
    assert!(s.fatal.is_none());

    let db = Db::open(&fx.db_path).unwrap();
    let after = db.count_files().unwrap();
    assert_eq!(after, before + s.added, "exactly the files reported as added are stored");
    assert!(s.added < 200, "the job was cancelled before it could finish");
    assert!(db.integrity_check().unwrap());
}
