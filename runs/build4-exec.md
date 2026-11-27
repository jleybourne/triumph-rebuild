# Build 4 — execution against the oracle (lane `shaderexec`)

`execcheck` · 2026-11-28 · **WARP** (the lead's rule after three driver resets on 2026-11-27: translated programs
run on the software rasterizer; the card only for a straight-line smoke set under `agents\CARD`). DXC 1.8.2502.11.
Code: `apps/rebuild/src/bin/execcheck/`. Per program: [`build4-exec.csv`](build4-exec.csv) (hash, stage, v0/v1,
verdict, reason).

```
REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin execcheck -- --stage cs|ps|vs
    [--only <hash>,... | @<file>] [--v0] [--gis] [--debug] [--out <dir>]      (REBUILD_EXEC_GBV=1: GPU-based validation)
```

## The counted number

```
shaders counted (bit-exact on every input set)      5,195 / 25,694     compute 252 · pixel 3,980 · vertex 963
  + float within a few ulp, bit-exact under -Gis       +28 → 5,223      named: DXC's float contraction or reassociation
v0 set (straight-line agnostic, 2,881)              2,762 bit-exact    compute 110 / 132 · pixel 1,733 / 1,798 · vertex 919 / 951
```

A program **counts** when every output word of ours equals the oracle's on all three input sets and at least one set
wrote something. 28 more differ only within 1-4096 ulp and become bit-exact when ours is compiled with `-Gis` (IEEE
strictness): the task's "ULP-tolerant with a named reason". Whether to compile with `-Gis` is a performance decision
for the lead (`rebuild_pipelines::translate::DXC_ARGS`); the counted number above does not assume it.

## Per stage

| stage | set | programs | match (bit-exact) | match (named ulp) | differ | vacuous | error | crash | untested |
|:--|:--|--:|--:|--:|--:|--:|--:|--:|--:|
| cs | v0 | 132 | 110 | 0 | 2 | 5 | 0 | 0 | 15 |
| cs | v1 | 553 | 142 | 3 | 23 | 17 | 1 | 0 | 367 |
| cs | all | 685 | 252 | 3 | 25 | 22 | 1 | 0 | 382 |
| ps | v0 | 1798 | 1733 | 6 | 21 | 31 | 0 | 0 | 7 |
| ps | v1 | 4991 | 2247 | 16 | 235 | 723 | 0 | 6 | 1764 |
| ps | all | 6789 | 3980 | 22 | 256 | 754 | 0 | 6 | 1771 |
| vs | v0 | 951 | 919 | 3 | 4 | 0 | 0 | 0 | 25 |
| vs | v1 | 703 | 44 | 0 | 0 | 0 | 0 | 0 | 659 |
| vs | all | 1654 | 963 | 3 | 4 | 0 | 0 | 0 | 684 |

`programs` is every agnostic program of the stage (9,128); `v0` one basic block, `v1` several. `vacuous`: nothing to
compare (the program writes nothing, or nothing on these inputs: every pixel discarded, every write out of range).

## How a program runs

- **Inputs**, from what the program reads (`rebuild_sir::lift::Lifted::desc_uses`: every descriptor read, its table,
  its byte offset when constant, and the view) and its root layout: user data as root constants (`b0`); each
  descriptor table at its own place in the heap; in each slot a view of the kind the program reads; three seeded
  sets (small integers, floats in [-2, 2), a mix); textures hold floats in [0, 1); UAVs, targets and stream-output
  buffers start from seeded words, so any write shows. An indexed buffer's shadow entry takes the record size the
  title's root signature gives the range (a structured buffer's stride), else the end of the furthest record read.
- **Both sides** on the same inputs: the oracle with its own static samplers (`VSH1` entry) at `space0`, ours with
  ours at `space100`. Compute: a dispatch of up to 4 × 4 groups. Pixel: a feeding vertex shader built from our input
  struct, a 64 × 64 quad with w from 1 to 2.25 (so a wrong interpolation mode shows), full 32-bit targets and depth.
  Vertex: seeded attributes for every input either side declares, 64 points × 2 instances, every output component
  through stream output (and its counter).
- **Compared**: every UAV, target, depth and stream-output word. A difference is classed as NaN payload, signed zero,
  NaN against a number, within 1 / 16 / 4096 ulp, or words differ; when ours and the oracle differ, ours runs a
  second time, and a program whose own runs differ is named *racing writes*.
- **Safety**: a worker process runs the GPU; a set that loops past 30 s or loses the device is recorded and skipped
  next time (a loop count or a descriptor index from the data); the program is `crash` only when no set completes.

## Translator fixes (`crates/sir`), each from a bucket

