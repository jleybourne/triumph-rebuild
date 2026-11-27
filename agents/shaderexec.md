# Lane `shaderexec` — the execution check against the oracle

Status: **built and run** (2026-11-28). Results: [`runs/build4-exec.md`](../runs/build4-exec.md), per program
[`runs/build4-exec.csv`](../runs/build4-exec.csv).

## Rules from the lead (2026-11-27, 21:23 and later)

1. **GPU incident, answered.** Yes: this lane ran on the RTX between 21:13 and 21:23. 21:13 a 10-program trial of
   `execcheck --stage cs` (2 programs ran); 21:16-21:21 the whole compute set, one program at a time in a worker
   process that restarts after a failure; 21:22-21:23 single-program debug runs; a second full run started at 21:23
   and was stopped at once when the message came. The three driver resets match that run: two programs removed the
   device (`1c72d12954a65b06`, `1cd8b2a3fda225b0`) and one never completed (`253532a6de53babc`, a v1 program with
   loops whose trip count comes from the data). Most worker deaths before them were a harness bug, not the GPU: a
   table with many ranges got one heap base per range, and the descriptor writes ran past the heap (fixed).
2. **Translated programs run on WARP** (`rebuild_pipelines::d3d::Gpu::warp()`). The card only with `--card`: at most
   20 straight-line programs, one at a time, under the card lock. Every run in the results is on WARP.
3. **DXC and translation are capped**: translation threads at `min(logical processors − 4, MAX_DXC)`, DXC through
   `rebuild_pipelines::translate::compile_file` and its semaphore (`MAX_DXC = 4`).
4. **The card lock**: `--card` waits for `rebuild_pipelines::d3d::CardLock` (create-new `agents\CARD`, absent
   `agents\MEASURING`) and drops it when done; when a worker dies holding it, the supervisor removes the lock only if
   it names that worker's pid. Every worker also waits before each program while `agents\MEASURING` exists.
5. Lanes `shadercov` and `wave64` also work in `crates/sir`: this lane keeps to execution-mismatch fixes, small
   commits, and merges `origin/master` (twice so far, after #31 and #35).

## Built

- `apps/rebuild/src/bin/execcheck/` — the harness. `main.rs`: selection (every agnostic VS/PS/CS the translator
  takes, through `rebuild_pipelines::translate::lift_hlsl`), the oracle entry (`VSH1`: DXIL, static samplers, root
  views, its `[WaveSize]`), a supervisor that runs the GPU in a worker process and names the program and input set
  a worker died on. `run.rs`: the inputs (user data as root constants; each descriptor table at its own place in the
  heap; in each slot a view of the kind the program reads; three seeded input sets: small integers, floats, a mix;
  the shadow table's stride from the title's root signature, else the furthest record read), both runs, the
  word-by-word comparison and the reason taxonomy. `ps.rs`: a feeding vertex shader built from our pixel shader's
  input struct, a 64 × 64 quad with varying w, full 32-bit targets and depth. `vs.rs`: seeded attributes for both
  sides' inputs, two instances, every output component through stream output (with its counter). `gpu.rs`: WARP or
  the card, the heaps, resources with starting contents and readback, a 30 s completion limit.
- `crates/sir`: `Lifted::desc_uses` (every descriptor read: its table, its byte offset when constant, through copies
  and constant adds of the table offset, and the view), carried to `rebuild_pipelines::translate::Hlsl::uses`.
  Metadata only.

## Translator fixes (each from a difference bucket)

| fix | found on | effect |
|:--|:--|:--|
| a load through a UAV descriptor (the title's root signature names the range a UAV) is declared as one | `0f4e0a1da329909a` read 0 where the oracle read the texture | compute 173 → 222 matching |
| a buffer read through a table is a constant buffer only when its own range is one | `3fe821a8916a4564` (an SRV range beside a CBV range) | |
| d16 typed buffer loads and stores carry halves | `e4d850a7baf18270` | |
| med3 of a value and two constants is a clamp, max then min (NaN goes to the low bound as on the console); general med3 checks NaN; `0 - x` stays a subtraction (`precise`: DXC folded it to `-x`, +0 became -0) | `9034c0d9d567c3e5`, `565cc0b38920b229` | 13 of 38 differing compute programs match |
| buffer loads through a CBV descriptor read the host's CBV (an SRV type over a CBV descriptor lost the WARP device; GPU-based validation named it) | `001a8d494b5f60b3` | the 16 vertex programs that lost the device now run |

## Next

- The fast-math question for the lead: DXC's default float freedom (contraction, reassociation) leaves a few
  programs within 1-4096 ulp; `--gis` (IEEE strictness) shows which. Decision is a performance one (`DXC_ARGS`).
- The biggest untested bucket is opcode coverage (lane `shadercov`), then compute oracles that need wave size 64
  (no adapter here runs them).
