# Build 4 — execution against the oracle (lanes `shaderexec`, `shaders`)

`execcheck` · 2026-11-28, re-run 2026-11-29 on the current translator (post-remap, after #37/#65/#70), third pass
2026-11-29 (lane `shaders`: the affected programs only) · **WARP** (the lead's rule after three driver resets on 2026-11-27: translated programs
run on the software rasterizer; the card only for a straight-line smoke set under `agents\CARD`). DXC 1.8.2502.11.
Code: `apps/rebuild/src/bin/execcheck/`. Per program: [`build4-exec.csv`](build4-exec.csv) (hash, stage, v0/v1,
verdict, reason).

```
REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin execcheck -- --stage cs|ps|vs
    [--only <hash>,... | @<file>] [--v0] [--gis] [--debug] [--out <dir>]      (REBUILD_EXEC_GBV=1: GPU-based validation)
```

## The counted number

```
shaders counted (bit-exact on every input set run)  6,188 / 25,694     compute 401 · pixel 4,467 · vertex 1,320
                                        was         5,994              compute 369 · pixel 4,633 · vertex 992
  of them on all three sets                         5,821              was 5,950 (the third pass, below)
v0 set (straight-line agnostic, 2,881)              2,824 bit-exact    compute 121 / 132 · pixel 1,765 / 1,798 · vertex 938 / 951
untested (of the 9,128 agnostic)                    196                was 1,331
```

A program **counts** when every output word of ours equals the oracle's on every input set that completes (a set that
never finishes or loses the device is skipped: 367 of the 6,188 match on fewer than three sets) and at least one set
wrote something. The `-Gis` variant (IEEE strictness: 28 more programs bit-exact in the first pass) was not re-run;
the counted number never assumes it. Whether to compile with `-Gis` is a performance decision for the lead
(`rebuild_pipelines::translate::DXC_ARGS`).

## Per stage

| stage | set | programs | match (bit-exact) | differ | vacuous | error | crash | untested |
|:--|:--|--:|--:|--:|--:|--:|--:|--:|
| cs | v0 | 132 | 121 | 4 | 6 | 0 | 0 | 1 |
| cs | v1 | 553 | 280 | 87 | 60 | 7 | 36 | 83 |
| cs | all | 685 | 401 | 91 | 66 | 7 | 36 | 84 |
| ps | v0 | 1798 | 1765 | 31 | 1 | 0 | 0 | 1 |
| ps | v1 | 4991 | 2702 | 876 | 1121 | 0 | 242 | 50 |
| ps | all | 6789 | 4467 | 907 | 1122 | 0 | 242 | 51 |
| vs | v0 | 951 | 938 | 8 | 0 | 0 | 0 | 5 |
| vs | v1 | 703 | 382 | 259 | 0 | 0 | 6 | 56 |
| vs | all | 1654 | 1320 | 267 | 0 | 0 | 6 | 61 |
| **all** | | **9128** | **6188** | **1265** | **1188** | **7** | **284** | **196** |

`programs` is every agnostic program of the stage (9,128); `v0` one basic block, `v1` several. `vacuous`: nothing to
compare (the program writes nothing, or nothing on these inputs: every pixel discarded, every write out of range).
`differ` includes the float-within-ulp classes (named in the CSV).

## The third pass (2026-11-29, lane `shaders`): the inputs fixed, then the translator

Only the programs each change touches were run again (WARP, 1-3 shards, one DXC each); every other row keeps the
second pass's verdict. Per step, in commit order:

| step | programs run | before | after | counted |
|:--|--:|:--|:--|--:|
| 1 · index words hold indices (bindless, recorded) | 532 | differ 532 | match 6 · vacuous 331 · differ 187 · crash 4 · untested 4 | +6 |
| 1 · the same, on the recorded programs that matched | 566 + 489 | match 1,055 | match 850 · vacuous 199 · crash 6 | **−205** |
| 2 · one texture read as 2D and 2D array | 608 | untested 437 · differ 171 | match 18 · differ 570 · crash 18 · vacuous 2 | +18 |
| 3 · the oracle's root SRVs fed | 508 | untested 508 | match 297 · differ 165 · crash 23 · vacuous 17 · untested 6 | +297 |
| translator: `v_fract`, `v_sin`/`v_cos` in turns, `v_rcp` | 416 | differ 416 | match 17 · differ 396 · crash 3 | +17 |
| translator: an image descriptor's base level | 329 | untested 329 | match 61 · differ 118 · vacuous 8 · crash 13 · untested 129 | +61 |

