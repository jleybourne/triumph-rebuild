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
git worktree add ..\triumph-rebuild-<lane> -b agent/<lane> origin/master    # <lane> = loader, shader, launcher, vfs, upscale or input
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
| S2 | census row per program: hash, stage, GFX10 code size, instruction count, uses wave64 (yes/no), uses DPP / LDS / scratch / 16-bit, **and the wave class** (`agnostic` or `wave-wide`, by the rules in `crates/sir/src/classify.rs`: any reduction, ballot, readlane, readfirstlane of a non-uniform value, DPP/swizzle beyond a quad, or a lane id seen beyond its quad bits) | `agents/shader-census.csv` (hashes and counts only); totals match the fork's (25,694 programs; 1,433 in a world frame; 4.64M instructions) where comparable |
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
| U5 | Play: [Launch] starts `rebuild.exe` with `--config <path>`; shows the frame-line log file (`diagnostics.frame_log`) as it grows. Until the layer runs the title, launch the proofs (`gputest --bare`) to show the path works | launch and log tail work with the proof binaries |

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
updated   2026-11-27, after the settings registry (launcher track C1)
progress  28 % (progress.md) · shaders 0 / 25,694 counted (they count once decoded programs match the oracle)
tests     43 green: cargo test --release  (uma 24, xlist 5, sir 8, config 6)
```

| build | state | where | proof |
|:--|:--|:--|:--|
| 0 (a) upload cost | ✅ | `crates/uma/src/upload.rs`, `uploadbench` | [runs/build0a-upload.md](runs/build0a-upload.md) |
| 0 (b) translation cost | ✅ | `crates/xlist`, `listbench` | [runs/build0b-lists.md](runs/build0b-lists.md) |
| 0 (c) title GPU time | ⏸️ needs a frame (Build 5) | — | — |
| 1 tiles at console addresses | ✅ | `crates/uma/src/{lib,tiles,arena}.rs`, `gputest --tiled/--arena/--scale` | [runs/build1-tiles.md](runs/build1-tiles.md) |
| 2 upload ring + labels | ✅ | `crates/uma/src/{ring,labels}.rs`, `gputest --ring/--labels` | [ring](runs/build2-ring.md) · [labels](runs/build2-labels.md) |
| 3 loader + runtime | ⏳ first half in lane `loader` | — | — |
| 4 shaders | 🔄 core: design, lane model, HLSL emission proven on the card; front end in lane `shader` | `crates/sir`, `sirtest`, [design](docs/design/shader-ir.md) | [runs/build4-sirtest.md](runs/build4-sirtest.md) |
| 5 device, lists, present | ⏳ (translator core done in `crates/xlist`) | — | — |
| 6 list translation | 🔄 core built | `crates/xlist` | Build 0 (b) |
| 7 upscaler | ⏳ (plan decision 5 corrected) | — | — |
| 8 pacing · 9 AMD testers | ⏳ | — | — |
| helper lanes | live status: [`agents/secondary-agents-updates.md`](agents/secondary-agents-updates.md) | running from 12:40: `agent/loader`, `agent/shader` (S5 added). Written, to start: `agent/launcher`, `agent/vfs`, `agent/upscale`, `agent/input` | their pull requests |
| launcher track | 🔄 C1 registry built; launcher app in lane `launcher` | `crates/config`, [design](docs/design/launcher.md) | 6 unit tests |

## 9. Taking over as lead

If the owner hands you the whole job, you are the lead. Read `README.md`, `plan.md`, `tasks.md`,
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
| 1 | `sir`: live-mask analysis. Divergent branches that contain wave ops become predication; host branches elsewhere | IR tests; `sirtest` still exact |
| 2 | `sir`: memory lowering to window descriptors (`ResourceDescriptorHeap[windows + (addr >> 32)]`, 32-bit offset, uniform window via `WaveReadLaneFirst`), native 16-bit, quad ops | `sirtest` extended with loads/stores into a tiled window |
| 3 | merge lane `shader` (S1-S4), then GFX10 → `sir`: CFG, SSA, structurize; agnostic programs first | census column "emitted"; the shader bar in `progress.md` starts moving |
| 4 | oracle diff harness against the fork's translations, per hash | programs count toward 25,694 only when they match |
| 5 | merge lane `loader` (L1-L4), then Build 3's second half: kernel (threads, events, waits, timers, TLS), files with a write overlay, the GDK discovery loop from `agents/loader-imports.csv` | the title's startup path runs to its first render call |
| 6 | Build 5: D3D12.X device, lists via `crates/xlist`, descriptor heaps with a dirty list, present, **the frame line**; then Build 0 (c) (per-list timestamps, RT on and off) and direct read vs ring per resource | a non-black frame; the first measured fps |
| 7 | Build 7: find the title's upscale pass by bindings; lower the internal resolution through tiles; DLSS (Streamline) and FSR (FidelityFX SDK) | correct picture at a lower internal resolution |
| 8 | Build 8 pacing → **30 fps on NVIDIA**; Build 9 AMD testers | the frame line |

### Owner-only items (ask, don't decide)

- The minimum spec in `plan.md` (GPUs, VRAM, ReBAR, output resolution, upscaler mode), needed before Build 7.
- AMD testers for Build 9.
- Anything published outside this repository and the owner's own GitHub.
- Whether to push the fork's local `dlss-hook` branch (commit `ee0b94a3`) so the docs can link to it.
