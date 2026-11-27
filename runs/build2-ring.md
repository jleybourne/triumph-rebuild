# Build 2 — CPU-written memory on an async ring

`cargo run --release --bin gputest -- --ring` · 2026-11-27 · i7-10700KF, RTX 4070 Ti. Code:
`crates/uma/src/ring.rs` (`Ring`, `Watched`, the coordinator and the persistent diff pool). Raw output:
[`build2-ring.txt`](build2-ring.txt).

## The design, as built

```
submit thread   ring.flush() -> V, at once; title queue.Wait(fence, V)     never watches, diffs or copies
coordinator     GetWriteWatch+reset -> hash diff on 4 persistent workers -> gather into one of 3 staging
                slots (96 MiB) -> one scatter dispatch per window on an async compute queue -> Signal(V)
guard           the submit thread is marked; any copy of title memory on it panics (every build, tested)
```

## The run

Two write-watched regions in two windows (256 MiB in window 0, 64 MiB in window 1), mapped onto
Build 1's tiles. Each frame the title rewrites 115 MiB with 30 % of chunks really changing, which is the
title's real volume (~31-48 MB). One frame is in flight.

```
submit    flush() + GPU wait queued: median 32.5 µs, worst 72.5 µs — the submit thread never copies
upload    31,240 pages · 341,775 runs · 34.2 MB a flush
          watch 3.22 · diff 9.25 · gather 6.01 · record 0.73 ms | flush -> signalled 19.4 ms median, 23.6 worst
title     31.1 ms of its own writes a frame; pacing wait 0.01 ms median
PROVEN    after 60 frames both regions read back through the tiles byte for byte
```

## Verdict

- **The submit thread is blocked for ~30 µs a frame**, against a 33.3 ms budget. The task closes.
- **The upload lands 19 ms after flush**, on workers, at the title's real shape (342k runs, 34 MB). It
  fits inside a frame with room, and runs in parallel with the title's next frame of CPU work.
- **Visibility is the console's:** the GPU sees CPU bytes as of when the coordinator read them. A chunk
  caught half-written is sent again on the next flush, and the final read-back proves nothing is lost.

## Learned

- **The first touch of a never-used page sets its write-watch bit, even a read.** The diff then finds
  nothing changed, so it costs a page diff, never a wrong byte (a unit test records it).

## Not yet

- GPU→CPU labels: the title's bottom-of-pipe writes and the fences it polls (the next Build 2 task).