**The correction (−205).** The second pass counted recorded bindless programs whose only writing input set was one
where a float word, used as a descriptor index, named an index past the console range. Ours reads the null view there
(by design, `sir_vi`); the recording pass then bound a seeded view into that null-bank slot, and the oracle read far
past its heap (undefined; WARP gave zeros). Both "matched" on nothing real. Now the recording pass never binds the
null bank, and a set whose indices leave the heap has the buffer words that feed them found and given set 0's word on
both sides (all buffer words from set 0, then whole buffers, halves, single words back while the indices stay in the
heap; 37-100 recording runs a set on the samples). Index words then hold small integers, as the title's do; every
other word keeps the set's floats. On that data these programs discard every pixel (vacuous: the alpha-test bucket)
or loop. The same artefact can remain in a filled dynamic region (an index from the data past the heap, not
recorded): not re-run.

**Step 2.** The slot read both ways gets one 2D-array view of an array texture, read by both declarations (the same
descriptor on both sides; a single-slice view gave the same results). A dynamic region over a plain and an array
texture goes to the recording pass, so its index words are kept in the heap too. Most of these programs now run and
differ on the float sets (1 and 2): the translator rows below are where that leads.

**Step 3.** Every root view the oracle binds is a root SRV `t<k>, space1` for the title's root descriptor at root
parameter `k` (372 of 508 with the live-slot table `t255` beside it). The plan gives that descriptor an address at
offset 0 of its own window (8, 9, …), ours reads the window's raw view (the harness heap now reaches the first 32
windows), and the oracle's root signature gets one root SRV per register bound to the same buffer. The live table
still needs `REBUILD_EXEC_LIVE=1` (set for this pass). Compute programs with the live table mostly never finish a
set on WARP (or grow past 3 GB and were stopped): crash. On set 1 the vertex programs lose the WARP device (an
offset from float data through a root SRV, which has no bounds); their sets 0 and 2 decide, under the existing rule
(a set that does not complete is skipped, `hung.txt`).

