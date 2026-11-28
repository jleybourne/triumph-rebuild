# Build 4 — execution against the oracle (lane `shaderexec`)

`execcheck` · 2026-11-28, re-run 2026-11-29 on the current translator (post-remap, after #37/#65/#70) · **WARP** (the lead's rule after three driver resets on 2026-11-27: translated programs
run on the software rasterizer; the card only for a straight-line smoke set under `agents\CARD`). DXC 1.8.2502.11.
Code: `apps/rebuild/src/bin/execcheck/`. Per program: [`build4-exec.csv`](build4-exec.csv) (hash, stage, v0/v1,
verdict, reason).

```
REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin execcheck -- --stage cs|ps|vs
    [--only <hash>,... | @<file>] [--v0] [--gis] [--debug] [--out <dir>]      (REBUILD_EXEC_GBV=1: GPU-based validation)
```

## The counted number

```
shaders counted (bit-exact on every input set)      5,994 / 25,694     compute 369 · pixel 4,633 · vertex 992
                                        was         5,195              compute 252 · pixel 3,980 · vertex 963
v0 set (straight-line agnostic, 2,881)              2,815 bit-exact    compute 121 / 132 · pixel 1,763 / 1,798 · vertex 931 / 951
untested (of the 9,128 agnostic)                    1,331              was 2,837
```

A program **counts** when every output word of ours equals the oracle's on all three input sets and at least one set
wrote something. The `-Gis` variant (IEEE strictness: 28 more programs bit-exact in the first pass) was not re-run;
the counted number never assumes it. Whether to compile with `-Gis` is a performance decision for the lead
(`rebuild_pipelines::translate::DXC_ARGS`).

## Per stage

| stage | set | programs | match (bit-exact) | differ | vacuous | error | crash | untested |
|:--|:--|--:|--:|--:|--:|--:|--:|--:|
| cs | v0 | 132 | 121 | 3 | 6 | 0 | 0 | 2 |
| cs | v1 | 553 | 248 | 79 | 34 | 7 | 6 | 179 |
| cs | all | 685 | 369 | 82 | 40 | 7 | 6 | 181 |
| ps | v0 | 1798 | 1763 | 30 | 1 | 0 | 0 | 4 |
| ps | v1 | 4991 | 2870 | 824 | 590 | 0 | 211 | 496 |
| ps | all | 6789 | 4633 | 854 | 591 | 0 | 211 | 500 |
| vs | v0 | 951 | 931 | 9 | 0 | 0 | 0 | 11 |
| vs | v1 | 703 | 61 | 3 | 0 | 0 | 0 | 639 |
| vs | all | 1654 | 992 | 12 | 0 | 0 | 0 | 650 |
| **all** | | **9128** | **5994** | **948** | **631** | **7** | **217** | **1331** |

`programs` is every agnostic program of the stage (9,128); `v0` one basic block, `v1` several. `vacuous`: nothing to
compare (the program writes nothing, or nothing on these inputs: every pixel discarded, every write out of range).
`differ` includes the float-within-ulp classes (named in the CSV).

## The second pass (2026-11-29): testing what the first could not

One pass, 3 shards side by side (`--shard k/3 --stage all`, one DXC each), WARP, batched workers (`--batch 150`, a
set that never finishes skipped after 10 s). 9,128 rows, against the first pass:

| first pass said | programs | now match | differ | vacuous | crash | untested |
|:--|--:|--:|--:|--:|--:|--:|
| untested: dynamic descriptor index over mixed views (bindless) | 953 | 14 | 503 | 20 | 102 | 314 |
| untested: the oracle packs its pixel inputs differently | 125 | 8 | 0 | 112 | 0 | 5 |
| untested: everything else (mostly not translated then) | 1,759 | 479 | 132 | 38 | 92 | 1,012 |

New matches 821 (479 newly translated by `shadercov` #37/#65, 295 that were vacuous and now compare coverage or
write, 21 that differed: `v_cvt_f16_f32` round-to-nearest-even and code-data `s_load`, both in this branch), 22
regressions (match → 7 vacuous, 6 words differ, 5 racing writes, 3 crash, 1 within 16 ulp), net **+799**.