| fix | found on | measured |
|:--|:--|:--|
| a load through a UAV descriptor (the title's root signature names the range a UAV) is declared as one | `0f4e0a1da329909a` read 0 where the oracle read the texture | compute 173 → 222 matching |
| a buffer read through a table is a constant buffer only when its own range is one | `3fe821a8916a4564` (an SRV beside a CBV in one table) | |
| d16 typed buffer loads and stores carry halves (they moved whole floats) | `e4d850a7baf18270` | |
| med3 of a value and two constants is a clamp, max then min (NaN goes to the low bound, as on the console); a general med3 checks for NaN; `0 - x` stays a subtraction (DXC folded it to `-x`: +0 became -0); all three `precise` | `9034c0d9d567c3e5`, `565cc0b38920b229` | 13 of 38 differing compute programs match |
| buffer loads through a CBV descriptor read the host's CBV (an SRV type over a CBV descriptor lost the WARP device; GPU-based validation named it) | `001a8d494b5f60b3` | vertex programs that lost the device: 16 → 0 |

Every `sirtest` and random-program test is unchanged (the IR, emitter and interpreters are untouched); `cargo test
--release --no-fail-fast` after merging master (#36): 500 passed, 0 failed; one warning, from master (`rebuild-xlist` lib test, an unused `HRESULT`). After the merge the v0 compute set still gives 110 matching.

## The buckets, biggest first

| stage | verdict | programs | reason | examples |
|:--|:--|--:|:--|:--|
| ps | untested | 952 | dynamic descriptor index over mixed views | 007bb29b393d08c5 00d279b806d20217 01b5ba8f45e5157b |
| ps · vs · cs | untested | 692 · 684 · 350 | not translated (opcode coverage: lane `shadercov`) | 006267451f43e7de 000113cc1e1aa543 00167d58cc5a78d7 |
| ps | vacuous | 519 | no output word written | 0040e42215faaf10 007e40d70587a1b1 00a3fd9db2df200f |
| ps | vacuous | 235 | no outputs to compare (the program writes nothing) | 0156324081446274 016cca23d08da25a 01c04398d190c31b |
| ps | differ | 177 | words differ | 00df3177d86d2f14 01335908a16cddb3 035539af40c67b97 |
| ps | untested | 125 | the oracle packs its pixel inputs differently (the feeding vertex shader links to ours only) | 000cf64578e85220 089eda9f1ae026a8 0da17dda7586c5ca |
| ps | differ | 70 | float within 4096 ulp | 0360f43d8f564b04 03d540b63d10f581 045ccb32b906db0d |
| cs | untested | 21 | the oracle needs wave size 64 (no adapter here runs it) | 196bf6114ff3a131 1e39de85388c6540 34d88edf583f3778 |
| cs | vacuous | 17 + 5 | no output word written · the program writes nothing | 08055d95e9ee79c0 342bb35d9dcadea5 |
| cs | differ | 13 | words differ | 0c35931f998617b7 1c149749f2e45123 269d0f4c25ac5b3a |
| cs | differ | 10 | float within 16 ulp | 0b0fd196f77470a7 17fba369586b4b79 373b741488154ac3 |
| cs · ps | untested | 6 · 2 | the oracle binds root views | 1a77e876755eff12 01250e3bd6a6fb99 |
| ps | crash | 6 | no input set completes (loops or device loss) | 488b335d556ef168 8b80bb84637f9118 c9b1aa037ac50d98 |
| vs | differ | 4 | words differ | 1f5b53605b7f844d 50dd41e4e8574def 6823b7116ea6c249 |
| cs · ps | differ | 2 · 3 | racing writes (our own runs differ) | 17218ee0955e2a1f 36c0d9dac5185d9f |
| ps | differ | 3 | words differ, bit-exact under -Gis (DXC float freedom) | 4e2e3dc7c62a4a1b 552c1219de163b28 63e37b1200bff988 |
| cs | untested | 4 | one descriptor read as two kinds (sampled and stored, raw and CBV) | 04be1ad677ff1627 3f2cbdc7d80ca3da |

What each needs:

- **Dynamic indices over mixed views (952 pixel)**: bindless material programs index the heap from data and read
  several view kinds (2D and 2D-array textures, raw buffers, CBVs). The harness cannot place them without knowing
  each index. Next: a recording pass (our program instrumented to write each heap index it computes), then views
  placed at exactly those slots.
- **Opcode coverage (1,726)**: lane `shadercov`.
- **Vacuous pixel programs (519)**: alpha tests that discard every pixel on these inputs, and similar. Next: an input
  set built to pass the test (textures near 1).
- **Words differ (177 pixel, 13 compute, 4 vertex)**: not yet read one by one. Families repeat (the same first
  differing word across hashes), so a few reads should close many.
- **The oracle packs its pixel inputs differently (125, all v1)**: an interface difference the S6 check has not
  covered for v1 (it checked v0). Worth an `oraclediff` pass over the v1 set: whether the title's vertex shaders link
  to ours is the real question.
- **Wave size 64 (21 compute oracles)**: the oracle declares `[WaveSize(64)]`; neither WARP nor the RTX runs it.
- **Implicit-derivative sampling in compute**: the oracle samples level 0 (an approximation); ours takes host-quad
  derivatives. Named in the harness; after the fixes above no counted program falls in it.

## On the card

None of these numbers come from the card. `--card` runs at most 20 straight-line programs under `agents\CARD`;
it was not run after the rule (the card lock was held by other lanes while this report was written).
