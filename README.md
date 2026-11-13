# hash-check

A desktop GUI file-integrity checker. It stores a hash of each file you add
and later compares the files against those hashes, to detect changes and
bit rot (silent corruption where the content changes but size and modified
time do not).

Primary target: Windows 11 x64. The code also builds and passes its tests
on Linux.

> **Status: 0.1.0, first milestone.** You can add files and folders, hash
> them and browse the stored entries. Verify, Update, Remove, reports and
> performance tuning arrive in the next milestones (see [Roadmap](#roadmap)).

## The read-only guarantee

hash-check never modifies, renames, moves or deletes your files.

- Files are opened read-only, sharing read, write and delete access, so
  other programs are never blocked.
- All access to user files goes through one module, [`src/fsread.rs`](src/fsread.rs),
  which exposes only "read" and "get metadata".
- The only place the program writes is its own folder (see
  [`src/appdata.rs`](src/appdata.rs)).
- [`tests/readonly_audit.rs`](tests/readonly_audit.rs) fails the build if
  `fsread.rs` gains any call that could change the file system, or if any
  other module touches file-system APIs.

## Download

Tagged versions have a ready-made `hash-check.exe` on the
[Releases page](https://github.com/camilus123/hash-check/releases). It is a
single self-contained file: put it in a folder of its own that you can
write to (not `Program Files`) and run it.

## Using it

1. Pick the hash algorithm for new entries:
   - **BLAKE3** (default): fast, cryptographic, uses all cores.
   - **XXH3-128**: fastest, not cryptographic. Fine against bit rot, not
     against deliberate tampering.
   - **SHA-256**: for comparing with hashes from other tools.
2. Click **Add files…** or **Add folder…**, or drop files and folders onto
   the window. With **Include subfolders** ticked, a folder is added as a
   whole tree.
3. Watch progress at the bottom. **Cancel** stops at once and keeps
   everything hashed so far.

Adding a file that is already tracked does nothing: a stored hash is never
overwritten by Add.

## Where data is stored

Everything lives next to `hash-check.exe`:

| File | Content |
|---|---|
| `hash-check.db` | The SQLite database (plus `-wal`/`-shm` files while running) |
| `hash-check.db.backup-v<N>-<time>` | Automatic copy made before a schema upgrade |

Per file the database holds: full path, size, modified time, hash,
algorithm, time first hashed, time last verified and the last result. The
algorithm is stored per entry, so one database can mix algorithms.

## Building

Requires a current stable Rust toolchain (edition 2024, Rust 1.85 or
newer).

```
cargo build --release
```

The result is `target/release/hash-check.exe` (Windows) or
`target/release/hash-check` (Linux). On Windows the C runtime is linked
statically, so the `.exe` needs no redistributables.

## Tests

```
cargo test
```

- **Hashing correctness**: published vectors for each algorithm, plus
  digests of patterned data up to 9 MB computed with independent
  implementations (Python `hashlib`, the reference XXH3, the official
  BLAKE3 bindings); one-shot, streamed in odd chunk sizes, multithreaded,
  and through the file reader.
- **Database integrity**: round trips, mixed algorithms, rollback, a writer
  process that is killed mid-run, backup before migration, failed
  migration, refusal of newer databases.
- **Add job**: Unicode names, a path longer than 260 characters, links not
  followed, files changing mid-hash, and a check that the user's files are
  byte-for-byte and timestamp-for-timestamp unchanged afterwards.
- **Read-only audit**: described above.

Tests create and alter only files they made themselves in a temporary
directory.

## Benchmarks

Not measured yet. A built-in benchmark and numbers for each algorithm on
SSD and HDD are part of milestone 0.4.0.

## Roadmap

| Version | Content |
|---|---|
| 0.1.0 | Core hashing, database, tests, CI, minimal window (this version) |
| 0.2.0 | Full GUI: roots panel, Remove, Update with confirmations, Pause |
| 0.3.0 | Verify (OK / Changed / Possibly corrupted / Missing / Unreadable / New), filters, CSV and text reports |
| 0.4.0 | Per-drive threading, sequential HDD mode, drive settings, benchmark |
| 1.0.0 | Robustness pass and benchmark results |

## Licence

GNU General Public License v3. See [LICENSE](LICENSE).