What changed in the harness: the recording pass (`record.rs`) for bindless programs; the heap covers every index
`sir_vi` can name; the descriptor remap's page table in the shadow buffer; raw buffers read through a CBV range given
the oracle as CBVs; coverage compared for pixel programs with no outputs; the oracle's feeding vertex shader built
from its own ISG1 rows and PSV0 interpolation modes, padded so DXC keeps its rows.

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
| ps | differ | 800 | words differ (532 of all differing rows are recorded bindless programs) | 007bb29b393d08c5 0151212fe3c4e847 022c5f45e13ed7b6 |
| ps | vacuous | 587 | no output word written | 000cf64578e85220 007e40d70587a1b1 00a3fd9db2df200f |
| vs | untested | 444 | the oracle binds root views | 000113cc1e1aa543 00752e3accbe0c99 00803876d05875ef |
| ps | untested | 429 + 8 | one descriptor read as Tex(1) and Tex(5) (recorded): one slot sampled as 2D and as 2D array | 00d279b806d20217 00d2bf895b978cc9 016c1c4d4dd2de8a |
| vs · cs · ps | untested | 206 · 67 · 56 | not translated (opcode coverage: lane `shadercov`) | 01296256bd7c30a7 00167d58cc5a78d7 006267451f43e7de |
| ps · cs | crash | 211 · 6 | no input set completes (loops or device loss) | 01510150f274bf09 0203930d6dc7e88b 26815466213f68f9 |
| cs | untested | 62 | the oracle binds root views | 00f607218c949e38 010a50ce68d02c16 0743686596b14ec9 |
| cs | differ | 49 | words differ | 088735d999e0c9f4 0c35931f998617b7 17f11dd08620363c |
| cs | untested | 47 | the oracle needs wave size 64 (no adapter here runs it) | 01d0c7a8cab915bd 0709e8356abd9082 0df635ace0117319 |
| cs | vacuous | 31 + 9 | no output word written · the program writes nothing | 08055d95e9ee79c0 214cae4f6e1ef745 |
| ps · cs · vs | differ | 39 · 21 · 4 | float within 1 / 16 / 4096 ulp | 0a474d0c7d8821be 0b0fd196f77470a7 d1732ad58b731d00 |
| ps · cs | differ | 15 · 7 | racing writes (our own runs differ) | 36c0d9dac5185d9f 17218ee0955e2a1f |
| vs | differ | 8 | words differ | 05cd62db489af324 0d4b64b415259785 1f5b53605b7f844d |
| cs | error | 7 | the oracle's pipeline refused | 128424b3011dae43 1e99b7e849e2882f 3d070f0d7e05150f |
| ps | untested | 5 | the oracle's pixel pipeline refused (E_INVALIDARG, signatures match ours) | 1a3c05a4c0bec967 57f90253f26a29c7 |

What each needs:

- **Recorded bindless programs that differ (532)**: most match on set 0 (small integers) and differ on set 1
  (floats in [-2, 2)): 333 differ on set 1 only, 126 on sets 1 and 2, 61 on all three. An index computed from float
  data names slots the recording gave a dummy; the oracle, with its own descriptor arithmetic, reads another slot.
  First read: whether the two sides agree on the index (record the oracle's too), else input sets whose words used
  as indices land in the filled region.
- **One slot read as 2D and 2D array (437 pixel)**: a recorded slot sampled both ways; needs the harness to serve one slot
  to both declarations (two views of the same texture are not possible in one slot; a per-side plan or a copy).
- **The oracle binds root views (444 vertex, 62 compute)**: the harness has no root-descriptor path for the oracle's
  root signature yet.
- **Crash (217)**: 102 are bindless programs whose recorded indices loop or leave every view; 92 are newly
  translated. Each named set is in `hung.k.txt` (965 sets across the three shards).
- **Vacuous pixel programs (587)**: alpha tests that discard every pixel on these inputs. Next: an input set built
  to pass the test (textures near 1).
- **Wave size 64 (47 compute oracles)**: the oracle declares `[WaveSize(64)]`; neither WARP nor the RTX runs it.

## On the card

None of these numbers come from the card. `--card` runs at most 20 straight-line programs under `agents\CARD`;
it was not run after the rule (the card lock was held by other lanes while this report was written).
