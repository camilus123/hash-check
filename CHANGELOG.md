# Changelog

Versions are `x.y.z`: `x` for major changes, `y` for added features, `z` for
bug fixes. `1.0.0` will be the first release that meets the whole project
brief.

## 0.1.1 (2026-11-13)

### Fixed
- Hovering over a path that did not fit its column showed two identical
  pop-ups with the full path instead of one.

## 0.1.0 (2026-11-13)

First milestone: core hashing and database, with a minimal window.

### Added
- Hashing with BLAKE3 (default), XXH3-128 and SHA-256, each using the
  fastest CPU instructions available at runtime.
- Read-only file access confined to one module (`src/fsread.rs`), with an
  automated audit test that fails if any other code touches user files or
  if that module gains a call that could change them.
- SQLite database stored next to the executable: schema version, WAL
  journal with durable commits, automatic backup before any schema
  migration, refusal to open a database from a newer version.
- Add files, folders or whole folder trees (buttons or drag-and-drop);
  added folders are remembered as roots.
- Hashing runs on a background thread with progress (files, bytes, current
  and average MB/s, ETA, current file) and Cancel. Results are committed in
  batches, so a cancelled or killed run keeps what it finished.
- Files whose size or modified time changes while being hashed are
  discarded and reported.
- Links and junctions are not followed.
- List of tracked entries that stays fast with millions of rows.
- Version shown in the window title and the About box.
- GitHub Actions: tests on Windows and Linux, Windows `.exe` built on every
  push and published as a release whenever the version number changes.

### Not yet implemented
Remove, Update, Verify and reports, Pause, per-drive threading and drive
detection, settings, benchmarks. See the roadmap in the README.
