//! Database integrity: schema, round trips, crash safety, migrations.

use hash_check::db::{Db, DbError, MIGRATIONS, NewEntry, SCHEMA_VERSION, Status};
use hash_check::hasher::{Algo, hash_bytes};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

fn insert(db: &Db, path: &str, algo: Algo, content: &[u8]) -> bool {
    let hash = hash_bytes(algo, content);
    db.insert_entry(&NewEntry {
        path,
        root_id: None,
        size: content.len() as u64,
        mtime_ns: 1_700_000_000_123_456_789,
        algo,
        hash: &hash,
        now: 1_700_000_100,
    })
    .unwrap()
}

#[test]
fn fresh_database_has_current_schema_and_is_sound() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    assert!(db.backup_made.is_none(), "a new database needs no backup");
    assert!(db.integrity_check().unwrap());
    assert_eq!(db.count_files().unwrap(), 0);
}

#[test]
fn entries_round_trip_with_mixed_algorithms() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let db = Db::open(&path).unwrap();
        let root = db.add_root("/data/photos", true, 5).unwrap();
        assert_eq!(db.add_root("/data/photos", true, 9).unwrap(), root, "same root, same id");
        db.begin().unwrap();
        for (i, algo) in Algo::ALL.into_iter().enumerate() {
            let hash = hash_bytes(algo, b"content");
            assert!(
                db.insert_entry(&NewEntry {
                    path: &format!("/data/photos/ünï-{i}-日本語.raw"),
                    root_id: Some(root),
                    size: u32::MAX as u64 * 100 + i as u64, // > 4 GB
                    mtime_ns: -5 + i as i64,                // before 1970 is legal
                    algo,
                    hash: &hash,
                    now: 42,
                })
                .unwrap()
            );
        }
        db.commit().unwrap();
    }
    // Reopen: everything must have survived exactly.
    let db = Db::open(&path).unwrap();
    let ids = db.all_ids().unwrap();
    assert_eq!(ids.len(), 3);
    for (i, id) in ids.iter().enumerate() {
        let e = db.entry(*id).unwrap().unwrap();
        let algo = Algo::ALL[i];
        assert_eq!(e.path, format!("/data/photos/ünï-{i}-日本語.raw"));
        assert_eq!(e.algo, algo);
        assert_eq!(e.hash, hash_bytes(algo, b"content"));
        assert_eq!(e.hash.len(), algo.digest_len());
        assert_eq!(e.size, u32::MAX as u64 * 100 + i as u64);
        assert_eq!(e.mtime_ns, -5 + i as i64);
        assert_eq!((e.first_hashed, e.last_verified), (42, 42));
        assert_eq!(e.status, Status::Ok);
        assert!(e.root_id.is_some());
    }
    let roots = db.roots().unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!((roots[0].path.as_str(), roots[0].recursive, roots[0].added_at), ("/data/photos", true, 5));
    assert!(db.entry(999_999).unwrap().is_none());
}

#[test]
fn adding_never_overwrites_a_stored_hash() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    assert!(insert(&db, "/a", Algo::Blake3, b"original"));
    assert!(!insert(&db, "/a", Algo::Sha256, b"different"), "second insert must be refused");
    let e = db.entry(db.all_ids().unwrap()[0]).unwrap().unwrap();
    assert_eq!(e.algo, Algo::Blake3);
    assert_eq!(e.hash, hash_bytes(Algo::Blake3, b"original"));
    assert!(db.file_exists("/a").unwrap());
    assert!(!db.file_exists("/A").unwrap());
}

#[test]
fn rolled_back_batch_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let db = Db::open(&path).unwrap();
    db.begin().unwrap();
    insert(&db, "/kept", Algo::Blake3, b"1");
    db.commit().unwrap();
    db.begin().unwrap();
    insert(&db, "/lost", Algo::Blake3, b"2");
    drop(db); // connection closed mid-transaction
    let db = Db::open(&path).unwrap();
    assert!(db.file_exists("/kept").unwrap());
    assert!(!db.file_exists("/lost").unwrap());
    assert!(db.integrity_check().unwrap());
}

const CRASH_ENV: &str = "HASH_CHECK_CRASH_TEST_DB";
const CRASH_BATCH: u64 = 100;

/// Helper process for `killed_writer_leaves_a_sound_database`: commits
/// batches forever until it is killed. Does nothing when run normally.
#[test]
fn crash_child() {
    let Ok(path) = std::env::var(CRASH_ENV) else { return };
    let db = Db::open(Path::new(&path)).unwrap();
    let mut n = 0u64;
    loop {
        db.begin().unwrap();
        for _ in 0..CRASH_BATCH {
            insert(&db, &format!("/crash/{n:012}"), Algo::Xxh3_128, &n.to_le_bytes());
            n += 1;
        }
        db.commit().unwrap();
        if n == CRASH_BATCH * 3 {
            println!("READY");
        }
    }
}

