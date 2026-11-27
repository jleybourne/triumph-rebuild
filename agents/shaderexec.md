# Lane `shaderexec` — the execution check against the oracle

Status: **in progress** (2026-11-27).

## Rules from the lead (2026-11-27, 21:23)

1. **GPU incident, answered.** Yes: this lane ran on the RTX between 21:13 and 21:23. 21:13 a 10-program trial of
   `execcheck --stage cs` (2 programs ran); 21:16-21:21 the whole compute set, one program at a time in a worker
   process that restarts after a failure; 21:22-21:23 single-program debug runs; a second full run started at 21:23
   and was stopped at once when the message came. The three driver resets match that run: two programs removed the
   device (`1c72d12954a65b06`, `1cd8b2a3fda225b0`, `DXGI_ERROR_DEVICE_REMOVED`) and one never completed
   (`253532a6de53babc`, a v1 program with loops). Most worker deaths before them were a harness bug, not the GPU: a
   table with many ranges got one heap base per range, and the descriptor writes ran past the heap (fixed: one base
   per table, slots checked against the heap).
2. **From now on, translated programs (ours and the oracle's) run on WARP** (`IDXGIFactory4::EnumWarpAdapter`). The
   real card only with `--card`: a small smoke set of straight-line (v0, no loops) programs, one at a time, only while
   `agents\MEASURING` is absent.
3. **Parallel DXC and translation are capped at logical processors − 4, at most 12** (16 here: 12).

## What is built

- `apps/rebuild/src/bin/execcheck/` (`main.rs`, `gpu.rs`, `run.rs`): the harness. See the module comment.
- `crates/sir`: `Lifted::desc_uses` — every descriptor read with its table, its constant byte offset in the table
  (tracked through copies and constant adds of the table's offset) and the view it is read as. Translation does
  not use it; `sirtest` and the random-program tests are unchanged.

## Next

Numbers per stage, the difference buckets and fixes: below, as they come.
