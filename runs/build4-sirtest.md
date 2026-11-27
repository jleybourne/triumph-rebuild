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

## Then: control flow and live masks (`runs/build4-sirtest-branches.txt`)

The IR gained structured `If` / `Else` / `EndIf` / `Phi`, both interpreters gained an EXEC stack, and the
emitter decides per branch: a **host branch** when the condition is uniform or nothing inside reads other
lanes, **predication** (both parts run, EXEC narrows as data, phi becomes a select) otherwise. 400 programs,
half of them with branches nested to depth 2:

```
ran       201 agnostic (wave32, one console lane per thread) + 400 doubled (two console lanes per thread, EXEC as data)
branches  182 lowered to host branches (uniform condition, or no cross-lane op inside) · 124 predicated (EXEC as data)
PROVEN    every lane of every output matches the 64-lane console interpreter, on the card
PROVEN    live masks: host branches and predicated regions both exact, nested up to depth 2
```

Before the GPU, the interpreters agreed bit for bit on **100,000 random branched programs**. One convention
was fixed on the way: a `ReadFirstLane` with no lane active reads lane 0. Hardware never runs it, but both
sides must agree. DXC's HLSL 2021 needs `and()` for `bool2` EXEC masks, not `&&`.

## Verdict

- **The whole path works end to end:** IR, emission, DXC, a pipeline, the GPU, and an exact comparison. Agnostic
  programs run as plain wave32 with one console lane per thread. Wave-wide programs run doubled, with EXEC as
  data. Every agnostic program was also run doubled; both agree.
- **The open question in the design is closed:** a per-lane lane index in `WaveReadLaneAt` is exact on this
  card, so no LDS fallback is needed for swizzles.
- What it does not yet cover: control flow (step 1 in `secondary-agent.md` section 9), memory (step 2), and real
  decoded programs (step 3).
