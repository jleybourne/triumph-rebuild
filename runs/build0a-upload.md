# Build 0 (a) — dirty-chunk upload at the old layer's volume

`cargo run --release --bin uploadbench` · 2026-11-27 · i7-10700KF (16 threads), RTX 4070 Ti, PCIe 3.0 x16.
Raw output: [`build0a-upload.txt`](build0a-upload.txt). Every row below was read back whole from video
memory and compared byte for byte with the CPU region: **40 of 40 verified**.

## What was measured

A 1 GiB `VirtualAlloc(MEM_WRITE_WATCH)` region. The title rewrites 115 MiB of it every frame, in
~9,300 ranges of 256 B to 64 KiB, placed at random (the old layer made ~11,500 copies for 83-115 MB). Each
frame, all on our own code:

```
watch    GetWriteWatch + reset                      -> the 4 KiB pages written
diff     64-byte chunks against what was last sent  shadow (a full copy) or hash (u64 per chunk)
gather   changed chunks packed into an upload heap
upload   copy:    one CopyBufferRegion per run, copy queue
         scatter: one compute dispatch puts packed chunks back by index
wall     watch start -> fence complete, title writes excluded
```

## The answer

```
ceiling           115 MiB staging -> VRAM in one copy: 9.54 ms = 12.64 GB/s (80 % of PCIe 3.0 x16)

best method       hash diff + compute scatter + 4 workers
                  change               runs      MB   wall ms (p95)   of which GPU
                  100 % of chunks     9,318   108.5   33.8  (39.1)    13.4
                   50 % of chunks   439,990    55.8   30.1  (32.4)     6.9
                   10 % of chunks   166,814    11.4   16.4  (19.5)     1.2
                   10 % of ranges     1,038    11.5   14.8  (16.3)     1.2
                    0 % (touched)         0     0.0   17-31         —     (re-run; see caveats)

rejected          copy-per-run: recording alone is ~0.33 µs a copy — 146 ms for 440k runs,
                  55 ms for 167k. Collapses on any fragmented frame.
                  shadow diff: slower than hash in every row with changes (faster only at 0 %), and costs 100 % of the region in RAM (hash: 12.5 %).
```

**What the title really sends** (measured in the fork, `jleybourne/Triumph` `docs/design/tracking-cost.md` and `recording-cost.md`):
of 115 MB copied, **~48 MB actually changed**, and the exact CPU-owned ranges are **226,099 ranges, 31.3 MB a
frame**. That shape sits between our 10 % and 50 % chunk rows, so the expected cost is **~16-30 ms of
worker wall time, ~1-7 ms of it on the GPU**. The fork independently measured one `CopyBufferRegion` per
range at 36 ms in the driver, which agrees with rejecting copy-per-run.

## Verdict for decision 1

**Decision 1 holds, with three conditions it did not state:**

1. **Scatter, not copies.** The upload is one compute dispatch over packed chunks.
2. **Off the critical path.** At worst-case volume the work is ~34 ms of wall time — a whole frame — so it
   must run on worker threads, a frame ahead, overlapping the title and the render. Run serially before
   submit, it alone would cap the frame at ~30 fps.
3. **Detection has a floor that does not depend on how much changed.** Watching and diffing ~36,800
   written pages costs ~13 ms on 4 workers even when nothing changed. That cost scales with pages
   *written*, so the region watched must be only the title's CPU-written memory, never GPU-owned memory.

The PCIe link is not the constraint: the GPU part is 1-13 ms of a 33 ms frame on the copy/compute queue.

## Caveats

- The destination is a plain default-heap buffer; Build 1 re-runs this against tiled windows.
- Frames are synchronous (watch → fence, then the next frame); overlap with rendering is not measured here.
- Worker threads are spawned per frame (~tens of µs); Build 2 uses a persistent pool.
- The first full run's `0 %` hash rows (200+ ms) were disturbed by other load on the machine; a re-run
  (`--change chunks:0`) gave 17.2 ms (hash, copy, 4 workers) and 31.1 ms (hash, scatter, 4 workers). That
  case still varies the most of any row.
- GPU times for the same bytes vary up to 2× between rows (9 vs 17 ms for 108 MB); clocks were not pinned.
