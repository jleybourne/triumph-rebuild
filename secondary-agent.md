# secondary-agent.md — helper agents, and taking over as lead

This file has two jobs:

1. **Helper lanes:** let helper agents work beside the lead session without breaking it (sections 1-7).
2. **Handover:** let a new session take over as **lead** when the owner hands the whole job over
   (section 9).

It is updated by the lead after every step. Section 8 is always the current state.

```
lead session   owns the main line (crates/uma, crates/xlist, crates/sir, crates/config), measurements, shared docs
helper agents  own one lane each, in their own worktree, branch and crate; hand back by pull request
coordinator    the session that starts the helpers; keeps agents/secondary-agents-updates.md
```

**Where agent updates go:** [`agents/secondary-agents-updates.md`](agents/secondary-agents-updates.md). The
coordinating session keeps it: each lane's state, its pull requests, anything flagged "for the lead", and a
log. The lead reads it before every step and never edits it.

## 1. Before you start (every helper agent)

Read, in this order: `README.md`, `plan.md` (the six decisions), `tasks.md` (your lane's build),
`progress.md`, then this file again. Then:

```powershell
cd D:\Developer\triumph-rebuild
git fetch origin
git worktree add ..\triumph-rebuild-<lane> -b agent/<lane> origin/master    # <lane> = shader, launcher, upscale, audio, dstorage, files, xg, or loader (L5)
cd ..\triumph-rebuild-<lane>
cargo test --release                                                         # must be green before you change anything
```

Work **only** in that worktree. Never `git checkout` or `git switch` in `D:\Developer\triumph-rebuild`.
That is the lead's working tree, and switching it breaks a run in progress.

## 2. The rules that keep the lead's work safe

| rule | why |
|:--|:--|
| **Do not edit** `crates/uma/**`, `crates/xlist/**`, `crates/sir/**`, `crates/config/**` (lane `launcher`: propose registry additions in `agents/launcher.md`), `docs/**`, `apps/rebuild/src/bin/{gputest,uploadbench,listbench,sirtest}.rs`, `plan.md`, `tasks.md`, `progress.md`, `README.md`, `HANDOFF.md`, `REVIEW-PROMPT.md`, `runs/**`, `secondary-agent.md` | the lead is changing these right now; any edit becomes a merge conflict |
| You **may** add one line to the root `Cargo.toml` `members` list for your crate | the lead resolves that one line on merge |
| **Measurement lock:** while `D:\Developer\triumph-rebuild\agents\MEASURING` exists, run **no** builds, tests or other heavy work. Check before every build: `if (Test-Path D:\Developer\triumph-rebuild\agents\MEASURING) { "wait" }` | the lead's GPU and CPU timings need a quiet machine. One background build once turned 7.3 fps into 2.3. |
| Build with `cargo build --release -j 4` | leaves cores free for the lead |
| **No GPU work**, except lane `upscale` and the launcher's system check, and **only while `agents\MEASURING` is absent**. Never `gputest`, `sirtest` or the benches | GPU timings are the lead's; a second device during a run skews them |
| Push only `agent/<lane>`. Never push `master`, never force-push, never rewrite history | the lead merges |
| **No game bytes in git**: no shader binaries, no disassembly of title shaders, no title files, no paths to them in committed code (read the path from config at run time) | asset policy; this repository holds no game content |
| Every task ends in a test or a number, never "done because it compiles" | the project's first rule |

## 3. Licences (GPL-3.0-only workspace)

- **`D:\Developer\xcl`** is GPL-2.0: **read for design only, never copy** a line.
- **The owner's fork** (`D:\Developer\fxceless-victory`, remote `triumph`, branch `triumph/main`, crates
  `victory-*`) is **GPL-3.0-or-later**. You may vendor **only code that carries no performance**:
  container and archive parsing, PE parsing, instruction decode tables. Every vendored file gets this
  header:
  ```rust
  // Derived from VIctory (Marvisak and contributors) via jleybourne/Triumph, GPL-3.0-or-later.
  // Used here under GPL-3.0-only. Changes: <what you changed>.
  ```
  Also add a line to `CREDITS.md` (create it in your branch if it does not exist).
- **Shader IR, lowering and emission are ours** (plan decision 4, built in `crates/sir`). Do not vendor
  them or work on them; they are the lead's.
- Read the fork through git without checking it out: `git -C D:\Developer\fxceless-victory show triumph/main:<path>`.

## 4. The game data

The title is at `game_dir` in `D:\Developer\fxceless-victory\victory.toml` (shaders are in its `shaders.rpf`,
411 MB). Read that path at run time from an environment variable, `REBUILD_GAME_DIR`; never hard-code it in
committed code. Treat the game folder as **read-only**: never write into it, and hash it before and after
a run if your tool opens files there.

## 5. The lanes

### Lane `loader` — Build 3, first half: PE loader and the import census

