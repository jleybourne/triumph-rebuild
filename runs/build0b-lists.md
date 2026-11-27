# Build 0 (b) — per-draw translation cost

`cargo run --release --bin listbench` · 2026-11-27 · i7-10700KF, RTX 4070 Ti. Raw output:
[`build0b-lists.txt`](build0b-lists.txt). Every recorded frame was submitted and ran to completion: no
device removal, 0 refused calls.

## What was measured

A synthetic recorded frame in the title's shape: **4,500 draws, 20,311 list calls in 48 lists**. Half the
draws are indirect (the title's ~4,500 indirect commands), with 1,433 pipelines (the title's world-frame
program count) and 200,000 console descriptor slots. A third of table and constant-buffer sets repeat what
is already bound. The translator is ours (`crates/xlist`):

```
state      repeats dropped (3,317 of 20,311 calls a frame)
address    console address -> PC address: one subtract, one compare, one add
object     console pipeline handle -> PC pipeline: one probe in a prebuilt open-addressing table
locks      none; each list is translated by one worker, everything it reads is built at start
```

Timed two ways: into a counting sink (our cost alone), and into real D3D12 command lists (ours plus the
runtime and the driver). Workers are a persistent pool, so no thread is created inside a timed frame.

## The answer

```
                            threads   wall ms   thread ms   µs / draw   µs / call
ours alone                     1        0.16       0.14       0.032       0.007
ours + D3D12 + driver          1        1.97       1.92       0.426       0.094
                               2        0.99       1.89       0.420       0.093
                               4        0.70       2.63       0.584       0.129
                               6        0.52       2.80       0.622       0.138
budget (serial)                                               7.4         1.6

descriptors   12,027 written a frame from a dirty list: 1.99 ms (0.166 µs each), one thread
```

## Verdict for decision 3

- **The translation itself is not the CPU problem.** The D3D12 floor for the title's call volume is
  ~0.4 µs a draw, ~2 ms a frame on one thread, **18× under the 7.4 µs budget**. The old layer's 29 µs a
  draw was not translation. It was the work its memory model hung on every bind: shadows, refreshes and
  deferred copies (the fork measured `State::bind` at 13-18 µs, bookkeeping, not API calls). Decision 1
  removes that work.
- **Parallel translation is not needed for the budget.** It works (0.52 ms wall on 6 threads, with mild
  driver contention: thread time 1.9 → 2.8 ms), but one thread already fits. Record each list where the
  title records it (its 5-6 job threads); no extra pool, no cache.
- **List caching is dropped from the plan.** At 2 ms a frame, a cache's hashing could not pay for itself.
- **The rule this leaves for Build 5:** per-draw work stays near 0.5 µs. Anything that adds more than 1 µs
  a draw (a lookup that allocates, a lock, a memory-tracking call) must be measured and justified.

## Not modelled (and where each is closed)

- The title's real call mix, render-target binds, clears, copies, and transition barriers between states.
  Build 5 measures the real stream on the frame line.
- Indirect argument formats that need converting (if the console's differ). Build 5.
- Descriptors are buffer and BC1 texture views; the title's mix is unknown until Build 5.
- Pipelines are distinct objects but trivially small (18 ms to build 1,433 here). Real build time is a
  Build 4 number, and it happens at start, never in a frame.
