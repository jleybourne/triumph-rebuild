# Lane `vfs` — the overlay file system (Build 3 piece)

`crates/vfs` (package `rebuild-vfs`). Pure Rust, no dependencies, no GPU, no title code, nothing vendored
(the fork's `victory-hle` files module was read for which drives the title uses; no line copied).

```
state     V1 ✅  V2 ✅  V3 ✅  V4 ✅
tests     18 green in crates/vfs; workspace 61 green (43 + 18)
bench     open + size + read + close on 10,000 small files: 77.0 µs/file warm, OS floor 73.7 µs/file,
          VFS overhead +3.3 µs/file (+4.5 %); in-memory lookup 1.54 µs/file
```

## Built

| # | what | where | closed by |
|:--:|:--|:--|:--|
| V1 | console path → host path. Drives: `G:` the package (read-only base = game folder, mods on it), `T:` temporary files, and persistent storage at whatever letter the runtime gives it (the fork mounts it at a free letter). Every form the title uses: `X:\a`, `X:/a`, `X:a`, `X:`, `\\?\X:\a`. `.` dropped, `..` stops at the drive root, trailing dots and spaces dropped as Win32 does. Lookup is case-insensitive: each directory is listed once and cached by folded key, and host paths keep the disk's case. `NotFound` and `PathNotFound` are told apart, as `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND`. An overlay folder inside the base or a mod is refused at build. | `src/path.rs`, `src/layer.rs`, `Vfs::resolve` | `path::tests` (6), `v1_*` (3) |
| V2 | write overlay: `<overlay>/<LETTER>/files/..`, default `%APPDATA%\triumph-rebuild\overlay`. A write to a lower file copies it up first (read-only attribute cleared on the copy); a delete or rename of one records a whiteout in `whiteouts.txt`; directory renames move the merged tree. Lower layers are only ever opened for reading. State persists into the next run. | `src/overlay.rs`, `Vfs::{open, delete, rename, create_dir, remove_dir}` | `v2_write_heavy_run_leaves_the_base_unchanged`: 503-file base, > 1,500 writes, deletes, renames (files and a directory), re-creation; base and mod folders hashed (names, sizes, times, read-only bit, every byte) before and after: **unchanged**; reopened, the state persists. `v2_dispositions_and_errors` |
| V2 | the same check against the **real game folder** (`REBUILD_GAME_DIR`, skipped when unset): writes over, deletes and renames the 12 smallest files through the overlay, then opens every file in it | `v2_game_folder_unchanged` | 151 entries hashed (names, sizes, times, attributes) plus every byte of the 12 touched files: `c13db2877f6ed4ea` before and after |
| V3 | mods: `ModSet::discover(mods_dir)` (every folder, enabled, by name), `set_enabled`, `move_to`, `conflicts()` (every path two or more enabled mods provide: providers in load order, the winner, the path in the winner's case). Mods layer on the package drive between the overlay and the base; later wins. A write over a mod's file copies it up from the winning mod. | `src/mods.rs` | `v3_later_mods_win`, `v3_enable_disable`, `v3_conflicts`, `v3_a_write_over_a_mod_file_lands_in_the_overlay` |
| V4 | the handle API: `open(path, OpenOptions)` with `CreateFile`'s five dispositions, `read_at` / `write_at` (positional, no shared cursor), `size`, `set_size`, `flush`, `close`, `enumerate` (merged, one entry per folded name, top layer wins), `resolve`. No global state: a `Vfs` owns its drives and handles and is shared by reference across threads. Handles carry a generation, so a stale one is refused even after its slot is reused. | `src/lib.rs` | `v4_handles`, `v4_shared_between_threads` (8 threads, 400 creates and 800 reads) |

## Measured

`cargo run --release -p rebuild-vfs --bin vfsbench`. Synthetic files in a temp folder: base 100 × 100 files (512 B–4 KiB, 23 MB),
two mods replacing 1,000 files each (500 contested), 1,000 files rewritten into the overlay. Console paths are given in a
different case from the disk. Raw output of three runs: [`vfs-bench.txt`](vfs-bench.txt). Run 1 of 3:

```
layer check     10000 / 10000 files served by the expected layer
cold vfs pass      921.7 ms   92.17 us/file   (directory listings built on first use)
warm vfs pass      770.1 ms   77.01 us/file   median open+size+read+close
OS floor           737.3 ms   73.73 us/file   median File::open + read_to_end on the same host files
vfs overhead       +32.9 ms   +3.29 us/file   warm vfs - OS floor
lookup only         13.8 ms    1.54 us/file   median resolve() of the 9000 base and mod files, no host call
4 threads         1451.4 ms   36.28 us/file   40,000 opens on one shared Vfs, wall
wrong           0 cold, 0 warm
```

- The cost is Windows' own open: ~70 µs a file on this machine (with other lanes building). The VFS adds ~3.3 µs.
- A first version listed each directory level per lookup and cloned on the way: 4.7 µs a lookup. Caching each
  listing under its folded key with its host path made a lookup two hash hits: **1.54 µs** (3×).
- The title opens few, large files (the package is 110 files, 209 GB), so the open path is not a frame cost.

## Decided

- Whiteouts, not deletes: the base is never touched; a deleted base file stays hidden across runs, also under a
  re-created directory of the same name.
- Copy-up copies the whole file. The package's writable targets are small (settings, saves); a range-level
  copy-up was not worth its complexity. Measure again if the title writes over a large package file.
- An overlay file's length is read from the host only when asked (`resolve`, `enumerate`): handles can change it.
- `T:` is overlay-only and **persists** like storage. Whether `T:` is wiped at start is the runtime's call (below).

## Next (not in this lane)

- Directory handles (`CreateFile` with `FILE_FLAG_BACKUP_SEMANTICS`), file times and attributes, share-mode
  enforcement, a current directory for relative paths, `\\.\` device paths. Add them when the GDK discovery loop
  shows the title calling them.
- The launcher's mods tab (lane `launcher`, U4) can use `ModSet::discover` and `ModSet::conflicts` directly.

## For the lead

1. **Hash tests must read each file's own metadata.** On NTFS, `fs::read_dir` entries carry the parent
   directory's copy of the times, which is updated lazily: the first version of the V2 test failed on a mod
   folder nobody wrote, because two directories' times in their entries moved by ~1 ms after creation. The tests
   now use `fs::metadata(path)`. Any other "hash before and after" check (lane `launcher` U3, `tasks.md` Build 3
   "game folder hash unchanged") needs the same.
2. **Runtime wiring (Build 3, second half):** mount `G:` over the title's folder (`VfsBuilder::package`), `T:`
   and the storage drive with `VfsBuilder::writable`; the storage path the title gets is the root of its drive
   (`S:\` or any free letter), since the fork found the title keeps only what precedes the last `:\`. Decide
   whether `T:` starts empty each run (the console clears it; the fork kept it under its data folder).
3. `tasks.md` Build 3 "files: console paths → game folder, write overlay, never modify the dump" can point at this
   crate once merged; its closing check ("game folder hash unchanged") is `v2_game_folder_unchanged` with
   `REBUILD_GAME_DIR` set.
