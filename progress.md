# Progress

```
work toward 30 fps   [██░░░░░░░░░░░░░░░░░░]   8 %
measured fps         none yet — the title has not rendered a frame on this code
baseline to beat     7.6 fps · 131.3 ms · ~4,500 draws · 356 dropped   (the layer being replaced)
target               30 fps · 33.3 ms · same draws · stated output resolution and upscaler mode
updated              2026-11-27
```

The percentage is **work done, weighted by build step**. It is not fps. It cannot say how close the frame
is to 33.3 ms until Build 5 produces a frame to measure. From then on, the frame line goes in the
second row and is the number that counts.

## By build step

| # | build | weight | done | earned | state |
|---|---|---:|---:|---:|---|
| 0 | three answers before more code | 10 | 40 % | 4.0 | (a) upload **done**: decision 1 holds with conditions; (b) and (c) todo |
| 1 | tiled VRAM at console addresses | 10 | 20 % | 2.0 | page bookkeeping (8 tests) and the GPU↔CPU proof done; tiles on the GPU not started |
| 2 | CPU-written memory on an async ring; GPU→CPU region | 8 | 20 % | 1.6 | 64 B diff and gather written and tested (7 tests); method chosen by measurement; ring, fence and pool todo |
| 3 | PE loader + console runtime | 15 | 0 % | 0 | — |
| 4 | shader recompiler RDNA2 → DXIL, wave32 | 15 | 0 % | 0 | — |
| 5 | D3D12.X device, lists, descriptors, present | 12 | 0 % | 0 | first frame |
| 6 | parallel list translation | 8 | 0 % | 0 | — |
| 7 | upscaler: DLSS + FSR | 10 | 0 % | 0 | plan correction needed: method 0x218 is not the upscale call |
| 8 | frame pacing | 6 | 0 % | 0 | 30 fps on NVIDIA closes here |
| 9 | AMD validation by testers | 6 | 0 % | 0 | 30 fps on AMD closes here |
| | **total** | **100** | | **7.6 ≈ 8 %** | |

The weights reflect size and risk: the loader, the runtime and the shader recompiler are the largest pieces
of work. A step counts as 100 % only when its closing number from `tasks.md` is measured.

## Done

- Address space: 4 GiB windows, 64 KiB pages, placement, sharing by address, lifetime by user count (8 tests).
- GPU writes into CPU-mapped memory, and the CPU reads the same bytes back (`gputest`), plus the control (`gputest --bare`).
- **Build 0 (a) — upload benchmark** (`uploadbench`, [`runs/build0a-upload.md`](runs/build0a-upload.md)):
  12.64 GB/s ceiling over PCIe 3.0. At the title's real shape (~226k ranges, ~31-48 MB) the upload costs
  ~16-30 ms of worker wall time, ~1-7 ms of it on the GPU. It must use hash diff + compute scatter and
  run a frame ahead, off the submit thread. Every config verified byte-exact.
- 64-byte chunk diff, gather and chunk index (`crates/uma/src/chunks.rs`, 7 tests).

## Not done

- Build 0 (b): per-draw translation cost. (c): the title's real GPU time, with RT on and off.
- Minimum spec in `plan.md` (owner decision, needed before Build 7).
- Everything from Build 1 tiles onward: see [`tasks.md`](tasks.md).

## Open problems that could move the target

1. **Plan decision 5 is built on a wrong premise.** The fork found that method 0x218 creates pipelines; the
   title's FSR runs as ordinary compute dispatches that must be found by their bindings.
2. **No known way to lower the internal resolution.** The fork's attempt to resize the title's targets made
   it assert at load. If the upscaler cannot lower 2560x1440, it buys little GPU time.
3. **The title's GPU time is probably above 45 ms** (the fork measured 43-63 ms, RT off). Above ~33 ms of
   pure title work, 30 fps needs the upscaler to actually cut pixels — see 2.
4. **Detection floor.** Watching and diffing costs ~13 ms of 4 worker threads every frame, whatever changed.

## Updates — one entry per upload, newest first

Each upload adds an entry: its own bar, what changed, what was measured, and what moved the percentage.
Older entries are never edited.

### 2026-11-27 — README: why we chose a rebuild

```
[██░░░░░░░░░░░░░░░░░░]   8 %   (unchanged: documentation only)
measured fps   none yet
```

- **Added** "Why a rebuild" to the README: where the old 131 ms goes, the 19 failed tuning experiments, why
  the memory model can't be retrofitted, and why not adopt xcl. Also a table of each main choice with its
  reason and its evidence, including the Build 0 (a) numbers, and the open questions stated plainly.
- **Fixed** two stale README lines: the test count (15, not 8), and list caching, which now reads "kept only if
  the hit rate pays", as the plan says.
- **Next:** unchanged — Build 0 (b).

### 2026-11-27 — Build 0 (a): the upload benchmark

```
[██░░░░░░░░░░░░░░░░░░]   8 %   (was 2 %: +4.0 Build 0 (a), +1.6 Build 2 chunk diff; Build 1 unchanged at 2.0)
measured fps   none yet
```

- **Built** `uploadbench` (`crates/uma/src/upload.rs`): write-watch on a 1 GiB region, 64 B diff (shadow and
  hash), gather into an upload heap, then copy-per-run or compute scatter. GPU timestamps, and a full
  readback check per config.
- **Built** `crates/uma/src/chunks.rs`: diff, run joining, gather and chunk indices, with 7 unit tests (15 in
  total, all green).
- **Measured** 40 configs, all verified byte-exact: 12.64 GB/s ceiling. Hash + scatter + 4 workers gives
  33.8 ms at 108 MB all changed, and 16-30 ms at the title's real shape. Copy-per-run is rejected (146 ms to
  record 440k copies). [`runs/build0a-upload.md`](runs/build0a-upload.md)
- **Decided:** decision 1 holds, with conditions: scatter, on workers, a frame ahead. Detection costs a
  floor of ~13-17 ms of worker time whatever changed.
- **Learned from the fork** (jleybourne/Triumph), only where it changes the rebuild: the title's real upload
  shape (226k ranges / 31 MB); 0x218 is not the upscale call; the internal resolution has not been lowered
  yet; title GPU time is 43-63 ms; six traps added to `tasks.md`.
- **Docs:** `tasks.md`, `HANDOFF.md` and `REVIEW-PROMPT.md` aligned to the six-decision plan. README now
  carries the bar.
- **Next:** Build 0 (b), the per-draw translation microbenchmark. Owner: correct decision 5 in `plan.md`,
  and fill in the minimum spec.

### before 2026-11-27 — address space and the GPU↔CPU proof

```
[█░░░░░░░░░░░░░░░░░░░]   2 %   (estimated after the fact, same weights: Build 1 bookkeeping, 2.0)
```

- `crates/uma` page bookkeeping, 8 tests; `gputest` and `gputest --bare` proven on the RTX 4070 Ti.
