# agents/secondary-agents-updates.md — status of the secondary agents, for the lead

Written and kept current by the session that started the helper agents, for the lead to read at a glance.
Not committed by the helpers: this file sits only in the lead's tree, so commit or delete it as you like.
Each lane's own detail is in its `agents/<lane>.md` on its branch, and arrives with its pull request.

**Last updated:** 2026-11-27 12:55 (Adelaide; earlier log times were estimates, corrected below)

## Lanes

| lane | build | branch | worktree | state | last reported |
|:--|:--|:--|:--|:--|:--|
| **loader** | Build 3, first half (L1–L4) | `agent/loader` | `D:\Developer\triumph-rebuild-loader` | ✅ **merged** by the lead ([#1](https://github.com/jleybourne/triumph-rebuild/pull/1), master 7cc1230) | lane finished |
| **shader** | Build 4 front end (S1–S5) | `agent/shader` | `D:\Developer\triumph-rebuild-shader` | 🔄 S1–S4 committed on its branch (1 commit, "25,694 programs"), working on S5; no PR yet | 13:02 (S1–S4 commit: 25,694 programs, 43.95M instructions) |
| **launcher** | Triumph Launcher (U1–U5, plus library and per-game settings) | `agent/launcher` | `D:\Developer\triumph-rebuild-launcher` | 🔄 2 commits: library, per-game store, theme pass 1 (31 tests); pass 2 in progress | 13:20 |
| **vfs** | overlay file system (V1–V4) | `agent/vfs` | `D:\Developer\triumph-rebuild-vfs` | ✅ **merged** by the lead | [#3](https://github.com/jleybourne/triumph-rebuild/pull/3) (2 commits) |
| **upscale** | DLSS and FSR backends, standalone (P1–P5) | `agent/upscale` → `agent/upscale-fsr` | `D:\Developer\triumph-rebuild-upscale` | ✅ P1, P2, P4, P5 **merged** (PR #4); 🔄 P3 (FSR) on `agent/upscale-fsr`, SDK files fetched to `D:\tools\fidelityfx\v2.4.0` | [#4](https://github.com/jleybourne/triumph-rebuild/pull/4) |
| **input** | controller and keyboard (I1–I3) | `agent/input` | `D:\Developer\triumph-rebuild-input` | ✅ **merged** by the lead | [#2](https://github.com/jleybourne/triumph-rebuild/pull/2) (2 commits) |
| **audio** | console audio hardware (A1–A4) | `agent/audio` | `D:\Developer\triumph-rebuild-audio` | 🔄 started 13:25 | — |
| **dstorage** | DirectStorage (D1–D4) | `agent/dstorage` | `D:\Developer\triumph-rebuild-dstorage` | 🔄 started 13:25 | — |

No lane merges; each opens a PR to `master` titled `agent/<lane>: <what closed>`.

### Loader numbers (as reported; PR confirmed open, contents not reviewed by me)

| task | result |
|:--|:--|
| L1 PE32+ parser | **4,523 / 4,523** System32 PE32+ images parsed, 0 errors (6,382 / 6,382 recursive); 38 32-bit PE32 files refused by design; 200,000 random corruptions, no panic |
| L2 map at base | a test DLL maps at 4 bases; real `dbghelp.dll` beside the Windows loader at the same base: **9,912 relocations, 0 wrong** |
| L3 import census | 18 title images, **93 imported DLLs, 962 distinct functions**; the title's own DLLs supply 265; **GDK discovery list: 697 functions in 86 DLLs** (`agents/loader-imports.csv`, names only); game folder hashed unchanged before and after |
| L4 title DLLs | all 16 tabled with export counts in `agents/loader.md` |
| tests | `cargo test --release`: 48 pass (13 new). Nothing vendored, so no `CREDITS.md` line. |

## Rules every lane was given

- `secondary-agent.md`, word for word, plus a reminder of the points below.
- Its own worktree only; never `checkout` or `switch` in `D:\Developer\triumph-rebuild`.
- None of the lead's files in section 2. One line in the root `Cargo.toml` `members` list is allowed.
- A separate check for `agents\MEASURING` before every build or test, and a wait while it exists. No dodging a block with chained short sleeps.
- `cargo build --release -j 4`. No GPU work of any kind.
- No game bytes in git: hashes, names and counts only. The game folder is read-only, through `REBUILD_GAME_DIR`.
- Licences: xcl (GPL-2.0) for design only. Only non-performance code from the fork, with the header and a `CREDITS.md` line. The shader IR, lowering and emission are the lead's.
- Every task ends in a test or a number.

## For the lead

Anything a lane flags about the lead's code goes here as a one-line pointer to its `agents/<lane>.md`, and is never fixed by the lane.

From **loader** (details in `agents/loader.md` on `agent/loader`):
1. `plan.md` line 120 says the title never loads the FidelityFX loader, but the exe imports `amd_fidelityfx_loader_xbox.dll` statically (4 functions) and has no delay-load imports, so it is mapped at start. The upscaler DLL is imported by nothing.
2. No image imports GDK runtime functions (`XGame*`, `XUser*`) directly, so the title must reach them through `GetProcAddress`. Discovery must catch those misses too, or 697 undercounts what the title calls.
3. The README says 35 tests; with this branch it is 48.
4. Two build warnings on every build: `crates/uma/src/tileproof.rs:572`, `crates/uma/src/upload.rs:560`.
5. Shared-file edits to merge: one line in the root `Cargo.toml` members, and `rebuild-loader` added to `apps/rebuild/Cargo.toml`.

From **input** (details in `agents/input.md` on `agent/input`, PR #2):
9. A poll costs ~170 µs with focus (4 empty XInput slots 64–82 µs median, 244-key scan 103 µs), against the debug menu's 0.3 ms overlay budget. Before L5, poll on its own thread (the fork polls every 8 ms) with the present reading the latest result, or check empty slots ~once a second.
10. I3 read the census from unmerged `agent/loader` (`46d04f8`), now merged in 7cc1230. The title imports one function from `gameinput.dll`, ordinal #1 (`GameInputCreate`); the rest are methods on returned objects, all tabled. The lane fetched the `agent/loader` ref explicitly in the lead's repo; only the remote-tracking ref changed, nothing was checked out.
11. Tests with this branch: 59 (43 + 16 new), 1 ignored (the poll-cost measure, run by hand).

From **vfs** (details in `agents/vfs.md` on `agent/vfs`, PR #3):
13. Hash-before/after checks must read `fs::metadata(path)` per file, not `fs::read_dir` entry times: NTFS updates those lazily, and the first V2 test failed on ~1 ms drift in folders nothing wrote. Applies to launcher U3 and Build 3's "game folder unchanged" check in `tasks.md`.
14. Runtime wiring: `VfsBuilder::package('G', title_dir, &mods)`, `.writable('T')`, `.writable(<storage letter>)`. The storage path given to the title should be a drive root (it keeps only what precedes the last `:\`).
15. Decide whether T: starts empty each run; today it persists like storage.
16. Numbers: 18 tests (workspace 61); 10,000-file bench +3.3 µs/file (+4.5 %) over plain Windows open+read; 0 wrong reads; real game folder hash `c13db2877f6ed4ea` unchanged.

From **upscale** (details in `agents/upscale.md` on `agent/upscale`, PR #4):
17. **DLSS works** through Streamline 2.14.16 on the analytic scene (2560x1440 out). Quality: 30.96 dB vs bilinear 26.50 (moving), 0.981 ms. Performance: 31.30 dB, 1.147 ms. 0 NaN, 0 black frames. Sign sweep: Streamline's `jitterOffset` is the **negated** sample offset; motion vectors pass through unchanged. Use the SDK's render sizes (balanced 1485x835), not a ratio rule.
18. Registry entries to add: `upscaler.dlss_dll` and `upscaler.streamline_dir`. Until then, `REBUILD_DLSS_DLL`, `REBUILD_STREAMLINE_DIR` and `REBUILD_SL_LOGS`. The lane extracted the SDK to `D:	ools\dlss\sl-2.14.16` and the runtime DLLs to `D:	ools\dlss\sl-runtime` (outside git).
19. **FidelityFX SDK download needed** (MIT, GPUOpen GitHub) for P3, side-by-side, and the FSR sign sweep. Not downloaded: owner or lead decision.
20. Build 7: under manual hooking, call `slUpgradeInterface` on the layer's swap chain right after creating it, or Streamline logs missing presents.
21. **The NVIDIA driver starts its own `nvngx_update.exe` during NGX init, even with over-the-air updates off** (downloaded plugins are not loaded). This touches the launcher's no-telemetry line.
22. NGX reports DLSS version 0.0.0; read the DLL's file version for the frame line.

23. **Owner: the launcher's user-facing name is "Triumph Launcher"** (brand, window title, About, exe display name). The lane keeps the package name `rebuild-launcher` unless the lead says otherwise.

24. **Owner: Triumph Launcher must support several games** (add and remove, one day other titles). The lane is building a library with per-game folder, config, settings copy, mods, logs and profile, and title detection from the game's own files through a detector trait. Remove never touches the game folder. **The lead may need a per-game config layout in `crates/config`;** the lane will propose it in `agents/launcher.md`.

25. **Owner: settings are saved per game and never replaced.** Per-game files in the launcher's app-data. Upgrades and reinstalls only fill missing keys, never reset values, and keep unknown keys. Atomic writes with a `.bak`, a schema version with non-destructive migration. The title-settings copy is regenerated from saved values at launch. Tests for isolation, upgrades, unknown keys, crash recovery and re-adding a removed game.

From the **coordinator**, on the new lane text in `secondary-agent.md` (71a521f):
6. Lane `launcher` U5 says "launch the proofs (`gputest --bare`)", but section 2 says helpers never run `gputest`. The launcher lane was told to build U5's launch path against a fake child process in tests, and to run no `gputest` itself. Please say which you meant.
7. Lane `upscale` P3 needs AMD's FidelityFX SDK. If it isn't already on disk, getting it is a download. The lane was told not to download without your say, and to do P1, P2, P4 and P5 first. For DLSS it was pointed at the Streamline SDK zip already in `D:\tools\dlss\download` (production `bin/x64`, extracted outside git) and the user-supplied `nvngx_dlss.dll` in `D:\tools\dlss`.
8. Lane `input` I3 reads `agents/loader-imports.csv` from the unmerged `agent/loader` branch (PR #1), and will say so.

12. **Owner request for the launcher (13:00):** "more premium; it's a bit basic". The lane was given a design brief on top of U1–U5, with no rule changes:
    - a sidebar and cards layout;
    - a custom dark theme with one accent colour, rounded surfaces and animations;
    - an embedded open font and icon font, credited;
    - a Play hero header with a status pill and the last fps;
    - styled settings, checklist, mod cards and log console;
    - key art only loaded at run time from the game folder, never in git;
    - screenshots of every tab saved outside git in `D:ictory-data\launcher-shots\` (GPU only while `MEASURING` is absent).

## Log

| time | event |
|:--|:--|
| 13:22 | Lead merged PR #4 (upscale, DLSS). Upscale FSR branch started with the SDK files on disk. Launcher committed the library and per-game store (31 tests). Audio and dstorage set up, no commits yet. |
| 13:30 | Owner approved the FidelityFX download in chat: **only the needed files** (~58 MB at tag v2.4.0: loader and upscaler DLLs, FSR 3.1 and API headers, licence) to `D:/tools/fidelityfx/v2.4.0`, outside git. The upscale lane fetches them, records SHA-256s, and reads the licence before any code (it stops if the licence is not GPL-3.0-compatible). |
| 13:25 | Read the lead's answers (section 10). Upscale told the FidelityFX download is approved (pinned version, `D:\tools\fidelityfx\<version>`, SHA-256). Launcher told U5 uses a fake child, `fs::metadata` for hash checks, rebase, and use `rebuild_vfs::ModSet`. New lanes **audio** and **dstorage** started. The lead is mid-merge of PR #4 in its tree; the coordinator touched nothing there but this file. |
| 13:22 | Owner: per-game settings must persist and never be replaced; sent to the launcher lane. |
| 13:20 | Owner asked for multi-game support in the launcher (add/remove games); sent to the lane. |
| 13:18 | Launcher pass 1 reviewed (grain far too strong, right-edge overflow, hero text over the key art, low contrast, demo data, name hygiene); renamed "Triumph Launcher" at the owner's request. |
| 13:12 | Lead merged PR #2 (input) and PR #3 (vfs). Open: #4 (upscale). Running: shader (S5), launcher (design pass 1). |
| 13:10 | **upscale**: P1, P2, P4, P5 closed; P3 blocked on the FidelityFX SDK download; PR #4 open (not merged). |
| 13:05 | **vfs done**: V1–V4 closed, PR #3 open (not merged). |
| 13:00 | Launcher lane given the owner's premium-design brief. |
| 13:00 | **input done**: I1–I3 closed, PR #2 open (not merged). |
| 12:55 | Lead merged PR #1 (lane loader) in 7cc1230, with answers to the coordinator's items. Lane shader has S1–S4 committed and is on S5; lane input has I1–I3 committed; launcher, vfs, upscale writing code. |
| 12:50 | `secondary-agent.md` updated by the lead (71a521f): lanes launcher, vfs, upscale and input started; lane shader told of the S2/S4 additions and S5. |
| 12:55 | **loader done**: L1–L4 closed, PR #1 open (not merged). |
| 12:44 | loader: `crates/loader` and `apps/rebuild/src/bin/imports.rs` written; a first `agents/loader-imports.csv` exists (L3 has run once). shader: `crates/shader` and `census.rs` being written (S1–S2). No commits yet. Both lanes also edited `apps/rebuild/Cargo.toml` (to add their dependency): not reserved, but expect a small merge there. |
| 12:40 | Both lanes started. The old project's (fxceless-victory) recurring status, sync, continuation and fork-check jobs were cancelled; no further work on it. |
