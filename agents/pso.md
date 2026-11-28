# Lane `pso` — no title thread blocks on a pipeline

Branch `agent/pso`, from master 292dc2d, origin/master 6c67ee4 merged (vram3 #88, the 1 ms timer #89).

## What exe+0x23cf683 is: the title's hang watchdog

Disassembled from the prepared image copy (capstone in a scratch script; `.pdata` for function bounds):

- `exe+0x2215a60` is a watchdog thread's loop: `QueryPerformanceCounter`, `Sleep(1000)` (`0x14092eca0`,
  `ecx = 0x3e8`), then it compares a heartbeat qword at **`exe+0x71b6338`** with the value it saw last. Unchanged:
  a countdown from **30** (`ebp = 0x1e`) goes down by one; changed (or the sleep itself took far longer than 1 s, a
  debugger or a suspended process): back to 30. A second check walks a 64-entry table of per-thread timestamps
  (hash `0x89aee1d6`) against a threshold.
- At 0 it calls the fatal handler through a pointer (`exe+0x2215c33 call rax`; the handler is `exe+0x23cf620`) and
  writes to address 0 (`exe+0x2215c35`). The handler decodes a 12-byte tag (`"GPU Loc Sig"`), logs it
  (`exe+0x2215950`) and does its own null write: **`exe+0x23cf683 mov dword ptr [0], 0x4d8bd421`**. Nothing
  calls `0x23cf620` directly (no `call`/`jmp rel32` in `.text`): it is reached only through that pointer.

So the fatal is the title's response to 30 s without a frame heartbeat: frames stop at ~102 s, the title dies
~30-33 s later (v3-a: 102 s -> 135 s; v2-c: 122 s -> 139 s). It is the symptom; the cause is whatever stops the
frames.

## Built

- **Timeline** (`crates/runtime/src/psotime.rs`): a `psotl` line on stderr every second, next to the frame rate:
  PSOs the pool made, waits on a PSO (count and ms; inline creation vs waiting for a worker), binds skipped while
  pending, the pool's queue, the title's own creation calls (count, ms on its thread), the longest wait **still in
  progress** (a thread stuck on a PSO shows while it is stuck), and the process's private commit against the job's
  memory limit (a RAM cap is not a pipeline stall). A `psowait` line for every wait over 50 ms: time, thread id,
  kind, ms, the pipeline.
- **Never block** (`REBUILD_PSO_FIRST_USE`, default `skip`; `wait` is the old behaviour): `pipeline::host` returns
  no host pipeline when the PSO is not ready and calls `Pool::hurry`, which moves it (queued behind the title's
  2,048 eager requests, or parked past the cap) to a new **urgent** queue the workers take first. The lists already
  skip and count draws and dispatches with no pipeline; `set_pipeline` no longer elides a repeated set of a pipeline
  that had none, so it is drawn with as soon as it is ready.
- **Persisted**: the pipeline library file (`ID3D12PipelineLibrary1`, `pso.rs`) lives in the game's per-build
  `pipelines` cache (`rebuild_storage`), named by a hash of the program library's identity (the translator's output)
  and the adapter and user-mode driver version. A **usage record** (`pso-used-<id>.bin`, `session::Record`) keeps the
  pipelines a title thread actually bound (`Pool::mark_used` / `used`: a few thousand of the ~57,000 the title
  creates), saved every 20 s while it grows; at the next start they are queued as prewarm work
  (`REBUILD_PSO_PREWARM=0` turns it off). The report adds cache loads vs driver creations with ms each.

## Measured

(card runs below)
