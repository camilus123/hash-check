//! The add job end to end, on files this test creates in a temp directory.

use hash_check::db::{Db, Status};
use hash_check::engine::add_blocking;
use hash_check::hasher::{Algo, hash_bytes};
use hash_check::hashfile::{BUF_SIZE, HashOutcome, hash_file};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Content, size and modified time of every file under `dir`.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let m = fs::symlink_metadata(e.path()).unwrap();
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                out.insert(e.path(), (fs::read(e.path()).unwrap(), m.modified().unwrap()));
            }
        }
    }
    out
}

fn make_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut put = |rel: &str, content: Vec<u8>| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, &content).unwrap();
        files.insert(rel.to_string(), content);
    };
    put("a.txt", b"alpha".to_vec());
    put("empty.bin", Vec::new());
    put("sub/b.txt", b"beta".to_vec());
    put("sub/deeper/c.bin", vec![7u8; 100_000]);
    put("ünïcödé/日本語 файл 🐈.txt", "Mici".as_bytes().to_vec());
    // A path well beyond the classic 260-character Windows limit.
    let long_dir = (0..8).map(|i| format!("{i}-{}", "x".repeat(40))).collect::<Vec<_>>().join("/");
    put(&format!("{long_dir}/long-path-file.dat"), b"deep".to_vec());
    files
}

#[test]
fn add_tree_hashes_everything_and_touches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let appdir = tmp.path().join("app");
    fs::create_dir_all(&appdir).unwrap();
    let files = make_tree(&data);
    assert!(
        files.keys().any(|k| data.join(k).to_string_lossy().len() > 300),
        "test tree must include a long path"
    );
    let before = snapshot(&data);
    let db_path = appdir.join("t.db");

    for algo in Algo::ALL {
        let db_path = appdir.join(format!("t-{}.db", algo.id()));
        let st = add_blocking(&db_path, &[data.clone()], true, algo);
        assert!(st.fatal.is_none(), "{:?}", st.fatal);
        assert_eq!(st.problem_count, 0, "{:?}", st.problems);
        assert_eq!(st.added, files.len() as u64);
        assert_eq!(st.files_done, files.len() as u64);

        let db = Db::open(&db_path).unwrap();
        let roots = db.roots().unwrap();
        assert_eq!(roots.len(), 1);
        assert!(roots[0].recursive);
        let root = PathBuf::from(&roots[0].path);
        let ids = db.all_ids().unwrap();
        assert_eq!(ids.len(), files.len());
        for id in ids {
            let e = db.entry(id).unwrap().unwrap();
            let rel = Path::new(&e.path).strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            let content = files.get(&rel).unwrap_or_else(|| panic!("unexpected entry {rel}"));
            assert_eq!(e.hash, hash_bytes(algo, content), "{rel}");
            assert_eq!(e.size, content.len() as u64);
            assert_eq!(e.algo, algo);
            assert_eq!(e.status, Status::Ok);
            assert_eq!(e.root_id, Some(roots[0].id));
            assert!(!e.path.starts_with(r"\\?\"), "stored paths must be in plain form: {}", e.path);
        }
    }

    // Adding again: nothing new, nothing overwritten.
    let first = add_blocking(&db_path, &[data.clone()], true, Algo::Blake3);
    assert_eq!(first.added, files.len() as u64);
    let again = add_blocking(&db_path, &[data.clone()], true, Algo::Sha256);
    assert_eq!(again.added, 0);
    assert_eq!(again.already_tracked, files.len() as u64);
    let db = Db::open(&db_path).unwrap();
    assert_eq!(db.count_files().unwrap(), files.len() as u64);
    assert_eq!(db.roots().unwrap().len(), 1);

    // THE RULE: user files are byte-for-byte and timestamp-for-timestamp as before,
    // and nothing was created in the user's tree.
    assert_eq!(snapshot(&data), before, "hash-check changed user files");
}

#[test]
fn non_recursive_add_and_loose_files() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let files = make_tree(&data);
    let db_path = tmp.path().join("t.db");

    let st = add_blocking(&db_path, &[data.clone()], false, Algo::Xxh3_128);
    let top_level = files.keys().filter(|k| !k.contains('/')).count() as u64;
    assert_eq!(st.added, top_level);
    let db = Db::open(&db_path).unwrap();
    assert!(!db.roots().unwrap()[0].recursive);
    drop(db);

    // A single file is a loose entry, not a root; the same file given twice is hashed once.
    let loose = data.join("sub").join("b.txt");
    let st = add_blocking(&db_path, &[loose.clone(), loose.clone()], true, Algo::Xxh3_128);
    assert_eq!((st.added, st.files_total), (1, 1));
    let db = Db::open(&db_path).unwrap();
    assert_eq!(db.roots().unwrap().len(), 1);
    let loose_entries = db
        .all_ids()
        .unwrap()
        .into_iter()
        .map(|id| db.entry(id).unwrap().unwrap())
        .filter(|e| e.root_id.is_none())
        .count();
    assert_eq!(loose_entries, 1);
}

#[test]
fn missing_input_is_reported_not_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    let st = add_blocking(&tmp.path().join("t.db"), &[tmp.path().join("does-not-exist")], true, Algo::Blake3);
    assert!(st.fatal.is_none());
    assert_eq!(st.added, 0);
    assert_eq!(st.problem_count, 1);
}

#[cfg(unix)]
#[test]
fn links_are_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(data.join("real.txt"), b"real").unwrap();
    fs::write(outside.join("secret.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(&outside, data.join("dir-link")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), data.join("file-link")).unwrap();
    std::os::unix::fs::symlink(&data, data.join("loop")).unwrap();

    let db_path = tmp.path().join("t.db");
    let st = add_blocking(&db_path, &[data.clone()], true, Algo::Blake3);
    assert_eq!(st.added, 1);
    assert_eq!(st.skipped_links, 3);
    assert_eq!(st.problem_count, 0);
}

#[test]
fn file_changed_while_hashing_is_discarded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("growing.bin");
    fs::write(&path, vec![1u8; BUF_SIZE * 2 + 10]).unwrap();
    let mut buf = Vec::new();
    let mut appended = false;
    let out = hash_file(&path, Algo::Blake3, &mut buf, true, &mut |_| {
        if !appended {
            // Another program appends while we are part-way through.
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"more").unwrap();
            appended = true;
        }
        true
    })
    .unwrap();
    assert_eq!(out, HashOutcome::ChangedDuringHash);

    // Untouched during hashing: a normal result.
    let out = hash_file(&path, Algo::Blake3, &mut buf, true, &mut |_| true).unwrap();
    assert!(matches!(out, HashOutcome::Done { size, .. } if size == (BUF_SIZE * 2 + 14) as u64));
}

#[test]
fn hashing_can_be_cancelled_mid_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("big.bin");
    fs::write(&path, vec![9u8; BUF_SIZE * 3]).unwrap();
    let mut buf = Vec::new();
    let mut blocks = 0;
    let out = hash_file(&path, Algo::Sha256, &mut buf, false, &mut |_| {
        blocks += 1;
        false
    })
    .unwrap();
    assert_eq!(out, HashOutcome::Cancelled);
    assert_eq!(blocks, 1, "must stop after the first block");
}
