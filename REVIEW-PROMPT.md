# REVIEW-PROMPT — the exact brief to give an independent model

Copy the block below into a fresh session from this directory. It is self-contained: the reviewer does not
need this conversation.

## Run it

```powershell
cd D:\Developer\triumph-rebuild
claude auth status --text        # must say logged in
claude                           # interactive, then paste the prompt below
```

or one-shot, non-interactive:

```powershell
claude -p "Read HANDOFF.md and answer its questions. Check the claims against this repo, D:\Developer\xcl and the fork at D:\Developer\fxceless-victory (git remote triumph, branch triumph/main). Be blunt — the owner wants errors found, not agreement. Do not modify files, and do not build or run xcl or the old layer." --model opus --max-turns 30
```

## The prompt

> Read `HANDOFF.md` in this repository, then `plan.md`, `tasks.md`, `progress.md`,
> `runs/build0a-upload.md` (with its raw output `runs/build0a-upload.txt`), and the code in
> `crates/uma/src/` (`lib.rs`, `chunks.rs`, `upload.rs`, `windows.rs`) and `apps/rebuild/src/bin/`.
> You may run `cargo test --release -p rebuild-uma` and `cargo run --release --bin uploadbench -- --quick`.
>
> Reference material, read as design only: `D:\Developer\xcl` (`crates/xcl/src/hle/d3d12x/sparse.rs`,
> `vram.rs`, `console_heap.rs`) and the owner's fork of the replaced layer, `D:\Developer\fxceless-victory`,
> through git without checking out (`git show triumph/main:<path>`): `ROADBLOCK.md`, `CLAUDE.md`,
> `docs/runs.md`, `docs/gpu-time.md`, `docs/design/recording-cost.md`, `docs/design/tracking-cost.md`,
> `docs/design/card-budget.md`, `docs/research/resolution.md`, and commit `ee0b94a3`.
>
> Answer the five questions at the end of `HANDOFF.md`. Verify claims against the code and the numbers,
> not the prose. Check whether the Build 0 (a) benchmark is a fair model of the title before accepting its
> verdict. Attack the five weak points the author lists, starting with the upscaler route.
>
> Be blunt. The owner wants errors found, not agreement. Do not modify any file, and do not build or run
> xcl or the old layer.

## What the author expects it to say

Recorded up front, so that a review which only repeats this is judged as adding nothing:

1. **Decision 5 needs rewriting.** 0x218 is pipeline creation; the title's FSR must be found by bindings.
   And with no way yet to lower the internal resolution, the upscaler's gain is unproven.
2. **The GPU half is probably worse than 45 ms** (43-63 ms in the fork, RT off), so 30 fps depends on 1.
3. **The upload verdict is conditional.** It holds only with scatter and a frame of overlap, and the
   benchmark did not measure that overlap, used a default heap rather than tiles, and spawns threads per
   frame. The ~13-17 ms detection floor competes with the title for 8 cores.
4. **The 7.4 µs serial budget is the wrong frame.** Translation runs on the title's recording threads,
   which mostly wait on the title's own scheduler.
5. **Rewrite versus hybrid is still not costed**, now that the fork translates 100 % of the shader census.

## The questions only our own code can settle

We do not run the reference layers. Build 0 answers on our code instead:

```
(a) upload at real volume       done — runs/build0a-upload.md
(b) per-draw translation cost   todo — the CPU half
(c) the title's GPU time        todo — per-list timestamps, RT on and off
```