New crate `crates/loader` (package `rebuild-loader`), plus a binary `apps/rebuild/src/bin/imports.rs` (a
new file, so it's allowed).

| # | task | closes when |
|:--:|:--|:--|
| L1 | PE32+ parse: headers, sections, relocations (DIR64), imports by name and ordinal, delay-load, TLS callbacks, exception tables (`.pdata`) | unit tests on synthetic images; parses every PE in `C:\Windows\System32` without a panic (count reported) |
| L2 | map an image into memory at a chosen base, apply relocations, resolve imports through a resolver trait (no execution) | a test DLL built in the test maps and its relocated pointers check out |
| L3 | **import census** over the title's folder: every DLL and function the title's images import, grouped by DLL, and which are missing from a resolver that knows nothing yet | `imports` prints the table and writes it to `agents/loader-imports.csv` (names only, no bytes); the count of distinct functions is the GDK discovery list for Build 3 |
| L4 | the title's own DLLs: list them (Bink, the FidelityFX loader, HTTP), with their exports | table in `agents/loader.md` |

Do **not** execute title code, create threads for it, or implement kernel calls. That is Build 3's
second half, and it needs the lead's memory model.

### Lane `shader` — Build 4 front end: the census and the decoder

New crate `crates/shader` (package `rebuild-shader`), plus a binary `apps/rebuild/src/bin/census.rs` (a new
file).

| # | task | closes when |
|:--:|:--|:--|
| S1 | read `shaders.rpf` and every shader container in it (DXBC/DXIL with the Xbox `XDAT` additions; the GFX10 code is zlib-compressed inside). Vendoring the fork's `victory-formats` / `victory-shader::dxbc,dxil` parsers is allowed (section 3) | `census` lists **25,694** programs by hash |
| S2 | census row per program: hash, stage, GFX10 code size, instruction count, uses wave64 (yes/no), uses DPP / LDS / scratch / 16-bit, **and the wave class** (`agnostic` or `wave-wide`, by the rules in `crates/sir/src/classify.rs`: any reduction, ballot, readlane, readfirstlane of a non-uniform value, DPP/swizzle beyond a quad, or a lane id seen beyond its quad bits) | `agents/shader-census.csv` (hashes and counts only); totals match the fork's (25,694 programs; 1,433 in a world frame; 43.95M console instructions, the fork's 40.56M without hints) where comparable |
| S3 | GFX10 (RDNA2) decoder: every encoding the corpus uses (SOP1/2/C/K/P, SMEM, VOP1/2/3/3P/C, DPP, SDWA, DS, MUBUF, MTBUF, MIMG, FLAT/GLOBAL/SCRATCH, EXP). Vendoring the fork's `gfx10` opcode tables is allowed | **byte-exact re-encode** of every instruction in all 25,694 programs; 0 unknown opcodes, or a named list |
| S4 | per-program feature summary for the lead: register counts, branch structure (structured or not), wave-uniform vs per-lane branch share, and the count of wave ops inside divergent branches (these become predication) | a section in `agents/shader.md` |

| S5 | **the oracle set**: for each program hash, the fork's translated output (its shader cache under `D:\victory-data\cache-fx`, or by running the fork's offline `translate` command; read its CLI with `git show triumph/main:apps/victory/src/main.rs`). Write it to `D:\victory-data\rebuild-oracle\<hash>.dxil`, **outside git** (game-derived bytes), plus an index `agents/shader-oracle.csv` (hash, present yes/no, size) | the index covers 25,694 hashes; the missing ones are named |

Stop after S5. Translating decoded GFX10 into `sir` is the lead's (section 8).

### Lane `launcher` — the desktop launcher (design: `docs/design/launcher.md`)

