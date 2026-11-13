# hash-check

A desktop GUI file-integrity checker. It stores a hash of each file you add
and later compares the files against those hashes, to detect changes and
bit rot (silent corruption where the content changes but size and modified
time do not).

Primary target: Windows 11 x64. The code also builds and passes its tests
on Linux.

## The read-only guarantee

hash-check never modifies, renames, moves or deletes your files.

- Files are opened read-only, sharing read, write and delete access, so
  other programs are never blocked.
- All access to user files goes through one module, [`src/fsread.rs`](src/fsread.rs),
  which exposes only "read" and "get metadata".
- The only place the program writes is its own folder (see
  [`src/appdata.rs`](src/appdata.rs)). The single exception is a report
  you export to a location you pick yourself; that file is always created
  new, and an existing file is never overwritten.
- [`tests/readonly_audit.rs`](tests/readonly_audit.rs) fails the build if
  `fsread.rs` gains any call that could change the file system, or if any
  other module touches file-system APIs.

## Download

Tagged versions have a ready-made `hash-check.exe` on the
[Releases page](https://github.com/camilus123/hash-check/releases). It is a
single self-contained file: put it in a folder of its own that you can
write to (not `Program Files`) and run it.

## Using it

**Adding.** Pick the hash algorithm for new entries, then click
**Add files…** or **Add folder…**, or drop files and folders onto the
window. With **Include subfolders** ticked, a folder is added as a whole
tree. Each added folder is remembered as a root and appears in the left
panel.

- **BLAKE3** (default): fast, cryptographic, uses all cores.
- **XXH3-128**: fastest, not cryptographic. Fine against bit rot, not
  against deliberate tampering.
- **SHA-256**: for comparing with hashes from other tools.

Adding a file that is already tracked does nothing: a stored hash is never
overwritten by Add.

**Browsing.** Click a root in the left panel to see only its entries, or
type in the search box. Select rows with click, Ctrl+click, Shift+click or
Ctrl+A. Right-click a row to copy its path or hash.

**Update.** Select entries and click **Update…** to re-hash them and
replace the stored hashes. Use it for files you changed on purpose. If a
selected entry is flagged *possibly corrupted*, a warning must be
acknowledged first, because updating would accept the damaged content as
correct.

**Remove.** Select entries and click **Remove…**, or select a root and
click **Remove this root…**. Only database entries are removed; files on
disk are never touched.

**Verify.** Open the **Verify** menu and choose everything, the selected
root, or the selected entries. Every file is re-hashed and classified:

| Result | Meaning |
|---|---|
| OK | The hash matches. |
| Changed | The hash differs and so does the size and/or modified time. Probably a normal edit; use Update if you made it. |
| **Possibly corrupted** | The hash differs although size and modified time are identical. This is what bit rot looks like. A red banner stays on screen until you dismiss it. |
| Missing | The file no longer exists. |
| Unreadable | The file could not be read; the OS error is shown. A read error on a file that used to be fine is a warning sign. |
| New | A file inside a tracked folder that is not in the database. Use the **New** filter to add them. |

Verify never changes a stored hash. If a drive is not connected, its
entries are reported as "drive not available" and left as they were. Use
the chips above the list to filter by result, and **Export report…** to
save the last verification as CSV or text.

**While a job runs** the bottom panel shows files and bytes done, current
and average speed, ETA and the current file. **Pause** holds the job,
**Cancel** stops it and keeps everything finished so far.

## Where data is stored

Everything lives next to `hash-check.exe`:

| File | Content |
|---|---|
| `hash-check.db` | The SQLite database (plus `-wal`/`-shm` files while running) |
| `hash-check.db.backup-v<N>-<time>` | Automatic copy made before a schema upgrade |
| `reports\` | Exported verification reports (unless you pick another location) |
| `logs\hash-check.log` | A line or more per job: what ran, the result, every problem |

Settings (threads per drive, direct I/O) are stored inside the database.

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
- **Managing entries**: Remove (entries and roots) leaves files on disk
  alone, Update replaces the hash and keeps the algorithm, a failed Update
  leaves the entry as it was, Pause holds a job without locking the
  database, Cancel keeps finished work.
- **Verification**: every classification on real files (touched, edited,
  resized, bit-flipped with the timestamp restored, deleted, replaced by a
  folder, locked by another program, permission denied), new-file
  detection, an offline drive, cancelling, restoring a file, and a check
  that verifying leaves the files on disk untouched.
- **Reports**: CSV and text content and ordering, quoting of awkward file
  names, and refusal to overwrite an existing file.
- **Scheduling**: 1, 2, 8 and 32 threads and direct I/O all produce the
  same database; parallel and sequential Verify agree file by file; the
  benchmark changes nothing; drive detection runs on the test machine.
- **Robustness**: hard links read once, the job log, the database check,
  skipping of the recycle bin at a drive root.
- **Read-only audit**: described above, plus a check that drive detection
  opens volumes with no access rights.

Tests create and alter only files they made themselves in a temporary
directory.

## Performance

Work is scheduled per physical drive, and all drives run at the same time:

- **SSD**: several files are read in parallel (NVMe: half the logical
  CPUs, at most 16; SATA: 4). Files of 16 MB and more additionally use
  BLAKE3's multithreaded mode.
- **Hard disk**: one file at a time, in path order, because parallel reads
  make the head seek back and forth. BLAKE3 spreads that one stream over
  all cores; XXH3 and SHA-256 hash it on one core, which is still far
  faster than a disk can deliver data.
- **Unknown or removable**: treated like a hard disk.

The **Drives** list in the left panel shows what was detected and lets you
override the number of parallel files per drive. Reads bypass the operating system's file cache (direct I/O), so verified
data really comes from the drive; **Tools > Read past the file cache**
switches that off.

All three algorithms pick the fastest instructions the CPU offers at
runtime (AVX2/SSE4.1 for BLAKE3 and XXH3, SHA extensions for SHA-256),
with portable fallbacks.

## Benchmarks

Measured with the built-in benchmark (**Tools > Benchmark…**) of version
0.4.1 on a Ryzen 9 5950X (16 cores, 32 threads, AVX2 and SHA extensions),
64 GB RAM, Windows 11. Each figure is MB/s over about 8 GB of existing
files, read past the file cache.

| Drive | Connection | How it is read | BLAKE3 | XXH3-128 | SHA-256 |
|---|---|---|---:|---:|---:|
| Samsung 990 Pro 4 TB | NVMe, PCIe 4.0 x4 | 16 files in parallel | 6 352 | 6 523 | 6 206 |
| Samsung 980 Pro 1 TB | NVMe, in a PCIe 3.0 x4 slot | 16 files in parallel | 2 758 | 2 854 | 2 105 |
| Samsung 860 EVO 1 TB | SATA SSD | 4 files in parallel | 515 | 407 | 536 |
| Samsung 840 EVO 500 GB | SATA SSD | 4 files in parallel | 525 | 525 | 523 |
| Seagate IronWolf 12 TB | SATA hard disk | 1 file at a time | 238 | 242 | 243 |
| WD Elements 18 TB | USB 3 hard disk | 1 file at a time | 221 | 222 | 222 |

The same algorithms on data already in memory, to show what the CPU alone
can do:

| Algorithm | MB/s |
|---|---:|
| BLAKE3, all cores | about 47 000 |
| BLAKE3, 1 thread | about 4 700 |
| XXH3-128, 1 thread | about 25 000 |
| SHA-256, 1 thread (SHA extensions) | about 2 200 |

What the numbers say:

- On every drive the drive is the limit, not the hashing. All three
  algorithms run at the speed the device can deliver, so the default
  (BLAKE3, cryptographic) costs nothing compared with the fastest one.
- Direct reads and normal reads were equally fast everywhere (BLAKE3,
  direct against normal: 6 352 / 6 409, 2 758 / 2 675, 515 / 526,
  525 / 525, 238 / 243, 221 / 222). Direct I/O is therefore the default,
  because it also guarantees the data comes from the drive.
- The 990 Pro sample was only 3.3 GB and took half a second, so its
  figures are coarse. The 407 MB/s for XXH3 on the 860 EVO (911 small
  files) is an unexplained outlier; the other two algorithms reached the
  SATA limit on the same files.

To measure your own drives, choose a folder or files in the benchmark
dialog. They are only read.

## Known limitations

- File names that are not valid Unicode cannot be tracked; they are
  reported as problems.
- Hard links are recognised within one Add run. Names of the same file
  added in separate runs are each read.
- Running two copies of hash-check on the same folder at once is safe for
  the database but not prevented; one will wait for the other.
- Drive type detection exists on Windows only. Elsewhere every drive is
  read one file at a time unless you set a number in the Drives list.
- With millions of entries, switching the view (a root, a filter) takes
  around a second.

## Version history

See [CHANGELOG.md](CHANGELOG.md). Milestones: 0.1.0 core hashing and
database, 0.2.0 full window, 0.3.0 verification and reports, 0.4.0
per-drive performance and benchmark, 1.0.0 robustness and benchmarks.

## Licence

GNU General Public License v3. See [LICENSE](LICENSE).
