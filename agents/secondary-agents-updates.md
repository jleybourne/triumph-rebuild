# agents/secondary-agents-updates.md — status of the secondary agents, for the lead

Written and kept current by the session that started the helper agents, for the lead to read at a glance.
Not committed by the helpers: this file sits only in the lead's tree, so commit or delete it as you like.
Each lane's own detail is in its `agents/<lane>.md` on its branch, and arrives with its pull request.

**Last updated:** 2026-11-27 (round 3 started by the lead)

## Lanes

### Round 3 (from 2026-11-27, started by the lead)

| lane | job | branch | worktree | state |
|:--|:--|:--|:--|:--|
| **gdk** | G5: the two interfaces the title asks for at start (from runs/build3-reached.txt) | `agent/gdk-2` | `D:\Developer	riumph-rebuild-gdk` | ✅ merged (#20) |
| **dstorage** | D5: our BCPack decoder and decode cache (plan decision 8) | `agent/dstorage-bcpack` | `D:\Developer	riumph-rebuild-dstorage` | ✅ merged (#24) |
| **device-research** | Build 5 input: D3D12.X device slots, inline object layouts, startup call sequence (docs only) | `agent/device-research` | `D:\Developer	riumph-rebuild-device` | ✅ merged (#19) |
| **shader** | S7: classify the last v0 interface diffs against the oracle (tool fixes only; translator bugs reported) | `agent/shader-s7` | `D:\Developer	riumph-rebuild-shader` | ✅ merged (#21) |
| **shader-cf** | v1: structurize reducible control flow in `crates/sir` (handed over by the lead during Build 5) | `agent/shader-cf` | `D:\Developer	riumph-rebuild-cf` | ✅ merged (#26) |
| **pipelines** | root signatures and the hash-keyed pipeline library (`crates/pipelines`) | `agent/pipelines` | `D:\Developer	riumph-rebuild-pipelines` | ✅ merged (#22) |
| **cmdlist** | Build 5/6: the D3D12.X command list and allocator objects (`crates/xlist`), interface for the lead's device | `agent/cmdlist` | `D:\Developer	riumph-rebuild-cmdlist` | ✅ merged (#25) |
| **profiles** | built-in test profiles (hardware tiers + stress) in `crates/config` and the launcher; a matrix writer (owner request) | `agent/profiles` | `D:\Developer	riumph-rebuild-profiles` | ✅ merged (#23) |


### Round 2 (from 2026-11-27 14:15)

| lane | job | branch | worktree | state | last reported |
|:--|:--|:--|:--|:--|:--|
| **gdk** | GDK runtime behind `xgameruntime.dll` (G1–G4) | `agent/gdk` | `D:\Developer\triumph-rebuild-gdk` | ✅ **G1–G4 closed, PR open**: all 30 GUIDs the exe passes to `QueryApiImpl` named, 0 unknown; task queues and async (21 tests); 11 interfaces, 102 slots; 177 slot entries resolve | [#18](https://github.com/jleybourne/triumph-rebuild/pull/18) |
| **audio** | A5, our own XMA2 decoder | `agent/audio-xma` | `D:\Developer\triumph-rebuild-audio` | ✅ **A5 closed, PR open**: all 1,818 XMA2 streams match FFmpeg within ±1 LSB (0.076 % off by 1); ~1,200× real time on one thread | [#17](https://github.com/jleybourne/triumph-rebuild/pull/17) |
| **files** | F4 CRT file functions, F5 links, WER and vfs gaps | `agent/files-2` | `D:\Developer\triumph-rebuild-files` | ✅ **F4–F5 closed, PR open**: CRT side by side 92/92, F5 46/46, base hash unchanged | [#16](https://github.com/jleybourne/triumph-rebuild/pull/16) |
| **shader** | S6, `oraclediff` | `agent/shader-s6` | `D:\Developer\triumph-rebuild-shader` | ✅ **S6 closed, PR open**: oracle vs itself 0 diffs over 25,694 (17.8 s, 4 threads); every injected binding, signature and wave-op mutation caught | [#15](https://github.com/jleybourne/triumph-rebuild/pull/15) |
| **launcher** | U6, Add tile fit; plus the idle fix the owner asked for | `agent/launcher-idle` | `D:\Developer\triumph-rebuild-launcher` | ✅ **done by the coordinator, PR open** | [#14](https://github.com/jleybourne/triumph-rebuild/pull/14) |

Stopped, per round 2: loader, vfs, input, upscale, xg (no new work). **dstorage D5 (BCPack decoder and cache) is run by the lead's own helper agent, not the coordinator**, so it is not tracked here.

### Round 4 (from 2026-11-27, started by the lead)

| lane | job | branch | worktree | state |
|:--|:--|:--|:--|:--|
| **launcher** | L6: top-bar redesign (owner), dark/light, all sections, session diagnostics with retention | `agent/launcher-l6` | `D:\Developer\triumph-rebuild-launcher` | running |
| **resources** | Build 5: resources, descriptor heaps, views, shadow table, raw buffer mirrors (`crates/d3d12x`) | `agent/resources` | `D:\Developer\triumph-rebuild-resources` | running |
| **pipelines** | async PSO creation, ID3D12PipelineLibrary cache, prewarm | `agent/pipelines-2` | `D:\Developer\triumph-rebuild-pipelines` | running |

### Round 1 (all merged by the lead: PRs #1–#13)

loader #1 + L5 #7 · input #2 · vfs #3 · upscale DLSS #4 + FSR #8 · shader S1–S4 #5 + S5 #10 · dstorage #6 · xg #9 · audio #11 · files #12 · launcher #13.

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

From **dstorage** (details in `agents/dstorage.md` on `agent/dstorage`, PR #6):
26. Binding: resolve `xcrdapi.dll!DStorageGetFactory` to `rebuild_dstorage::DStorageGetFactory`, after `install(Engine::new(vfs, Config { pages: Some(..), .. }))` with the `G:` vfs.
27. Implement `Pages` over `runtime::physical` (64 KiB frames, the same size), with a retry for pages the title hasn't made yet: the fork lost 12–446 reads a launch without one.
28. **BCPack is CPU work, and it's the real streaming cost:** the fork's decoder ran 33–66 MB/s a thread, against zlib's 397 MB/s. A decoder needs ~78 KB of underivable format tables, which can't go in git, so they'd load at run time (suggested config `dstorage.bcpack_tables`). Then a decoder is either written here or the fork's vendored by exception. **Owner decision.** Until then BCPack requests return `E_NOTIMPL` and are counted (`Stats::bcpack_refused`).
29. Numbers: 124 workspace tests green, 0 warnings. Whole requests at 336 / 688 / 1,444 / 2,024 MB/s on 1 / 2 / 4 / 8 workers. Write-watch saw 64/64 pages of worker writes, so the Build 2 ring sees them free.

From **loader L5** (details in `agents/loader.md`, PR #7):
30. The GDK areas (XGame, XUser, XAsync, XTaskQueue, XStore) are **not** looked up by name. The title takes 6 entry points from `xgameruntime.dll` by name (`InitializeApiImpl`, `InitializeApiImplEx`, `InitializeApiImplEx2`, `UninitializeApiImpl`, `QueryApiImpl`, `XErrorReport`) and reaches every area through objects `QueryApiImpl` returns. This matches the fork's notes.
31. GetProcAddress must answer by name for `xgameruntime.dll` (6), `gameplatformservices.dll` (3: `GetApiVersionImpl`, `RestartApiImpl`, `RundownApiImpl`), winhttp's `WinHttpWebSocket*` (6) and `icu.dll` (14 time-zone functions). `agents/loader-getproc.csv`: 76 names (high 42, medium 3, low 31; the low ones are likely log strings).
32. The FidelityFX loader names `amd_fidelityfx_framegeneration_xbox.dll`, which the title does not ship. If that loader is ever called, its load of that DLL must fail cleanly.

From **upscale P3** (details in `agents/upscale.md`, PR #8):
33. FSR 3.1.6 through FidelityFX SDK v2.4.0 works. Quality: 33.69 dB moving (DLSS 30.96, bilinear 26.50), static frame-to-frame 0.159, 0.508 ms. 0 NaN, 0 black frames. Synthetic PSNR proves the wiring, not which upscaler looks better.
34. Download (owner-approved, needed files only): two AMD-signed DLLs plus 6 headers and the licence, in `D:\tools\fidelityfx\v2.4.0`, SHA-256s in `agents/upscale.md`. **Licence:** files not in the MIT list are AMD binary-only; every file we use (both DLLs included) is in the MIT list, loaded at run time, so it's GPL-3.0-compatible. Nothing in git.
35. Registry entry to add: `upscaler.fidelityfx_dir`. Until then, `REBUILD_FIDELITYFX_DIR`.
36. Both SDKs want jitter negated on both axes and motion vectors passed as-is; both backends now share one `Signs` mapping. FSR render sizes follow the ratio rule (balanced 1505x847); Streamline's don't (1485x835).
37. FSR 4: the DLL is 4.1.2, but this card is offered only 3.1.6 and 2.3.4. RDNA 4 will probably be offered FSR 4; whether AMD should prefer it is the lead's call (one line in `Fsr::attach`).

From **xg** (details in `agents/xg.md` on `agent/xg`, PR #9):
38. Binding: wrap `Computer` in a 13-slot vtable in `crates/runtime`, in `SLOTS` order. **Slot 0x30 must return 0 for a texel outside the texture**: the fork saw an error code read as an offset send the title into wild memory. Size copy slices with `Computer::copy_extent`.
39. Build 5: `Layout::tables` (per-axis row and column tables) is ready for detiling on the card. The fork estimates 2–4 ms of its 4–6 ms texture refresh moves off the CPU.
40. `XGSuggestSwizzleMode` follows the fork's older library rule: if streamed textures come out scrambled, check it first.
41. HTILE, CMASK, FMASK and DCC metadata planes aren't sized (as in the fork). That only matters if the title sizes a pool from console sizes.
42. Numbers: 0 bytes differ from the fork's detiler over 174 synthetic cases (43.4 MB). BC7 64KB_D 2048² full chain: 0.49–0.54 ms (~11 GB/s). Workspace 135 green, 0 warnings. The layout maths are vendored from the fork's `victory-formats/src/texture.rs` (itself following AMD's MIT addrlib) with the header; `CREDITS.md` created.

From **shader** (details in `agents/shader.md`, PRs #5 and #10):
43. **Numbers:** 25,694 programs (VS+PS+CS+MS), matching the fork's census by hash and stage; all 1,433 world-frame hashes found. The decoder re-encodes **43,952,121 / 43,952,121 instructions byte-exact, 0 unknown opcodes**. 99.18 % of programs are structured (20 irreducible CS; the 195 ES end in an indirect jump). Branches are 59.7 % per-lane, 40.3 % wave-uniform. Wave class: 9,128 agnostic, 4,631 wave-wide only via `v_readfirstlane`, 12,325 wave-wide (9,329 of them via `v_mbcnt`). 445,906 of 529,146 wave ops sit in divergent regions (approximate, program order). Median 40 VGPRs and 66 SGPRs.
44. **Oracle:** 25,694 / 25,694 present, 0 missing, 854.8 MB DXIL in `D:\victory-data\rebuild-oracle\<hash>.dxil` (plus `.vsh` whole cache entries; 1.8 GB, outside git). Made by the fork's `translate --warm all` with the build that gave its 25,694/25,694 census (`wt-bisect\target\vn\diag`); the fork's `cache-fx` couldn't be used (its names hash a build fingerprint). 50 spot re-translations were byte-identical.
45. **Doc fix:** `tasks.md` and `secondary-agent.md` quote "4.64M instructions" as a corpus total. It's actually 4,641,843 *DXIL* instructions over the 1,433 world-frame programs; the corpus has 43.95M console instructions (40.61M without hints, against the fork's 40.56M, +0.13 %).
46. "25,694 programs" counts VS+PS+CS+MS only; the archive also has 195 ES and 195 GS.

From **audio** (details in `agents/audio.md`, PR #11):
47. **XMA decoder: decision needed.** Open decoders exist: FFmpeg's `wmaprodec.c` (LGPL-2.1+), and the fork's pure-Rust `crates/victory-formats/src/xma.rs` (GPL-3.0+, derived from FFmpeg's). There's no MIT/BSD one. A codec carries performance, so section 3 kept the lane from vendoring either. It plugs in through the `FrameDecoder` trait; until then XMA voices play silence with exact timing.
48. The fork's notes say the title's audio middleware reaches host WASAPI through COM itself. If so, A4's output is only for audio the layer mixes itself.
49. XApu now answers asynchronously ("pending" until ready); the fork decoded inline on the title's thread. The title polls, so this should be fine.
50. Numbers: workspace 128 green, 0 warnings. The slowest title-facing call is 27.6–68.1 µs (the first version had 4.6 ms and 3.8 ms stalls, now fixed). Opus 20 ms stereo packet: ~29 µs. The test tone played (440 Hz, 0.5 s, amplitude 0.05).
51. Unknown (marked in A1): the ApuAlloc flags, ApuCreateHeap size units, XApuConnect option bit 1, the DSP impulse-response layout (so XDsp output is silence for now), and pitch.

From **files** (details in `agents/files.md`, PR #12):
52. All 22 `files` overrides are built, with host signatures and host handles. The side-by-side script matches the host on **145 / 145** calls (return values, `GetLastError`, attributes, find data, `MoveFileExW` flags); the base folder hash is unchanged.
53. **Bind 5 more overrides** (built, last in `FUNCTIONS`): `FindNextFileW`, `FindNextFileA`, `FindClose` (search handles are ours because directories span overlay, mods and base), `SetFileAttributesW` and `GetTempPathW`. Until they're bound, the host gets our search handles and console paths, **and if `G:` or `T:` exists on the host, the host would act on real host files.**
54. **The host CRT bypasses every file override** (`fopen`, `_wfopen`, `_open`, `_wstat64`, `remove`, `_wrename`, `_wchdir`, `_wgetcwd`, …): it calls the host's own `CreateFileW`. These need overrides too (open through our `CreateFileW`, then `_open_osfhandle`).
55. Not overridden yet: `CopyFile2`, `CreateHardLinkW`, `CreateSymbolicLinkW`, `WerRegisterFile`. The census says 15 file functions, but the override list has 22 names; all 22 are built.
56. Wiring: `rebuild_winfiles::install(Files::new(vfs, r"G:\"))`, then `override_export(name)` in `HostResolver`'s override arm; read `discoveries()` with `modules::misses()`.
57. vfs gaps: delete-on-close and delete or rename through a handle leave the index stale; directory attributes and a directory case change below the overlay are refused and logged; `CREATE_ALWAYS` over a base file drops its attributes.

From **launcher** (details in the PR #13 body and `agents/launcher.md`):
58. The PR body carries the per-game config layout and **8 registry and config proposals** for `crates/config` (per-game settings, never replaced; see items 23–25).
59. One known nit: on the dashed Add tile, the subtitle is slightly wider than the dashed outline at 1280x800.

60. **Config, which matters for the owner's "settings never replaced" rule:** `Config::parse` drops unknown keys, and `Config::set` stores nothing for a value equal to the default, so **a later change of default silently moves that user's value**. The launcher works around both; the layer's `crates/config` should keep explicit values and unknown keys and add a `schema` key.
61. Proposed registry entries: `title.settings_source` (the title's `settings.xml` lives in its storage folder, not the game folder), `diagnostics.frame_log_path` (today `REBUILD_FRAME_LOG`), a per-setting `settings.xml` element name, and help text for `display.height` and `display.window`; plus supported titles listed in config rather than code.
62. Smaller: `rebuild.exe` ignores `--config` until C2; the VRAM check uses a provisional 8 GiB until the owner sets the minimum spec; `crates/config/src/lib.rs:349` checks the same thing twice. System check on this PC: 8/8 pass (RTX 4070 Ti, driver 610.88, 11.7 GiB, tiled tier 4, SM 6.8, GPU upload heaps, DXC 10.0.26100).

From **files** round 2 (details in `agents/files.md`, PR #16):
64. **Bind 45 names** through `override_export`: the 22 file overrides, the 5 from round 1, the 5 from F5, and the 13 CRT functions in `crt::CRT_FUNCTIONS`. Bind CRT names by function name, whatever `api-ms-win-crt-*` DLL they come from. `SetFileInformationByHandle` isn't imported, so route a `GetProcAddress` miss for it to `override_export` too. Wiring unchanged: `rebuild_winfiles::install(Files::new(vfs, r"G:\"))`.
65. **Behaviour change in round-1 code:** opening a read-only file to write or overwrite it now fails, as on the host. Before, the copy into the overlay cleared the read-only bit.
66. Chosen limits: Unicode `ccs=` modes write no BOM (logged; the title's `fopen` calls are byte streams); an invalid `fopen` mode fails with `EINVAL` and is logged instead of terminating the process; `_wchdir` changes only our current directory. Workspace 242 green, 0 warnings; round-1's 145-call comparison still has 0 differences.

From **audio** round 2 (details in `agents/audio.md`, PR #17):
67. **Licence check needed:** `wma/tables.rs` (WMA Pro code and level tables, data only, underivable) comes from the fork's `xma/tables.rs`, itself converted from FFmpeg's `wmaprodata.h` (LGPL-2.1+). It has the section 3 header and a `CREDITS.md` line; all decoding code is our own. LGPL-2.1-or-later may be used under GPL-3.0, but **confirm that vendoring this data is acceptable** under section 3.
68. Stereo XMA2 isn't checked against a reference: all of the title's XMA2 streams are mono, and nothing can encode stereo XMA2. It's written and passes the random-input test; add a stereo stream dumped from a run-time XMA context if one appears.
69. XMA voices now decode by default (`acp::Hal`). Codec 11 (most of `audio.rpf`) is the Opus framing `xapu.dll` decodes. The reference is FFmpeg 9.0.2's `xma2` (already installed via WinGet; nothing downloaded). Our output lines up 576 samples later than FFmpeg's (FFmpeg drops the first frame and trims 64).

From **gdk** (details in `agents/gdk.md` and `agents/gdk-slots.csv`, PR #18):
70. Call `rebuild_gdk::profile::configure(...)` before the title runs. Proposed config entry: `user.gamertag` (1–15 bytes, default `Player`). Title id and Store id come from `MicrosoftGame.config`; package size from the game folder.
71. Wiring: point `GetProcAddress(xgameruntime, name)` at `runtime::get_proc` (it answers the 6 names); `LoadLibraryExA("xgameruntime.dll")` needs a stand-in handle; add `rebuild_gdk::discovery::lines()` to the Build 3 discovery list.
72. **Seven interfaces in the exe's table aren't served yet:** game save, local storage, game UI, HDR, shell, capture and speech. `QueryApiImpl` refuses and logs each. The fork served them, so **the title may expect game saves at start-up**. The lane proposes a **G5** for these before the first start-up run.
73. Workspace 238 → 280 tests green, 0 warnings; the crate's tests passed 30/30 repeat runs, which caught and fixed a real block-reuse race.
74. **The game folder is changing; the owner says it is their own download** (14:50): a newer copy of the title is being written into a subfolder under `game_dir` (since ~14:27, multi-GB files still growing). No lane writes there. Until it finishes, any whole-folder "hash unchanged" check will fail for that reason alone, so scope such checks to the files a test actually opens, or re-baseline once the download completes. The lead should say which copy lanes target once it lands.

From the **coordinator** (launcher follow-up, PR #14):
63. **The launcher was not idle**: it requested a frame every 16 ms, so it drew at 60 fps even in the background beside a running game. It's fixed in PR #14: a scene clock that only runs when the window is focused and visible with no game running, `repaint_after()` (idle waits for input; motion at 30 fps; a running game's log read 4x a second, 1x minimised), and minimise on launch with restore on exit (`REBUILD_LAUNCHER_STAY_OPEN=1` keeps it open). **Measured: 4.5 % of one core in the foreground while drifting, 0.0 % minimised, 86–87 MB.** U6 (Add tile fit) is in the same PR.

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
| 14:50 | Owner: the game-folder writes are their own download. Item 74 updated. |
| 14:45 | **gdk** done: PR #18 open (G1–G4; G5 proposed for 7 unserved interfaces). **All round-2 lanes have handed back.** A newer copy of the title is being written into the game folder (not a lane); owner asked. |
| 14:40 | **audio A5** done: PR #17 open (our own XMA2 decoder, ±1 LSB on all 1,818 streams, ~1,200× real time); vendored LGPL-derived tables flagged for the lead. |
| 14:35 | **files F4–F5** done: PR #16 open (CRT 92/92, F5 46/46 against the host). |
| 14:30 | **shader S6** done: `oraclediff` PR #15 open (0 self-diffs over 25,694; all mutations caught; our own bitcode reader resolves 297,657 `dx.op` calls in a 1-in-50 audit). |
| 14:22 | Master 927942b: BCPack decided (our own decoder, tables outside git, decode cache) as dstorage D5, **run by the lead's own helper**; the coordinator does not start it. No other new work. |
| 14:20 | All 13 round-1 PRs merged. Round 2 started: **gdk** (new), **audio** A5, **files** F4–F5, **shader** S6. Launcher U6 and the idle fix done by the coordinator: PR #14. |
| 14:10 | **launcher** done: PR #13 open. **All 12 helper jobs have handed back**: 4 merged, PRs #5–#13 open for the lead. |
| 14:15 | **files** done: PR #12 open (145/145 match the host). |
| 14:12 | **audio** done: PR #11 open; XMA decoder choice flagged. |
| 14:08 | **shader** done: S1–S5 closed; PR #5 (S1–S4) and PR #10 (S5, stacked: merge #5 first). |
| 14:05 | **App restart at ~13:40** stopped launcher, audio and files mid-work (uncommitted work kept on disk); all three resumed. Launcher pass 3b reviewed: good, 4 polish items, then its PR. |
| 13:42 | **xg** done: X1–X4 closed, PR #9 open. |
| 13:40 | **upscale P3** done: FSR 3.1.6 beside DLSS, PR #8 open; licence checked compatible. |
| 13:36 | **loader L5** done: PR #7 open (76 run-time names; the GDK is reached through `xgameruntime!QueryApiImpl` objects, not by name). |
| 13:35 | Shader opened PR #5 (S1–S4); S5 continues. Launcher pass 3 reviewed: the grain and card layout are fixed, but a font bug shows only the first letter of each word; fix ordered. |
| 13:33 | **dstorage**: D1–D3 closed, D4 answered, PR #6 open. **launcher**: owner says "pretty broken"; fix-first pass 3 ordered. |
| 13:27 | Lead added lanes files, xg and loader L5 (master 5a26027). Started **files** and **xg**; the loader agent resumed for **L5** (fast-forward of agent/loader, new PR). |
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
