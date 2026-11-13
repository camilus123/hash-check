//! Enforces the one absolute rule at build time: hash-check never modifies
//! user files.
//!
//! - `src/fsread.rs` is the only module that may open or list user files,
//!   and it must not contain any call that can change the file system.
//! - `src/appdata.rs` is the only module that may create or remove files
//!   (inside the app's own directory).
//! - No other module may use file-system APIs at all. (The database file is
//!   written by SQLite, at a path that comes from `appdata`.)

use std::path::Path;

/// Anything that creates, changes, moves or deletes.
const MUTATING: &[&str] = &[
    "File::create",
    "create_new",
    "fs::write",
    "remove_file",
    "remove_dir",
    "rename(",
    "fs::copy",
    "create_dir",
    "set_permissions",
    "set_len",
    "set_modified",
    "set_times",
    "hard_link(",
    "::symlink(",
    "symlink_file",
    "symlink_dir",
    ".write(",
    ".write_all(",
    ".append(",
    ".truncate(true",
    ".create(true",
    "DeleteFile",
    "MoveFile",
    "FILE_FLAG_DELETE_ON_CLOSE",
    "GENERIC_WRITE",
    "access_mode",
];

/// Any direct file-system access.
const FS_ACCESS: &[&str] = &[
    "std::fs",
    "fs::",
    "File::",
    "OpenOptions",
    "read_dir",
    "read_to_string",
    "tokio::fs",
    "memmap",
    "Mmap",
];

fn code_lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
    src.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .map(|(i, l)| (i + 1, l))
}

#[test]
fn only_fsread_touches_user_files_and_only_to_read() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    let mut violations = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        assert!(
            path.is_file(),
            "audit assumes a flat src/ directory; extend it for {path:?}"
        );
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        checked += 1;
        let forbidden: Vec<&str> = match name.as_str() {
            "appdata.rs" => vec![],
            "fsread.rs" => MUTATING.to_vec(),
            _ => MUTATING.iter().chain(FS_ACCESS).copied().collect(),
        };
        for (n, line) in code_lines(&text) {
            for token in &forbidden {
                // SQL and hasher method names are not file access.
                let allowed =
                    (*token == ".write(" && name == "hasher.rs" && line.contains("h.write(data)"))
                        || (*token == "fs::" && line.contains("crate::fsread::"));
                if line.contains(token) && !allowed {
                    violations.push(format!("{name}:{n}: `{token}` in: {}", line.trim()));
                }
            }
        }
    }
    assert!(checked >= 8, "expected to audit all modules, saw {checked}");
    assert!(
        violations.is_empty(),
        "read-only rule violated:\n{}",
        violations.join("\n")
    );
}

#[test]
fn fsread_opens_files_read_only() {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fsread.rs"))
        .unwrap();
    let opens: Vec<_> = code_lines(&text)
        .filter(|(_, l)| l.contains("OpenOptions::new()"))
        .collect();
    assert_eq!(
        opens.len(),
        1,
        "fsread must have exactly one place that opens files"
    );
    assert!(text.contains("o.read(true);"));
    assert!(text.contains("FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE"));
}

/// Drive detection talks to Windows directly. It must be the only place
/// that does, and its volume handle must carry no access rights at all.
#[test]
fn drive_detection_opens_volumes_without_any_access_rights() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        if name != "drives.rs" {
            // fsread may ask Windows one read-only question about an open
            // handle (is this file a hard link?); nothing else may call out.
            let allowed = if name == "fsread.rs" { 1 } else { 0 };
            assert_eq!(
                text.matches("extern \"system\"").count(),
                allowed,
                "{name}: unexpected Windows API declarations"
            );
            if name == "fsread.rs" {
                assert!(text.contains("fn GetFileInformationByHandle("));
                let declared = code_lines(&text)
                    .filter(|(_, l)| l.trim_start().starts_with("fn Get"))
                    .count();
                assert_eq!(
                    declared, 1,
                    "fsread may declare exactly one Windows function"
                );
            }
            for token in ["CreateFileW", "DeviceIoControl"] {
                assert!(
                    !text.contains(token),
                    "{name} must not call the Windows API directly ({token})"
                );
            }
            continue;
        }
        let calls: Vec<_> = code_lines(&text)
            .filter(|(_, l)| l.contains("CreateFileW("))
            .collect();
        assert_eq!(
            calls.len(),
            2,
            "one declaration and one call of CreateFileW expected"
        );
        assert!(text.contains("const NO_ACCESS: u32 = 0;"));
        let call_at = text.rfind("CreateFileW(").unwrap();
        let args = &text[call_at..call_at + 200];
        assert!(
            args.contains("NO_ACCESS"),
            "the volume must be opened with no access rights: {args}"
        );
        for token in [
            "GENERIC_",
            "FILE_WRITE",
            "FILE_READ_DATA",
            "0x4000_0000",
            "0x8000_0000",
            "WriteFile",
            "FSCTL_",
        ] {
            assert!(
                !text.contains(token),
                "drives.rs must stay query-only ({token})"
            );
        }
    }
}