#[test]
fn killed_writer_leaves_a_sound_database() {
    let dir = tempfile::tempdir().unwrap();
    for round in 0..3u64 {
        // A fresh database per round, killed at a different moment each time.
        let round_path = dir.path().join(format!("crash-{round}.db"));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
            .env(CRASH_ENV, &round_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(
            lines.by_ref().any(|l| l.unwrap().contains("READY")),
            "writer process never became ready"
        );
        // Let it get somewhere into a later batch, then kill it outright.
        std::thread::sleep(std::time::Duration::from_millis(40 + 35 * round));
        child.kill().unwrap();
        child.wait().unwrap();

        let db = Db::open(&round_path).unwrap();
        assert!(db.integrity_check().unwrap(), "database damaged by kill (round {round})");
        let count = db.count_files().unwrap();
        assert!(count >= CRASH_BATCH * 3, "committed batches were lost: {count}");
        assert_eq!(count % CRASH_BATCH, 0, "a partial batch became visible: {count}");
        // Every surviving row is intact.
        let ids = db.all_ids().unwrap();
        for id in [ids[0], ids[ids.len() / 2], ids[ids.len() - 1]] {
            let e = db.entry(id).unwrap().unwrap();
            let n: u64 = e.path.trim_start_matches("/crash/").parse().unwrap();
            assert_eq!(e.hash, hash_bytes(Algo::Xxh3_128, &n.to_le_bytes()));
        }
        // The database stays writable afterwards.
        assert!(insert(&db, "/after-crash", Algo::Blake3, b"x"));
    }
}

#[test]
fn migration_backs_up_first_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let v1: &[&str] = &MIGRATIONS[..1];
    {
        let db = Db::open_with(&path, v1).unwrap();
        insert(&db, "/precious", Algo::Sha256, b"data");
    }
    let v2: Vec<&str> = vec![MIGRATIONS[0], "ALTER TABLE files ADD COLUMN note TEXT"];
    let db = Db::open_with(&path, &v2).unwrap();
    assert_eq!(db.schema_version().unwrap(), 2);
    assert!(db.file_exists("/precious").unwrap());
    assert!(db.integrity_check().unwrap());

    let backup = db.backup_made.clone().expect("a backup must be made before migrating");
    assert!(backup.exists());
    assert_eq!(backup.parent(), path.parent(), "backup must sit next to the database");
    assert!(backup.file_name().unwrap().to_string_lossy().starts_with("t.db.backup-v1-"));
    let old = Db::open_with(&backup, v1).unwrap();
    assert_eq!(old.schema_version().unwrap(), 1, "backup must hold the old schema");
    assert!(old.file_exists("/precious").unwrap());
    assert!(old.integrity_check().unwrap());

    // Opening again at the same version migrates nothing and backs up nothing.
    drop(db);
    assert!(Db::open_with(&path, &v2).unwrap().backup_made.is_none());
}

#[test]
fn failed_migration_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let v1: &[&str] = &MIGRATIONS[..1];
    {
        let db = Db::open_with(&path, v1).unwrap();
        insert(&db, "/precious", Algo::Blake3, b"data");
    }
    let broken: Vec<&str> =
        vec![MIGRATIONS[0], "ALTER TABLE files ADD COLUMN note TEXT; THIS IS NOT SQL;"];
    assert!(Db::open_with(&path, &broken).is_err());
    let db = Db::open_with(&path, v1).unwrap();
    assert_eq!(db.schema_version().unwrap(), 1, "version must not advance on failure");
    assert!(db.file_exists("/precious").unwrap());
    assert!(db.integrity_check().unwrap());
}

#[test]
fn database_from_a_newer_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let future: Vec<&str> = vec![MIGRATIONS[0], "CREATE TABLE from_the_future (x)"];
    drop(Db::open_with(&path, &future).unwrap());
    match Db::open(&path) {
        Err(DbError::NewerSchema { found: 2, supported }) => assert_eq!(supported, SCHEMA_VERSION),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a newer database must not be opened"),
    }
    // Still intact for the newer program.
    assert_eq!(Db::open_with(&path, &future).unwrap().schema_version().unwrap(), 2);
}

#[test]
fn status_ids_are_stable() {
    let expect = [
        (Status::Ok, 1),
        (Status::Changed, 2),
        (Status::PossiblyCorrupted, 3),
        (Status::Missing, 4),
        (Status::Unreadable, 5),
    ];
    for (s, id) in expect {
        assert_eq!(s.id(), id);
        assert_eq!(Status::from_id(id), Some(s));
    }
}
