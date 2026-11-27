# Build 4 — the shader core on the card

`cargo run --release --bin sirtest` · 2026-11-27 · RTX 4070 Ti, DXC 1.8.2502.11 (Windows SDK 10.0.26100).
Code: `crates/sir` (`hlsl.rs` the emitter, `gen.rs` random programs, `lanes.rs` the interpreters). Raw output:
[`build4-sirtest.txt`](build4-sirtest.txt).

## What was run

200 random IR programs. Half are wave-wide (reductions, ballots, readlanes, first-lane reads, swizzles across
all 64 lanes, lane ids, under random EXEC masks); half are agnostic (lane-local arithmetic and quad swizzles).
Each one goes through **IR → HLSL → DXC (`cs_6_6`, `-O3`, `-enable-16bit-types`, `[WaveSize(32)]`) → the
GPU**, and every lane of every output is compared with the 64-lane console interpreter.

```
ran       100 agnostic (wave32, one console lane per thread) + 200 doubled (two console lanes per thread, EXEC as data)
PROVEN    every lane of every output matches the 64-lane console interpreter, on the card
PROVEN    a per-lane WaveReadLaneAt index (the doubled swizzle) is exact on this GPU
```

## Verdict

- **The whole path works end to end:** IR, emission, DXC, a pipeline, the GPU, and an exact comparison. Agnostic
  programs run as plain wave32 with one console lane per thread. Wave-wide programs run doubled, with EXEC as
  data. Every agnostic program was also run doubled; both agree.
- **The open question in the design is closed:** a per-lane lane index in `WaveReadLaneAt` is exact on this
  card, so no LDS fallback is needed for swizzles.
- What it does not yet cover: control flow (step 1 in `secondary-agent.md` section 9), memory (step 2), and real
  decoded programs (step 3).
