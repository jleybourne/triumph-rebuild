# secondary-agent.md — the temporary plan for helper agents

**Temporary.** This file exists so that helper agents can work beside the lead session without breaking
it. It is deleted when both lanes below are merged. Read it fully before touching anything.

```
lead session   owns Build 2 (crates/uma ring), all measurements, and every shared document
helper agents  own one lane each, in their own worktree, branch and crate; hand back by branch
```

## 1. Before you start (every agent)

Read, in this order: `README.md`, `plan.md` (the six decisions), `tasks.md` (your lane's build),
`progress.md`, then this file again. Then:

```powershell
cd D:\Developer\triumph-rebuild
git fetch origin
git worktree add ..\triumph-rebuild-<lane> -b agent/<lane> origin/master    # <lane> = loader or shader
cd ..\triumph-rebuild-<lane>
cargo test --release                                                         # must be green before you change anything
```

Work **only** in that worktree. Never `git checkout` or `git switch` in `D:\Developer\triumph-rebuild`.
That is the lead's working tree, and switching it breaks a run in progress.

## 2. The rules that keep the lead's work safe

| rule | why |
|:--|:--|
| **Do not edit** `crates/uma/**`, `crates/xlist/**`, `crates/sir/**`, `docs/**`, `apps/rebuild/src/bin/{gputest,uploadbench,listbench}.rs`, `plan.md`, `tasks.md`, `progress.md`, `README.md`, `HANDOFF.md`, `REVIEW-PROMPT.md`, `runs/**`, `secondary-agent.md` | the lead is changing these right now; any edit becomes a merge conflict |
| You **may** add one line to the root `Cargo.toml` `members` list for your crate | the lead resolves that one line on merge |
| **Measurement lock:** while `D:\Developer\triumph-rebuild\agents\MEASURING` exists, run **no** builds, tests or other heavy work. Check before every build: `if (Test-Path D:\Developer\triumph-rebuild\agents\MEASURING) { "wait" }` | the lead's GPU and CPU timings need a quiet machine. One background build once turned 7.3 fps into 2.3. |
| Build with `cargo build --release -j 4` | leaves cores free for the lead |
| **No GPU work** of any kind (no D3D12 device creation, no `gputest`) | GPU timings are the lead's; a second device skews them |
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
- **Shader IR, lowering and DXIL emission are ours** (plan decision 4). Do not vendor them, and do not
  start them: the lead designs the IR first.
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
| S2 | census row per program: hash, stage, GFX10 code size, instruction count, uses wave64 (yes/no), uses DPP / LDS / scratch / 16-bit | `agents/shader-census.csv` (hashes and counts only); totals match the fork's (25,694 programs; 1,433 in a world frame; 4.64M instructions) where comparable |
| S3 | GFX10 (RDNA2) decoder: every encoding the corpus uses (SOP1/2/C/K/P, SMEM, VOP1/2/3/3P/C, DPP, SDWA, DS, MUBUF, MTBUF, MIMG, FLAT/GLOBAL/SCRATCH, EXP). Vendoring the fork's `gfx10` opcode tables is allowed | **byte-exact re-encode** of every instruction in all 25,694 programs; 0 unknown opcodes, or a named list |
| S4 | per-program feature summary for the IR design: register counts, branch structure (structured or not), wave-uniform vs per-lane branch share | a section in `agents/shader.md`; the lead uses it to design the IR |

Stop after S4. The IR, the lowering and DXIL emission are the lead's next design (plan decision 4).

## 6. Handing back

1. Keep `agents/<lane>.md` in your branch: what you built, the numbers, what's left. Use the same voice
   as `progress.md`: built / measured / decided / next.
2. Commit small, with messages that say what was measured. End each with the attribution line your session
   gives you.
3. Push `agent/<lane>` and open a pull request to `master` titled `agent/<lane>: <what closed>`. **Do not merge it.**
4. The lead reviews, merges, writes the `progress.md` entry (one per upload, with its own bar), and
   updates `tasks.md`.

## 7. If something looks wrong in the lead's code or documents

Don't fix it. Write it in `agents/<lane>.md` under "for the lead", with the file, the line and the
evidence. The lead picks it up at merge.

## 8. Status of the lead (as this file was written, 2026-11-27)

```
done      Build 0 (a), 0 (b), Build 1 (tiles), Build 2 (ring + labels), translator core (crates/xlist)
doing     the shader IR design (plan decision 4); lane shader's S4 feeds it
next      Build 3 second half (kernel, runtime) once lane loader's L1-L3 merge
progress  27 % (see progress.md); the shader IR lives in crates/sir (lead only)
```
