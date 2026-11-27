# Progress

```
work toward 30 fps   [█████░░░░░░░░░░░░░░░]  24 %
shaders (census)     [░░░░░░░░░░░░░░░░░░░░]   0 / 25,694   (fork reference: 25,694 translated, 7 pipelines refused)
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
| 0 | three answers before more code | 10 | 70 % | 7.0 | (a) upload **done**; (b) translation **done**: 0.43 µs a draw; (c) blocked until the title renders (moved to Build 5) |
| 1 | tiled VRAM at console addresses | 10 | 100 % | 10.0 | **closed**: tiles, console addresses, sharing (buffer and render target), arena, 10 GiB scale, upload onto tiles |
| 2 | CPU-written memory on an async ring; GPU→CPU region | 8 | 80 % | 6.4 | ring built and proven: submit thread 32 µs, upload 19 ms at the title's shape, verified; GPU→CPU labels todo |
| 3 | PE loader + console runtime | 15 | 0 % | 0 | — |
| 4 | shader recompiler RDNA2 → DXIL, wave32 | 15 | 0 % | 0 | decided: our own core (lane state as data, direct addressing, uniformity, native ops); decode front end may be vendored with credit |
| 5 | D3D12.X device, lists, descriptors, present | 12 | 0 % | 0 | first frame |
| 6 | list translation | 8 | 10 % | 0.8 | translator core written and measured (5 tests); list cache dropped by measurement |
| 7 | upscaler: DLSS + FSR | 10 | 0 % | 0 | plan corrected: the upscale pass is found by bindings; lowering the internal resolution is its own task |
| 8 | frame pacing | 6 | 0 % | 0 | 30 fps on NVIDIA closes here |
| 9 | AMD validation by testers | 6 | 0 % | 0 | 30 fps on AMD closes here |
| | **total** | **100** | | **24.2 ≈ 24 %** | |

The weights reflect size and risk: the loader, the runtime and the shader recompiler are the largest pieces
of work. A step counts as 100 % only when its closing number from `tasks.md` is measured.

## Done

- Address space: 4 GiB windows, 64 KiB pages, placement, sharing by address, lifetime by user count, card-page reuse.
- GPU writes into CPU-mapped memory, and the CPU reads the same bytes back (`gputest`), plus the control (`gputest --bare`).
- **Build 0 (a) — upload benchmark** (`uploadbench`, [`runs/build0a-upload.md`](runs/build0a-upload.md)):
  12.64 GB/s ceiling over PCIe 3.0. At the title's real shape (~226k ranges, ~31-48 MB) the upload costs
  ~16-30 ms of worker wall time, ~1-7 ms of it on the GPU. It must use hash diff + compute scatter and
  run a frame ahead, off the submit thread. Every config verified byte-exact.
- 64-byte chunk diff, gather and chunk index (`crates/uma/src/chunks.rs`, 7 tests).
- **Build 0 (b) — translation benchmark** (`listbench`, [`runs/build0b-lists.md`](runs/build0b-lists.md)):
  our translator plus D3D12 costs 0.43 µs a draw, 1.9 ms for the title-shaped frame on one thread, which is 18×
  under budget. 12,027 descriptor writes from a dirty list cost 2.0 ms. List translation is not the CPU problem.

- **Build 1 — tiled VRAM** ([`runs/build1-tiles.md`](runs/build1-tiles.md)): 4 GiB reserved windows with 64 KiB tiles
  on 64 MiB heaps. Shaders read and write at console addresses. Two buffers, or a buffer and a render target,
  at one address see each other's writes. Only stencil formats need the arena. 10 GiB committed at random
  addresses with 0 wrong. The upload has no measurable tile penalty.

- **Build 2 — the upload ring** ([`runs/build2-ring.md`](runs/build2-ring.md)): the submit thread calls `flush()` and
  queues a GPU wait in 32 µs. The coordinator and 4 workers send 34 MB in 342k runs in 19.4 ms. Both regions
  verified byte for byte after 60 frames.

## Not done

- Build 0 (c): the title's real GPU time, with RT on and off. Blocked: it needs the title rendering, so it moved to Build 5.
- Minimum spec in `plan.md` (owner decision, needed before Build 7).
- Build 2's GPU→CPU labels, then everything from Build 3 onward: see [`tasks.md`](tasks.md).

## Open problems that could move the target

1. ~~Plan decision 5 is built on a wrong premise.~~ **Corrected 2026-11-27:** the title's upscale pass is
   recognised by its bindings and replaced; lowering the internal resolution is its own task.
2. **No known way to lower the internal resolution.** The fork's attempt to resize the title's targets made
   it assert at load. If the upscaler cannot lower 2560x1440, it buys little GPU time.
3. **The title's GPU time is probably above 45 ms** (the fork measured 43-63 ms, RT off). Above ~33 ms of
   pure title work, 30 fps needs the upscaler to actually cut pixels — see 2.
4. **Detection floor.** Watching and diffing costs ~13 ms of 4 worker threads every frame, whatever changed.

## Updates — one entry per upload, newest first

Each upload adds an entry: its own bar, what changed, what was measured, and what moved the percentage.
Older entries are never edited.

### 2026-11-27 — Build 2: the upload ring; helper-agent plan; README links

```
[█████░░░░░░░░░░░░░░░]  24 %   (was 19 %: +4.8 Build 2 ring)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/uma/src/ring.rs`: write-watched regions, a coordinator thread, a persistent diff pool,
  3 staging slots, and one scatter dispatch per window on an async compute queue behind a fence.
  `flush()` returns at once.
- **Proven** with `gputest --ring`, 60 frames at the title's real shape (34 MB, 342k runs a flush): the submit
  thread is blocked 32 µs median, 72 µs worst. The upload lands in 19.4 ms. Both regions match byte for
  byte. [`runs/build2-ring.md`](runs/build2-ring.md)
- **Guard:** the submit thread is marked, and copying title memory on it panics in every build (tested).
- **Learned:** the first touch of a never-used page sets its write-watch bit, even a read. Harmless.
- **Fixed the README links:** every badge now links somewhere; the fork's `ROADBLOCK.md` is linked; the
  unpushed fork commit is labelled as local; `runs/README.md` index added; LICENSE (GPL-3.0) added. All
  links checked 200 on GitHub. (That fix was pushed without its own entry; it is recorded here.)
- **Added** `secondary-agent.md`: a temporary plan for helper agents. Lanes `loader` (Build 3 first half)
  and `shader` (the Build 4 census and decoder), each in its own worktree, branch and crate. A measurement
  lock (`agents/MEASURING`) keeps their builds off the lead's timed runs.
- **Next:** Build 2's GPU→CPU labels.

### 2026-11-27 — Decisions: our own shader core; plan decisions 3 and 5 corrected

```
[████░░░░░░░░░░░░░░░░]  19 %   (unchanged: decisions only)
shaders     0 / 25,694
measured fps   none yet
```

- **Decided: the shader core is ours**, chosen for performance. The fork's own analysis puts ~10-22 ms of
  the title's GPU frame in its translator's core choices: lane state, per-access address lookups, scalar work
  done per lane, emulated 16-bit and wave ops. Ours is designed around fixing those from the first line.
  Only the decode front end (no performance in it) may be vendored, with credit.
- **Corrected decision 3:** translate on the title's own threads, no list cache, ≤ 1 µs a draw (from
  Build 0 (b)).
- **Corrected decision 5:** the title's FSR is found by its bindings, not called through 0x218. DLSS/FSR
  replace that pass. Lowering the internal resolution is its own Build 7 task; tiles at console addresses
  may let a smaller target sit behind the title's address.
- **Next:** Build 2.

### 2026-11-27 — Build 1: tiled VRAM at console addresses

```
[████░░░░░░░░░░░░░░░░]  19 %   (was 11 %: +8.0 Build 1 closed)
shaders     0 / 25,694 (bar added; the census tool is the first Build 4 task)
measured fps   none yet
```

- **Built** `crates/uma/src/tiles.rs`: reserved 4 GiB windows, 64 MiB tile heaps, an `UpdateTileMappings`
  batcher, and aliasing for other resources at one address. Also `arena.rs` for untileable resources, and
  card-page reuse in `Uma` (it used to grow forever). 21 tests in `crates/uma`.
- **Proven on the card:** 7 `gputest --tiled` proofs; the `--arena` split (only D32S8 and D24S8 need it); a
  `--scale 10` run of 10 GiB in 5,120 random placements, 0 wrong, no device removal.
- **Measured:** `uploadbench --tiled` shows no penalty from tiles. Both destinations swing between ~7-10 and
  ~19 ms of GPU time with the card's clock state.
- **Shaders:** Build 4 rewritten as a census against the fork's 25,694 programs, used as an oracle. The goal
  is 0 refusals and wave32 everywhere. A separate shader bar is on this page.
- **Owner decision raised:** the fork's shader crate is GPL-3.0-or-later and could legally be reused here
  with credit. Fresh code or reuse?
- **Next:** Build 2, CPU-written memory on an async ring.

### 2026-11-27 — Build 0 (b): per-draw translation

```
[██░░░░░░░░░░░░░░░░░░]  11 %   (was 8 %: +3.0 Build 0 (b), +0.8 Build 6 translator core)
measured fps   none yet
```

- **Built** `crates/xlist`: list calls as records, and a lock-free translator (state elision, console→PC
  addresses, pipeline table). It writes to a counting sink or a real D3D12 list. 5 tests; 20 in the
  workspace, all green.
- **Built** `listbench`: a synthetic frame in the title's shape (4,500 draws, 20,311 calls, 48 lists, 1,433
  pipelines, 200k descriptor slots). It runs on a persistent pool of 1-6 workers, and every frame is
  executed on the GPU.
- **Measured:** our cost is 0.03 µs a draw; with D3D12 and the driver it is 0.43 µs a draw, 1.9 ms a frame on
  one thread (0.52 ms on 6). 12,027 descriptor writes from a dirty list cost 2.0 ms.
  [`runs/build0b-lists.md`](runs/build0b-lists.md)
- **Decided:** decision 3 is simplified. One thread fits, so translate on the title's own recording threads,
  with no extra pool. The **list cache is dropped**: it could not pay at 2 ms a frame. The old 29 µs a draw
  was memory bookkeeping, which decision 1 removes.
- **Moved:** Build 0 (c) needs the title rendering, so it closes in Build 5. Until then the fork's 43-63 ms stands.
- **Next:** Build 1, tiled VRAM at console addresses.

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
