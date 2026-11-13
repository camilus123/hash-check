# Changelog

Versions are `x.y.z`: `x` for major changes, `y` for added features, `z` for
bug fixes. `1.0.0` will be the first release that meets the whole project
brief.

## 0.4.1 (2026-11-13)

### Fixed
- Nothing was ever copied to the clipboard: "Copy as Markdown" in the
  benchmark window, "Copy full path", "Copy hash" and "Copy path" all did
  nothing, because clipboard support had been left out of the build.
- For the same reason the link in the About box did not open.

## 0.4.0 (2026-11-13)

Fourth milestone: performance.

### Added
- Hashing is scheduled per physical drive, all drives at the same time.
  SSDs are read with several files in parallel; hard disks are read one
  file at a time, with BLAKE3 spreading that one stream over all cores.
  Two volumes on the same physical disk share one queue.
- Drive detection on Windows (SSD or HDD, NVMe/SATA/USB, model, physical
  disk number), using a volume handle with no access rights.
- "Drives" list in the left panel with a per-drive setting: Auto, or a
  fixed number of files to read in parallel.
- Optional direct I/O ("Read past the file cache" in the Tools menu), so
  verified data really comes from the drive and not from memory. Falls
  back to normal reads where a drive does not support it.
- Built-in benchmark (Tools menu): reads files you choose, read-only, once
  per algorithm past the file cache, plus in-memory figures for the CPU
  alone. Results can be copied as a Markdown table.
- The status panel shows how each drive is being read.

### Changed
- Add, Update, Verify and "add new" all run on the new scheduler. Large
  files (16 MB and up) use multithreaded BLAKE3 even when several files
  are read in parallel.
- Read buffers are aligned for direct I/O.

## 0.3.0 (2026-11-13)

Third milestone: verification.

### Added
- Verify: everything, one root, or the selected entries. Each file is
  re-hashed with the algorithm its entry was created with and classified:
  - **OK**: hash matches (a changed timestamp alone is still OK).
  - **Changed**: hash differs and size and/or modified time differ.
  - **Possibly corrupted**: hash differs although size and modified time
    are identical. Shown in red, with a red banner across the window that
    stays (also after a restart) until dismissed.
  - **Missing**: the file no longer exists.
  - **Unreadable**: I/O error or access denied, with the OS error text.
  - **New**: a file under a tracked root that is not in the database, with
    a button to add all or the selected ones.
- Verify never changes a stored hash. For problem files the stored size
  and modified time are kept too, so repeating the check gives the same
  answer; restoring the original content brings a file back to OK.
- Entries on a drive that is not connected are reported as "drive not
  available" and left untouched, not marked missing.
- Summary after each verification, live counts while it runs, filter chips
  per classification with counts, "Last verification only" filter, and a
  Details column explaining each finding.
- Export of the last verification as CSV or plain text, problems first,
  into `reports\` next to the program or to a location you pick.
  hash-check refuses to overwrite an existing file when exporting.

### Changed
- Database schema version 2 (verification runs and the list of new files).
  An existing database is upgraded automatically after a backup copy
  (`hash-check.db.backup-v1-<time>`) has been written next to it.

## 0.2.2 (2026-11-13)

### Fixed
- On Windows, full-width punctuation in file names ("：", "？", "｜" and
  similar, as written by download tools) still showed as empty boxes: the
  0.2.1 fix only covered characters that belong to a specific script. A
  second fallback now searches the fonts that ship with Windows for any
  character the others cannot draw.

## 0.2.1 (2026-11-13)

### Fixed
- The entry list could not be scrolled sideways when its columns were wider
  than the window. It now has a horizontal scroll bar (Shift+wheel and
  touchpad scrolling work too).
- Adding a folder and cancelling before any file was hashed left an empty
  root behind. A stopped Add now removes roots it created that received no
  entries; roots that already existed, or that got at least one entry, stay.
- Characters missing from the built-in font (full-width punctuation such as
  "：", Chinese, Japanese, Korean) showed as empty boxes in file names.
  Fonts installed with the operating system are now used as fallbacks.

## 0.2.0 (2026-11-13)

Second milestone: the full window for managing entries.

### Added
- Left panel listing every tracked root with its entry count, plus "All
  entries" and "Single files"; click one to show only its entries.
- Remove: delete the selected entries, or a whole root with its entries,
  from the database after a confirmation. Nothing on disk is touched.
- Update: re-hash the selected entries and replace their stored hashes,
  after a confirmation. If any of them is flagged "possibly corrupted" a
  red warning appears and must be acknowledged first. Each entry keeps the
  algorithm it was created with.
- Pause and Resume for hashing jobs. A paused job holds no database lock,
  and paused time is left out of the average speed and ETA.
- Row selection as in Explorer: click, Ctrl+click, Shift+click, Ctrl+A,
  "Select all" and "Select none".
- Search box that filters the list by any part of the path.
- Right-click menu on rows (copy full path, copy hash) and on roots.

### Changed
- The path is shown as two columns, Name and Folder, so the file name stays
  visible for long paths.
- Removing runs on the background thread like hashing does.

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
