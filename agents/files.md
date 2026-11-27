# Lane `files` — the file overrides (Build 3 piece)

`crates/winfiles` (package `rebuild-winfiles`). No GPU, no title code, nothing vendored, no game bytes.
Depends on `crates/vfs`; `crates/runtime` as a dev-dependency only (a test reads its `OVERRIDES`).

```
state     F1 ✅  F2 ✅  F3 ✅  (round 1, PR #12, merged)   F4 ✅  F5 ✅  (round 2)
tests     round 1: 7; round 2: +4 (winfiles 11)
F2        145 outcomes side by side with the host on the same tree: 0 differ (129 / 145 on the first pass)
F4         92 CRT outcomes side by side with the host CRT (ucrtbase): 0 differ (90 / 92 on the first pass)
F5         46 outcomes side by side with the host: 0 differ (42 / 46 on the first pass); base hash unchanged
```

## Round 2: F4 and F5

### Built

| # | what | closed by |
|:--:|:--|:--|
| F4 | `src/crt.rs`: the 13 path-taking CRT functions the title imports (`fopen`, `_wfopen`, `_fsopen`, `_wfsopen`, `_open`, `_wopen`, `_wstat64`, `remove`, `_wremove`, `_wrename`, `_wrmdir`, `_wchdir`, `_wgetcwd`). **Opening:**<br>• Mode strings and `oflag` become `CreateFile` access, disposition, share, attributes and flags as the CRT derives them (`_O_TEMPORARY` is delete-on-close, `_S_IWRITE` absent means read-only, plus `_O_SHORT_LIVED`, `_O_SEQUENTIAL`/`_O_RANDOM`, `_O_OBTAIN_DIR`).<br>• The open goes through **our** `CreateFile` core.<br>• The host handle goes to the **same** `ucrtbase.dll` the title is bound to, through `_open_osfhandle` and `_fdopen`, so `fread`, `fclose`, `_read`, `_close` and the rest stay the host's.<br>• Text mode follows `_get_fmode`, and the CRT's final Ctrl-Z truncation for `r+` text is kept.<br>**The rest:**<br>• `errno` and `_doserrno` follow the CRT's own `_dosmaperr` table.<br>• `_wstat64` calls the host CRT on the resolved host path, with `st_dev`/`st_rdev` set to the console drive.<br>• `_wgetcwd(NULL, …)` allocates with the host CRT's `malloc`. | `f4_crt_side_by_side_with_the_host_crt` (92 outcomes, 0 differ); `f4_mode_strings_and_exports` (every function is in `agents/loader-imports.csv` and resolves through `override_export`) |
| F5 | `src/extra.rs`:<br>• `CopyFile2` is the host's copy into the overlay; `COPY_FILE_FAIL_IF_EXISTS` is checked in the merged view.<br>• `CreateHardLinkW` copies the existing file up first, so a link never shares data with the base, and the link is made in the overlay.<br>• `CreateSymbolicLinkW` is refused with `ERROR_PRIVILEGE_NOT_HELD` (the host's answer to an unprivileged caller) and logged.<br>• `WerRegisterFile` succeeds and is logged.<br>• `SetFileInformationByHandle`: a rename through a handle goes to an overlay path and the index follows; a delete disposition settles once the host removes the file; other classes go to the host. | `f5_copies_links_handle_changes_and_directory_attributes_match_the_host` (46 outcomes, 0 differ; base hash unchanged) |
| F5 | The vfs gaps (item 57), in `crates/vfs`:<br>• **Delete-on-close and delete through a handle:** `Vfs::delete_pending` records the file. The next operation (one atomic load when nothing is pending) drops it from the index once the host has removed it, and whites out the copy below, so a base file deleted this way stays hidden.<br>• **Rename through a handle:** `overlay_path_for` gives the target, and `note_renamed` updates the index, directories included.<br>• **Directory attributes and directory write handles:** `materialize_dir` copies a base directory into the overlay (attributes copied); its files still merge from below.<br>• `note_created` records a file the host made (a copy or a link). `console_path_of` maps a handle's final path back to the console. | `f5_delete_pending_settles_and_hides_the_base_file`; vfs's 18 tests unchanged and green |

### Measured

- F4 first pass, 90 of 92. Both differences had one cause: a write open (`"w"`) of a read-only base file succeeded,
  because copy-up clears the read-only bit. Fixed in the `CreateFile` core for every caller: the host refuses to write
  or overwrite a read-only file, so ours does too.