New app `apps/launcher` (package `rebuild-launcher`), egui/eframe. It depends on `crates/config` (read
only: the registry is the lead's; propose new options in `agents/launcher.md`). No network crates, no
telemetry, no browser engine.

| # | task | closes when |
|:--:|:--|:--|
| U1 | shell: tabs Play · Display & upscaler · Title settings · Features · Mods · Diagnostics · Profiles · About. **Every settings widget is generated from `rebuild_config::REGISTRY`**: bool → checkbox, int → slider in range, choice → dropdown, path → picker; help text, live/restart badge, experimental warning, reset to default | saves a `rebuild.toml` that `Config::parse` reads with 0 problems; a test round-trips every registry entry through the UI model |
| U2 | first-run setup and system check: game folder, GPU and driver, tiled resources tier ≥ 2, shader model 6.6, DXC present, VRAM, ReBAR, each with a one-line fix. Querying the adapter through DXGI is fine; **creating a D3D12 device needs the measurement lock to be absent** | each check reports pass/fail; checks are unit-tested with fakes |
| U3 | title settings: show `title.*` registry entries; on launch, write them into a **copy** of the title's `settings.xml` under the launcher's own overlay folder (never the game folder or the title's storage) | the original file's hash is unchanged (test) |
| U4 | mods tab: list `mods/<name>/` folders, enable/disable, reorder, and show conflicts (two enabled mods with the same relative path: the later wins) | conflict detection unit-tested |
| U5 | Play: [Launch] starts `rebuild.exe` with `--config <path>`; shows the frame-line log file (`diagnostics.frame_log`) as it grows. Test the launch path against a **fake child process**; helpers never run `gputest` (the lead checks the real launch) | launch and log tail work against the fake child, in tests |

Launch-time integration with the real layer (C2) and the in-game menu (L5) are the lead's. The mods tab
uses lane `vfs`'s crate once it merges; until then, detect conflicts by walking the folders itself.

### Lane `vfs` — the overlay file system (Build 3 piece; feeds the launcher's mods tab)

New crate `crates/vfs` (package `rebuild-vfs`). Pure Rust, no GPU, no title code.

| # | task | closes when |
|:--:|:--|:--|
| V1 | console path → host path: the title's drive prefixes and its storage paths (the fork's `victory-hle` files module is the reference for which prefixes the title uses: read, don't copy) mapped onto a read-only base (the game folder) | unit tests per prefix; case-insensitive lookup like the console's |
| V2 | a **write overlay**: every create, write, rename or delete lands in an overlay folder (e.g. `%APPDATA%\triumph-rebuild\overlay`); reads see overlay-then-base | a test hashes the base folder before and after a write-heavy run: unchanged |
| V3 | **mods**: ordered layers between overlay and base (`mods/<name>/`, later wins); `conflicts()` lists every path more than one enabled mod provides, and which wins | unit tests: order, enable/disable, conflicts |
| V4 | a file-handle API the runtime will call (open, read at offset, size, enumerate a directory, close), with no global state | tests; a benchmark of open+read on 10,000 small files (number in `agents/vfs.md`) |

### Lane `audio` — the console's audio hardware (Build 3 piece)

New crate `crates/audio` (package `rebuild-audio`). No GPU. Read `runs/build3-runtime-census.md` first. The
title imports 19 functions from three console-only DLLs: `acphal.dll` (`AcpHalCreate`,
`AcpHalAllocateShapeContexts`, `AcpHalReleaseShapeContexts`, `ApuAlloc`, `ApuFree`, `ApuCreateHeap`,
`ApuIsVirtualAddressValid`, `ApuMapApuAddress`, `ApuMapVirtualAddress`), `xapu.dll` (`XApuConnect`,
`XApuDisconnect`, `XApuEnqueueCommand`, `XApuDequeueResult`, `XApuGetResultCount`) and `chad.dll`
(`XDspActivate`, `XDspDeactivate`, `XDspConnectWithMaximumStreamLimit`, `XDspDisconnect`, `XDspSubmitCommand`).
The fork's `victory-hle` audio and APU modules are the reference for what they do (read, don't copy).

| # | task | closes when |
|:--:|:--|:--|
| A1 | what each function does and what the title passes (from the fork's design and any run logs it has), in `agents/audio.md` | every one of the 19 has a stated meaning, or is marked unknown |
| A2 | the APU memory functions (`ApuAlloc` / `ApuFree` / `ApuCreateHeap` / `Apu*Address`) over ordinary host memory, address round trips exact | unit tests |
| A3 | the command queues (`XApu*`, `XDsp*`) as host-side queues; the decode work they carry (XMA / Opus / shape contexts) on the CPU through a host decoder where one exists (an MIT/BSD Opus decoder is fine; none is committed with title data) | a decode test on synthetic input; results dequeued in order |
| A4 | output to the host through WASAPI, in a thread of its own; no audio work on the title's threads | a test tone plays; no call blocks longer than 1 ms (measured) |

Signatures are the lead's to bind (`crates/runtime` resolves imports); expose plain Rust functions with the
console argument layout documented, and the lead wires them.

### Lane `dstorage` — DirectStorage (Build 3 piece)

New crate `crates/dstorage` (package `rebuild-dstorage`). No GPU at first. The title imports one function,
`xcrdapi.dll!DStorageGetFactory`. The fork found its requests use **zlib** and **BCPack**, and that BCPack output
is in the console's tiled texture layout (`docs/design/gpu-textures.md` on the fork).

| # | task | closes when |
|:--:|:--|:--|
| D1 | the console DirectStorage factory, queue and request layouts the title uses (from the fork's design and run logs), in `agents/dstorage.md` | each interface method the title calls is named, by vtable slot |
| D2 | requests served from files through `crates/vfs` (merged), on worker threads; completion through a fence or status array the title polls | ordered completion test on synthetic files |
| D3 | zlib decompression (a Rust crate under MIT/Apache/zlib is fine) | round-trip and throughput number in `agents/dstorage.md` |
| D4 | BCPack: what it is and whether it can be decoded on the CPU or must be on the GPU; the fork's notes first | a written answer with evidence; a decoder only if it is plain code with no performance cost on the GPU side |

### Lane `files` — the 15 file overrides (Build 3 piece)

New crate `crates/winfiles` (package `rebuild-winfiles`). No GPU. Read `runs/build3-runtime-census.md`. The title
imports 15 host file functions whose paths must go through the overlay file system (`crates/vfs`, merged):
`CreateFileW`, `CreateFile2`, `CreateFileA`, `DeleteFileW`, `DeleteFileA`, `CreateDirectoryW`, `CreateDirectoryA`,
`RemoveDirectoryW`, `MoveFileExW`, `GetFileAttributesW`/`A`, `GetFileAttributesExW`/`A`, `FindFirstFileW`/`ExW`/`A`/`ExA`,
`GetFullPathNameW`/`A`, `GetDiskFreeSpaceExW`, `Get`/`SetCurrentDirectoryW` (the list in `crates/runtime/src/lib.rs`
`OVERRIDES`, "files").

| # | task | closes when |
|:--:|:--|:--|
| F1 | each as an `extern "system"` function with the host's exact signature, translating console paths through `vfs` and returning **host handles** (so `ReadFile`, `CloseHandle` and the rest stay the host's) | unit tests per function |
| F2 | behaviour matches the host's on the same files: error codes (`GetLastError`), attribute bits, find-data structures, `MoveFileExW` flags | a test calls the override and the host function side by side on a temporary tree and compares |
| F3 | a path the title uses that `vfs` does not know is logged as a discovery entry, never guessed | test |

The lead binds them in `crates/runtime` once merged.

### Lane `xg` — the console texture layout (`xg_xs.dll`; Build 5 piece)

New crate `crates/xg` (package `rebuild-xg`). No GPU at first. The title imports `XGCreateTextureComputer` and
`XGSuggestSwizzleMode`: the console's texture layout calculator (tile modes, swizzle, mip and slice offsets). The
fork's `docs/design/gpu-textures.md` and its texture code are the reference (read; decode-only maths may be vendored
with credit, section 3).

| # | task | closes when |
|:--:|:--|:--|
| X1 | the layouts the title uses (from the fork's notes and logs): which tile/swizzle modes, which formats | table in `agents/xg.md` |
| X2 | address maths: (x, y, slice, mip) → byte offset for each mode, and the inverse | round-trip unit tests over every texel of small textures in every mode |
| X3 | the texture computer's interface as the title calls it (vtable slots used) | named in `agents/xg.md` |
| X4 | a CPU detiler: console layout → linear, for the formats in X1 | tested against the fork's detiled output on synthetic data |

### Loader L5 — names the title looks up at run time

On branch `agent/loader` (a new pull request): the GDK functions (`XGame*`, `XUser*`, `XAsync*`, `XTaskQueue*`, …) are not
imported, so the title reaches them by `GetProcAddress`. Scan the title's images for ASCII and UTF-16 strings that look
like API names next to the DLL names they would come from, and list them (names only) in `agents/loader-getproc.csv` with
a confidence column. This is an early estimate of what discovery will find at run time.

### Lane `upscale` — the DLSS and FSR backends, standalone (Build 7 piece)

New crate `crates/upscale` (package `rebuild-upscale`), plus a binary `apps/rebuild/src/bin/upscaletest.rs`.
**GPU allowed** only while `agents\MEASURING` is absent; check before each run.

| # | task | closes when |
|:--:|:--|:--|
| P1 | one interface, from plan decision 5: inputs colour, depth, motion vectors, jitter, exposure, reset, render size, output size, mode | the trait, with docs on every field's convention (jitter sign, motion-vector scale) |
| P2 | a synthetic test scene, rendered by the test itself: moving geometry, Halton jitter, motion vectors and depth at a render size below the output | the scene renders; vectors are checked against analytic motion |
| P3 | **FSR** through AMD's FidelityFX SDK (MIT), FSR 3.1 path; it runs on the NVIDIA card | output frames written as PNG to a folder outside git; no NaN or black; a temporal-stability number (frame-to-frame difference on a static camera) |
| P4 | **DLSS** through NVIDIA Streamline (MIT headers). The DLSS runtime DLL is never committed: its path comes from the registry entry the lead adds (propose `upscaler.dlss_dll` in `agents/upscale.md`) | same checks as P3; DLSS and FSR side by side on the same scene |
| P5 | the conventions table: jitter sign, motion-vector units, depth (reversed or not), what "reset" means, for both SDKs | in `agents/upscale.md`; the lead uses it when wiring the title's pass in Build 7 |

### Lane `input` — controller and keyboard (Build 3 piece; the in-game menu needs it)

New crate `crates/input` (package `rebuild-input`). No GPU.

| # | task | closes when |
|:--:|:--|:--|
| I1 | XInput pads (slots 0-3) and a keyboard stand-in pad (the fork's mapping in its README "Running it" section is the reference: WASD / IJKL sticks, arrows D-pad, Enter / Backspace / X / Y faces, Q / E shoulders, Z / C triggers, Esc / Tab Menu / View) as one `PadState` | unit tests with a fake source |
| I2 | **menu isolation** (the fork's `docs/design/debug-menu.md` A1): while the menu is open the title sees a neutral pad and no keys; on close, neutral until every key and button is up; rumble stopped on open | unit tests: open, hold, close-while-held, all-up |
| I3 | which GameInput functions the title imports (lane `loader`'s `agents/loader-imports.csv`, once merged) mapped onto `PadState` | a table in `agents/input.md` |

### Round 2 (2026-11-27, the second lead session)

Every round-1 lane has handed back and is merged. Lanes `loader`, `vfs`, `input`, `upscale` and `xg`
have no new work: stop them. `dstorage` D5 is run by the lead's own helper agent, not the coordinator. The rules of sections 1-4 are unchanged. Each task ends in a test or a number.

#### New lane `gdk` — the GDK runtime behind `xgameruntime.dll` (Build 3 piece)

New crate `crates/gdk` (package `rebuild-gdk`). No GPU, no title code. Loader L5 found that the title takes 6 entries
from `xgameruntime.dll` by name (`InitializeApiImpl`, `InitializeApiImplEx`, `InitializeApiImplEx2`,
`UninitializeApiImpl`, `QueryApiImpl`, `XErrorReport`) and reaches every GDK area through objects `QueryApiImpl`
returns. The fork's `victory-hle` GDK modules and its notes are the reference (read, don't copy). This is likely
what the title's startup waits on.

| # | task | closes when |
|:--:|:--|:--|
| G1 | the entry points' arguments and results, every interface the title asks `QueryApiImpl` for (by GUID), and each interface's vtable slots, from the fork's design and run logs | a table in `agents/gdk.md`; unknowns marked, never guessed |
| G2 | **XTaskQueue and XAsync** as host code with the console's structure layouts: queues, work and completion ports, dispatch modes (thread pool, manual, immediate, serialized), `XAsyncBlock` lifetime, cancellation, `XAsyncGetStatus` waits. **No call blocks the caller beyond what the GDK documents** | tests: ordering, manual dispatch, callbacks on the right port, cancel, terminate with pending work |
| G3 | **XGame, XUser, XStore/XPackage, XSystem**: one signed-in local user (gamertag from config), the title owned and fully installed, no network (network calls fail with the GDK's offline error, cleanly). Each call the fork logged is answered | unit tests per function; an unknown call is logged as discovery |
| G4 | a table `(interface, slot) → Rust function` the lead wires into `QueryApiImpl`, with each function's console argument layout documented | the table, and a test that every listed slot resolves |

#### Lane `audio` — A5, the XMA decoder

| # | task | closes when |
|:--:|:--|:--|
| A5 | **our own XMA2 decoder** behind `FrameDecoder`. FFmpeg's `wmaprodec.c` and the fork's `xma.rs` are design references only (read, don't copy: a codec carries performance). Test streams are read at test time from the title's folder through `REBUILD_GAME_DIR`, never committed | output equals a reference decode within ±1 LSB (FFmpeg if `ffmpeg.exe` is already on disk; if not, say so and the lead decides), and a speed number: real-time factor for one stereo stream on one thread (aim ≥ 50×) |

#### Lane `files` — F4 and F5

| # | task | closes when |
|:--:|:--|:--|
| F4 | the **CRT file functions** the title imports (check `agents/loader-imports.csv`: `fopen`, `_wfopen`, `_open`, `_wopen`, `_wstat64`, `remove`, `_wrename`, `_wchdir`, `_wgetcwd`, …): open through our `CreateFileW`, then `_open_osfhandle` / `_fdopen`, so CRT file I/O sees the overlay | a side-by-side test against the host CRT, like F2 |
| F5 | `CopyFile2`, `CreateHardLinkW`, `CreateSymbolicLinkW` (into the overlay, or refused with the host's error), `WerRegisterFile` (succeeds, logged); and the vfs gaps from item 57: delete-on-close, rename or delete through a handle (`SetFileInformationByHandle`), directory attributes. Editing `crates/vfs` is allowed for this | tests; the base folder hash is unchanged |

#### Lane `shader` — S6, the oracle diff tool (not translation)

| # | task | closes when |
|:--:|:--|:--|
| S6 | `oraclediff`: given our DXIL for a hash and the oracle's (`D:\victory-data\rebuild-oracle`), compare what must match: input and output signatures, resource bindings and spaces, thread-group size, wave-size attribute, and a per-category instruction histogram (memory, wave ops, control flow). Report per hash to a CSV outside git. **No GPU**: the lead adds the execution check | the oracle against itself gives 0 diffs over 25,694; injected mutations (a binding moved, a signature element dropped, a wave op removed) are each caught |

#### Lane `dstorage` — D5, BCPack (owner-approved 2026-11-27)

The owner approved: our own decoder, the ~78 KB format tables loaded at run time from a file **outside git**, and a
cache of decoded output so each texture is decoded once. `agents/dstorage.md` D4 is the background.

| # | task | closes when |
|:--:|:--|:--|
| D5a | load the tables from the path in `REBUILD_BCPACK_TABLES` (the lead adds the registry entry `dstorage.bcpack_tables`); a missing or wrong-sized file keeps BCPack refused with a clear log line, never a crash. Record where the file comes from and its SHA-256 in `agents/dstorage.md`; never commit it | tests with synthetic tables; the real file's hash recorded |
| D5b | **our own decoder** behind the `BcPack` trait: rANS lanes, models, back references, dictionaries, endpoint and index residuals, BC1/3/4/5/6H/7. The fork's `bcpack.rs` is a design reference (read, don't copy). Output stays in the console's tiled layout | on the title's real BCPack requests (read at test time, nothing committed), output equals the fork's decoder block for block, and **every difference is explained** (the fork's BC3/5/7 paths are suspects for red-block artefacts); MB/s per thread per format, aiming above the fork's 33-66 |
| D5c | **the decode cache**: decoded output in `%LOCALAPPDATA%\triumph-rebuild\bcpack-cache`, keyed by a hash of the compressed bytes and the format, written atomically, read back on the next request instead of decoding; a size cap with oldest-first eviction | tests: hit, miss, a corrupted or partial entry ignored and rebuilt; the hit-path MB/s against decode MB/s |

#### Lane `launcher` — U6

| # | task | closes when |
|:--:|:--|:--|
| U6 | the dashed Add tile's subtitle fits its outline at 1280x800 (item 59). Then stop until the lead's C2 (launch through `--config`) and the `crates/config` changes in section 10 land | a screenshot outside git |

## 6. Handing back

1. Keep `agents/<lane>.md` in your branch: what you built, the numbers, what's left. Use the same voice
   as `progress.md`: built / measured / decided / next.
2. Commit small, with messages that say what was measured. End each with the attribution line your session
   gives you.
3. Push `agent/<lane>` and open a pull request to `master` titled `agent/<lane>: <what closed>`. **Do not merge it.**
4. The coordinator records the lane's state and the pull request in
   [`agents/secondary-agents-updates.md`](agents/secondary-agents-updates.md).
5. The lead reviews, merges, writes the `progress.md` entry (one per upload, with its own bar), and
   updates `tasks.md` and section 8 here.

## 7. If something looks wrong in the lead's code or documents

Don't fix it. Write it in `agents/<lane>.md` under "for the lead", with the file, the line and the
evidence. The coordinator adds a one-line pointer under "For the lead" in
`agents/secondary-agents-updates.md`, and the lead answers it in the next step.

## 8. Current state (updated by the lead every step)

```
updated   2026-11-27, second lead session: all 11 helper PRs merged (#1-#13); round 2 of the lanes in section 5
progress  46 % (progress.md) · shaders 0 / 25,694 counted (all decode byte-exact; they count once translations match the oracle)
tests     238 green (4 ignored), 0 warnings: cargo test --release
```

| build | state | where | proof |
|:--|:--|:--|:--|
| 0 (a) upload cost | ✅ | `crates/uma/src/upload.rs`, `uploadbench` | [runs/build0a-upload.md](runs/build0a-upload.md) |
| 0 (b) translation cost | ✅ | `crates/xlist`, `listbench` | [runs/build0b-lists.md](runs/build0b-lists.md) |
| 0 (c) title GPU time | ⏸️ needs a frame (Build 5) | — | — |
| 1 tiles at console addresses | ✅ | `crates/uma/src/{lib,tiles,arena}.rs`, `gputest --tiled/--arena/--scale` | [runs/build1-tiles.md](runs/build1-tiles.md) |
| 2 upload ring + labels | ✅ | `crates/uma/src/{ring,labels}.rs`, `gputest --ring/--labels` | [ring](runs/build2-ring.md) · [labels](runs/build2-labels.md) |
| 3 loader + runtime | 🔄 loader ✅ (PR #1), files ✅ (`vfs`, PR #3), input ✅ (PR #2); kernel ⏳ | `crates/loader`, `imports` | `agents/loader.md`, `agents/loader-imports.csv` |
| 4 shaders | 🔄 core: design, lane model, HLSL emission proven on the card; front end in lane `shader` | `crates/sir`, `sirtest`, [design](docs/design/shader-ir.md) | [runs/build4-sirtest.md](runs/build4-sirtest.md) |
| 5 device, lists, present | ⏳ (translator core done in `crates/xlist`) | — | — |
| 6 list translation | 🔄 core built | `crates/xlist` | Build 0 (b) |
| 7 upscaler | ⏳ (plan decision 5 corrected) | — | — |
| 8 pacing · 9 AMD testers | ⏳ | — | — |
| helper lanes | live status: [`agents/secondary-agents-updates.md`](agents/secondary-agents-updates.md) | merged: all of round 1 (#1-#13). **Round 2 (section 5): new lane `gdk` (G1-G4), `audio` A5, `files` F4-F5, `shader` S6, `launcher` U6** | their pull requests |
| launcher track | 🔄 C1 registry built; launcher app in lane `launcher` | `crates/config`, [design](docs/design/launcher.md) | 6 unit tests |

## 9. Taking over as lead

If the owner hands you the whole job, you are the lead. **Start with [`claude-max-handoff.md`](claude-max-handoff.md)** (the in-flight step and its next actions). Then read `README.md`, `plan.md`, `tasks.md`,
`progress.md` (top block and the latest three entries), `docs/design/shader-ir.md`, this file, and
`agents/secondary-agents-updates.md` (what the helper lanes have done and flagged). Run
`cargo test --release` (must be green) and `cargo run --release --bin gputest -- --bare` (the control).

### How the owner wants the work done

- **Keep building until there are no tasks.** Don't stop to offer menus.
- **Before every step, read `agents/secondary-agents-updates.md`.** Merge any open lane pull request first,
  and answer every "For the lead" item.
- **Decide technical questions yourself, choosing whatever performs best.** Record each decision and its
  reason in `plan.md` and `tasks.md`. Stop only for owner-only items (below).
- **Every push adds a new dated entry at the top of `progress.md` "Updates"**, with its own bar
  (`[████░…]` 20 cells, 5 % each), what moved the percentage, and notes: built / measured / decided /
  learned / next. Never edit older entries. In the same push, update the top block, the per-build table,
  the README bar and badges (`progress-NN%25`, tests count), `tasks.md`, `runs/README.md`, and section 8
  of this file.
- **Commit to `master` and push** (`git push origin master`), with the attribution line your session gives
  you. Small commits whose messages say what was measured.
- **Every claim has a run.** Proofs are binaries (`gputest --…`, `sirtest`, benches) with a write-up in
  `runs/<build>-<name>.md` and the raw output beside it in `.txt`.
- **Measurement lock:** create `agents\MEASURING` before any timed or GPU run and delete it after, so helper
  agents pause.
- **The fork is data, not code:** its docs, numbers and translations (as an oracle) are fine to use. Vendor
  only code that carries no performance, with credit. xcl: never copy.
- **No game bytes in git.**
- Tip: edit several docs in one Python script file in the scratchpad. Long bash heredocs with quotes broke once.

### The next steps, in order

| # | step | closes when |
|:--:|:--|:--|
| 1 | ✅ `sir`: live-mask analysis (done: 601 GPU runs exact, 182 host branches + 124 predicated) | — |
| 2 | ✅ `sir`: memory lowering and native 16-bit (done: 342 loads + 331 stores through tiled windows and 650 half ops exact on the card) | — |
| 3 | merge lane `shader` (S1-S4), then GFX10 → `sir`: CFG, SSA, structurize; agnostic programs first | census column "emitted"; the shader bar in `progress.md` starts moving |
| 4 | oracle diff harness against the fork's translations, per hash | programs count toward 25,694 only when they match |
| 5 | ✅ lanes `loader`, `vfs`, `input` merged; ✅ runtime census (89.5 % host). Build 3's second half, now 73 functions: kernel (threads, events, waits, timers, TLS), files with a write overlay, the GDK discovery loop from `agents/loader-imports.csv` **plus every `GetProcAddress` miss** | the title's startup path runs to its first render call |
| 5b | ✅ the title's images map and bind: 97.5 % (`maptitle`). Next: D3D12.X device creation (`d3d12_xs`, 5), then run the title's startup to its first render call | the startup path reaches a render call |
| 6 | Build 5: D3D12.X device, lists via `crates/xlist`, descriptor heaps with a dirty list, present, **the frame line**; then Build 0 (c) (per-list timestamps, RT on and off) and direct read vs ring per resource | a non-black frame; the first measured fps |
| 7 | Build 7: find the title's upscale pass by bindings; lower the internal resolution through tiles; DLSS (Streamline) and FSR (FidelityFX SDK) | correct picture at a lower internal resolution |
| 8 | Build 8 pacing → **30 fps on NVIDIA**; Build 9 AMD testers | the frame line |

### Owner-only items (ask, don't decide)

- The minimum spec in `plan.md` (GPUs, VRAM, ReBAR, output resolution, upscaler mode), needed before Build 7.
- AMD testers for Build 9.
- Anything published outside this repository and the owner's own GitHub.
- Whether to push the fork's local `dlss-hook` branch (commit `ee0b94a3`) so the docs can link to it.

## 10. The lead's answers to "For the lead" (newest first)

**2026-11-27, second lead session, to items 26-62** (`agents/secondary-agents-updates.md`). PRs #5-#13 are all merged.

- `dstorage` 26-27: agreed. The lead binds `DStorageGetFactory` and writes `Pages` over `runtime::physical` with a
  retry for pages the title hasn't made yet. 28, BCPack: **the owner decided** (D5): our own decoder, tables
  from a file outside git, and a decode cache.
- `loader` 30-32: agreed. `GetProcAddress` answers by name for `xgameruntime` (6), `gameplatformservices` (3),
  `WinHttpWebSocket*` (6) and `icu` (14). A load of `amd_fidelityfx_framegeneration_xbox.dll` fails cleanly. The GDK
  areas behind `QueryApiImpl` are the new lane `gdk`.
- `upscale` 33-37: agreed. The lead adds `upscaler.fidelityfx_dir` beside `upscaler.dlss_dll` and
  `upscaler.streamline_dir`. FSR 4: **prefer the highest FSR the driver offers** (FSR 4 on RDNA 4), with 3.1 as
  the fallback. Lane finished.
- `xg` 38-42: agreed. The lead binds the 13-slot vtable, slot 0x30 returning 0 outside the texture; GPU
  detiling from `Layout::tables` is Build 5. Metadata planes stay unsized until a run shows they matter. Lane finished.
- `shader` 43-46: the doc fix is taken. S2's "4.64M instructions" is DXIL over the 1,433 world programs, not the
  corpus's 43.95M console instructions. Next: S6 (above). Translation stays the lead's.
- `audio` 47: **decided: our own XMA decoder (A5)**, not vendored, because it carries performance. 48: agreed:
  A4's output is for audio the layer mixes itself. 49: fine. 51: noted; each unknown is settled by a run.
- `files` 53: the lead binds all 22, the 5 extra first. 54-55, 57: F4 and F5 (above).
- `launcher` 58, 61: the per-game layout and the registry proposals are accepted; the lead adds them to
  `crates/config`. 60: agreed and important for the owner's rule: `crates/config` will **keep explicit values even
  when equal to the default, keep unknown keys, and gain a `schema` key**. 62: the double check at
  `crates/config/src/lib.rs:349` is the lead's to fix. U6 (above).

**2026-11-27, to lane `upscale` (PR #4, merged):** the FidelityFX SDK download is approved (see item 7 below):
pinned version, official GPUOpen GitHub release, extracted outside git, SHA-256 recorded. Then do P3, and confirm the
FSR column of the conventions table with the same sweep as DLSS.

**2026-11-27, to lanes `input` (PR #2) and `vfs` (PR #3), both merged:**

- `input`, poll cost: agreed. Input is polled on a thread of its own, and the present path reads the last
  `Polled` (L5 in `tasks.md`).
- `input`, I3 read the CSV before PR #1 merged: fine. It is merged now; nothing to change.
- `input`, F / H stick clicks: kept.
- `vfs` 1, hash tests: agreed. Every "hash before and after" check reads `fs::metadata(path)` per file. Lane
  `launcher`: apply this to U3.
- `vfs` 2, drives: decided. `G:` is the game package, read-only. The title's storage drive is its own root.
  **`T:` starts empty every run** (the console's behaviour, so no stale state between runs).
- `vfs` 3: `tasks.md` Build 3 files now points at `crates/vfs`, closed by `v2_game_folder_unchanged`.

**2026-11-27, to the coordinator's items 1-8** (`agents/secondary-agents-updates.md`, 13:05):

1. **FidelityFX loader:** right. `plan.md` decision 5 now says the exe imports the loader statically (mapped at
   start), but as a Series X the title never calls into it to upscale, and nothing imports the upscaler DLL.
2. **`GetProcAddress`:** right. Build 3's discovery loop logs every `GetProcAddress` miss as well as import
   misses (`tasks.md`, section 9 step 5).
3. **Test count:** README updated: 59 with `loader` merged.
4. **Build warnings:** fixed (`tileproof.rs`, `upload.rs`, and one in `sir/gen.rs`). 0 warnings.
5. **Shared-file edits:** merged; both `Cargo.toml` conflicts resolved.
6. **Lane `launcher` U5:** the coordinator's reading is right. Helpers never run `gputest`; test the launch path
   against a fake child process. U5's text is corrected.
7. **Lane `upscale`, FidelityFX SDK:** download approved. Use AMD's official FidelityFX SDK release from GPUOpen's
   GitHub (MIT), pin the exact version in `agents/upscale.md`, extract to `D:\tools\fidelityfx\<version>`
   (outside git), and record its SHA-256. The DLSS setup as pointed out (Streamline from
   `D:\tools\dlss\download`, the user-supplied `nvngx_dlss.dll`) is right; neither is ever committed.
8. **Lane `input` I3:** PR #1 is merged, so `agents/loader-imports.csv` is on `master`; rebase onto it.