**The translator.** With real float data three VALU results parted: `v_fract_f32` never returns 1.0 on the console
(the host's `frac` rounds a tiny negative fraction up to 1.0); `v_sin`/`v_cos` take turns and take the whole turns
off before scaling (exact, and a half turn's sine is +0); `v_rcp_f32` as a correctly rounded division (DXC's `rcp`
is a fast one). 489 sampled current matches that use these ops: no translator regression (their 48 non-matches are
the correction above). Cost: a full division where a fast one was; the lead may prefer the fast one (the console's
own `v_rcp` is 1 ulp).

**Counted: 6,188 / 25,694** (was 5,994): +399 new matches, −205 artefacts withdrawn. Under the existing rule a set
that never completes is skipped, and a match on the sets left counts: **367** of the 6,188 match on fewer than three
sets (was 44 of 5,994), 270 of them the root-view vertex programs above. Counting only matches on all three sets:
**5,821** (was 5,950). Whether the rule should require all three is the lead's call; the per-program detail is in the
pass CSVs.

## The 329 programs not translated (step 4)

Bucketed by the first refusal (`translate v1`, lift only), before and after this pass:

| refusal | before | after | the older build (fxceless-victory) |
|:--|--:|--:|:--|
| `s_bfe_u32` of an image descriptor's word 3, field 12-15 (base level): 173 vs · 41 cs · 15 ps | 229 | 0 | rebuilt the descriptor's words from the host view (word 3: identity swizzle, base level 0, last level from the view's levels, type); **taken here: base level 0, identity swizzle** |
| `image_sample`: sampler not traced (28 ps · 1 cs) | 29 | 29 | traces a sampler through joins and loads at computed table addresses |
| `v_movrels_b32` / `v_movreld_b32` (M0-relative registers) | 16 | 16 | registers as an array indexed by M0 |
| `exp` target 13 (pos1) | 11 | 13 | pos1/pos2 carry the clip then cull distances, in semantic order |
| `s_lshr_b32` / `s_bfe_u32` of a descriptor word the lifter cannot name | 10 | 10 | |
| a lane mask read as data (`s_and`, `s_bfe`, `s_bcnt1`) | 8 | 8 | ballots of masks read as data |
| `s_load_dword` (reached after word 3) | 0 | 7 | |
| `v_med3_f16` | 6 | 6 | f16 med3 with the NaN rules |
| `s_and_b32` of an image descriptor's word 4 (depth or layers) | 3 | 3 | word 4 from the view's depth or layers |
| irreducible control flow | 3 | 3 | |
| an output parameter in no signature element (reached after word 3) | 0 | 2 | |
| one or two each (16-bit image, global and LDS forms, `image_gather4_l`, `image_sample_o`, `v_interp_p2_f32`, `s_setpc_b64`) | 14 | 14 | |
| **all** | **329** | **111** | |

The older build's shader cache (`%LOCALAPPDATA%\victory\cache\shaders`, 32,744 entries, read only) is keyed by its
translator's fingerprint, not the program hash: an entry was matched to a program by what the console program decides
in it (stage, user-data and sampler words, static samplers) and its DXIL's input and output signatures, against the
oracle entry of the same hash. **323 of the 329** have a matching entry (61 entries match more than one program); the
6 without one are 4 word-3 programs, 1 lane mask and `v_interp_p2_f32`. So the older build translated nearly all of
them; what it did differently is in the right-hand column (from its source, used as a reference only; nothing of its
output is copied).

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
| third pass: `v_fract` never 1.0, `v_sin`/`v_cos` in turns, `v_rcp` a correctly rounded division | `016c1c4d4dd2de8a` (bindless, once its index words held indices) | 416 differing → 17 match; no regression in 489 sampled matches |
| third pass: an image descriptor's base level (word 3, bits 12-15) read as data is 0, the swizzle the identity | 229 refusals, one instruction (`s_bfe_u32 …, 0x4000c`) | refused 329 → 111; of the 329, 61 match |

Every `sirtest` and random-program test is unchanged (the IR, emitter and interpreters are untouched); `cargo test
--release --no-fail-fast` after merging master (#36): 500 passed, 0 failed; one warning, from master (`rebuild-xlist` lib test, an unused `HRESULT`). After the merge the v0 compute set still gives 110 matching.

## The buckets, biggest first (the second pass; the third pass's work on them is above)

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

- **Recorded bindless programs that differ (532)**: done in the third pass (index words): the indices from float data
  left the heap on both sides; with index words holding indices 331 discard every pixel, 187 still differ, 6 match.
- **One slot read as 2D and 2D array (437 pixel)**: done in the third pass (one array view for both declarations).
- **The oracle binds root views (444 vertex, 62 compute)**: done in the third pass (root SRVs over windows); the
  compute programs with the live table never finish a set on WARP.
- **Next, from the third pass**: the float sets (1 and 2) of the recorded programs differ in 570 + 187 rows (an
  example, `0d46328910533465`: `o0.w = vcc ? k : 0` is 0 in ours, k in the oracle's, on set 0 too); the root-view
  vertex programs lose the WARP device on set 1 (an offset from float data through a root SRV); the recording pass's
  index search grows a worker past several GB on a few compute programs (stopped by hand; `INDEX_TESTS` 600 is
  generous, the samples needed 37-100).
- **Crash (217)**: 102 are bindless programs whose recorded indices loop or leave every view; 92 are newly
  translated. Each named set is in `hung.k.txt` (965 sets across the three shards).
- **Vacuous pixel programs (587)**: alpha tests that discard every pixel on these inputs. Next: an input set built
  to pass the test (textures near 1).
- **Wave size 64 (47 compute oracles)**: the oracle declares `[WaveSize(64)]`; neither WARP nor the RTX runs it.

## On the card

None of these numbers come from the card. `--card` runs at most 20 straight-line programs under `agents\CARD`;
it was not run after the rule (the card lock was held by other lanes while this report was written).