- F5 first pass, 42 of 46. All four came from a rename through a handle to a bare name. The host resolves a relative name
  against the process's current directory (here on another volume: `ERROR_NOT_SAME_DEVICE`), not the file's directory.
  Ours now resolves it against our current directory, and the test uses full paths, since a bare name depends on the
  host process's directory.
- The symbolic-link refusal was compared with the host's (it returned 1314 here); with an elevated host the line is skipped.

### Decided

- Unicode text modes (`ccs=`, `_O_WTEXT`, `_O_U8TEXT`, `_O_U16TEXT`): opened with the right descriptor mode, but no byte-order mark
  is read or written. Each use is logged as discovery. The title's `fopen` calls are byte streams (`msvcp140`'s `_Fiopen`).
- An invalid mode string fails with `EINVAL` and is logged. The host CRT would call its invalid-parameter handler and terminate.
- `_wchdir` changes our current directory only. The CRT's per-drive `=X:` variables are not set.

## Built

| # | what | closed by |
|:--:|:--|:--|
| F1 | The 22 `files` entries of `crates/runtime` `OVERRIDES` as `unsafe extern "system"` functions with the host's exact signatures (`src/lib.rs`, `src/find.rs`). Paths are made absolute against **our** current directory (relative, `\x`, `X:rel`, `\\?\X:\x`), go through `vfs`, and the host's own function opens the resolved host path, so **the handles are the host's** (`ReadFile`, `WriteFile`, `CloseHandle`, `GetFileSizeEx` stay bound to the host). Reads open the winning layer as it is. Any open that could write (write, append, attributes, `DELETE`, `MAXIMUM_ALLOWED`, delete-on-close, or a creating or truncating disposition) goes through `vfs.open` first (create in the overlay, copy up, truncate there), then the host opens the overlay file with `OPEN_EXISTING`, and the `ERROR_ALREADY_EXISTS` / 0 last error is set as the host sets it. New files take the title's attribute bits. The base and mods are never opened for writing. `binding`: `override_export(name)` and `FUNCTIONS`, as `modules::override_export` does. State: one `Files` (a `Vfs` and the current directory), set by `install` | `f1_create_file_reads_the_layers_and_writes_only_the_overlay` (base and mod hashed before and after: unchanged), `f1_find_merges_overlay_mod_and_base`, `f1_delete_directories_and_move`, `f1_full_paths_current_directory_disk_and_temp`, `f1_every_files_override_is_implemented` (the list equals the runtime's `files` list, in order) |
| F2 | The same script of 145 calls runs against our functions on `G:\…` (base: a copy of the tree) and the host's functions on the tree's host path. Each outcome compares the return value, `GetLastError`, bytes read, attribute bits, sizes, 8.3 names and find order (times differ between two trees, so not compared). Covers all five dispositions and invalid ones, directories with and without backup semantics, `CreateFile2`, `CreateFileA`, A/W attribute pairs, `GetFileAttributesEx` levels, delete (including read-only and twice), directory create and remove (missing, non-empty, a file), `MoveFileExW` (replace, missing source or parent, a directory, change of case, `COPY_ALLOWED`), 15 search patterns (`*`, `*.`, `?.txt`, exact, trailing `\`, a file as a directory, Basic level and large fetch), full path names (the too-small size too), the current directory and relative paths, and free space | `f2_side_by_side_with_the_host`: **145 / 145 equal**; base folder hash unchanged after the script |
| F3 | A path `vfs` does not know is a **discovery entry** and the call fails, never guessed and never handed to the host: UNC, `\\.\` devices, `CON`/`NUL`/`CONOUT$`, an unmounted drive letter, a host path, streams (`a.txt:s`, `ERROR_INVALID_NAME`), the empty path, plus `MOVEFILE_DELAY_UNTIL_REBOOT` and write access to a directory below the overlay. `discoveries()` lists (function, path, why, count); repeats are counted, not listed twice. Nothing installed: `ERROR_NOT_READY`, never the host | `f3_unknown_paths_are_logged_and_refused` (a real host file at the passed host path is untouched) |

## Measured

- F2 first pass: 129 of 145 equal. The 16 differences, all fixed:
  - searching inside a file gives `ERROR_DIRECTORY`, not `ERROR_PATH_NOT_FOUND`;
  - `*.` matched nothing through the `\\?\` form: the host applies its DOS wildcard rules only to plain paths, so searches use the plain form when it fits;
  - `GetFullPathNameW`'s too-small size is the length **before** normalising (+1). Ours now hands the whole call to the host on the absolute console path;
  - free space on a missing directory is `ERROR_PATH_NOT_FOUND`;
  - `CREATE_NEW` on a directory is `ERROR_ACCESS_DENIED`;
  - a change of case only, on a base file: `vfs` renames case only in the overlay, so we copy the file up first;
  - the rest were the test printing host paths, and the drive root keeping its `\` (correct for a drive).

## Decided

- **Searches are our handles.** A console directory can be several host directories, so `FindFirstFile*` return our search
  handles (values above the user address space, never a host handle). Each entry is the host's own `WIN32_FIND_DATA`
  from the host's `FindFirstFileExW` on that layer's directory with the title's pattern, level and flags. An entry is
  kept only from the layer that wins it, so whiteouts and shadowed files never show. One layer: the host's order.
  Several: by upper-cased name. A drive root has no `.` / `..`.
- `T:` is the temp path (`GetTempPathW`). The current directory starts at whatever `Files::new` is given (`G:\` in the tests).
- Free space is the overlay's volume for every drive: every write lands there.

## For the lead

Round 2 (current):

1. **Bind the CRT overrides too.** `override_export(name)` answers for all 45 names:
   - the 22 `files` entries;
   - `FindNextFileW`, `FindNextFileA`, `FindClose`, `SetFileAttributesW` and `GetTempPathW`;
   - `CopyFile2`, `CreateHardLinkW`, `CreateSymbolicLinkW`, `WerRegisterFile` and `SetFileInformationByHandle`;
   - the 13 in `crt::CRT_FUNCTIONS`.

   The CRT names come from `api-ms-win-crt-stdio-*`, `-filesystem-*` and `-environment-*`. Bind them by function name
   whatever the DLL, as `OVERRIDES` already does.
2. `SetFileInformationByHandle` is not in the title's import table. It only matters if a `GetProcAddress` miss shows the
   title reaching it, so route that miss to `override_export` as well.
3. Wiring is unchanged: `rebuild_winfiles::install(Files::new(vfs, r"G:\"))` before binding.

Round 1 (answered by the lead; kept for the record):

1. **`OVERRIDES` needs 5 more** (implemented here, in `FUNCTIONS[22..]`):
   - `FindNextFileW`, `FindNextFileA` and `FindClose`: our search handles need them. They pass host handles to the host.
     The title imports `FindNextFileW` and `FindClose` (exe, `msvcp140`, `vcomp140`).
   - `SetFileAttributesW`: imported by the exe and `msvcp140`.
   - `GetTempPathW`: `msvcp140`; the host's returns the host's temp folder.
   - Until they are bound, the title's own `FindNextFileW` hits the host with our handle, and `SetFileAttributesW` gets a
     console path. **If a console letter (`G:`, `T:`) also exists on the host, the host functions act on host files.**
2. **The CRT bypasses every file override.** `ucrtbase` (`api-ms-win-crt-*`) is the host's, and its internal
   `CreateFileW` is the host's. So the title's `fopen`, `_wfopen`, `_open`, `_wopen`, `_fsopen`, `_wfsopen`, `_wstat64`,
   `remove`, `_wremove`, `_wrename`, `_wrmdir`, `_wchdir` and `_wgetcwd` (in `agents/loader-imports.csv`) see host paths
   through the host. They need overrides too: open through our `CreateFileW`, then `_open_osfhandle` gives a host CRT descriptor.
   `msvcp140` is the title's own, so its imports already come to us.
3. Also path-taking and not overridden: `CopyFile2`, `CreateHardLinkW`, `CreateSymbolicLinkW` (`msvcp140`, `std::filesystem`),
   `WerRegisterFile`. They would log as discovery once routed here.
4. The census says files 15; `OVERRIDES` lists 22 `files` names (some are not imported, e.g. `FindFirstFileA`). All 22 are built.
5. To wire: `rebuild_winfiles::install(Files::new(vfs, r"G:\"))` before binding. Then, in `HostResolver`'s override arm, try
   `rebuild_winfiles::override_export(name)` beside `modules::override_export`. Read `discoveries()` with `modules::misses()`.
6. Known gaps in `vfs` (lane `vfs`, not changed here):
   - `FILE_FLAG_DELETE_ON_CLOSE` and handle-based delete or rename (`SetFileInformationByHandle`) change the overlay
     behind `vfs`'s index, which keeps a stale node.
   - Directory attributes and a change of case of a directory below the overlay are refused (logged).
   - A `CREATE_ALWAYS` over a base file does not carry its attributes.

## Next (not in this lane)

Binding in `crates/runtime` is the lead's.
