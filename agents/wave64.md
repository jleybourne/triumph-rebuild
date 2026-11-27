# Lane `wave64` — wave-wide programs (Build 4)

Status: first hand-back (2026-11-28). Branch `agent/wave64`, PR #36.

## Built

- `crates/sir/src/lift/wave.rs`: the wave mapping per program (`Wave::choose`), from the census class, the
  console wave size and the stage:

  ```text
  agnostic, or every v_readfirstlane operand provably uniform   Agnostic    as before (v1)
  console wave32, any stage                                     Native32    host wave32 ([WaveSize(32)] in compute)
  console wave64 pixel / vertex                                 Partial64   a host wave (<= 64) is a partly filled console wave64
  console wave64 compute                                        Doubled     host lane i carries console lanes i and i + 32
  ```

  Cross-lane operations for the three wave-wide mappings: lane masks read as numbers (ballots), numbers read as
  masks, `v_mbcnt_lo/hi`, `v_readlane`, `v_readfirstlane` (exact first active lane), `v_writelane`, DPP16 (all row
  patterns, row/bank masks, `bound_ctrl`, `fi`), DPP8, `v_permlane16/x16`, `ds_bpermute`, 64-bit bit counts and scans.
  Doubling is done on the lifter's text: each instruction that names a per-lane value is written once per half with
  `_0` / `_1` names; reads across halves name the half; statements that must run once go before/after.
- The uniformity pass for `ReadFirstLane` programs (`rfl_uniform`).
- `translate v2 [ps] [vs] [cs]`: every program of every class, compiled by DXC, split by class (4 workers).
- `crates/pipelines/src/translate.rs`: `lift_hlsl` picks the wave mapping (`wave_of`); `check_kind` no longer
  refuses wave-wide programs.
- `crates/sir/src/lift/wavegen.rs`: random wave-wide GFX10 programs and a 64-lane reference console.
- `wavetest`: those programs lifted, compiled, run on WARP (or the card) and compared lane by lane.
- `wavecost`: real wave64 compute programs, doubled against two host waves exchanging through groupshared memory.

## Measured ([`runs/build4-wave64.md`](../runs/build4-wave64.md))

```text
translate v2   9,415 compiled (pixel 7,840 · vertex 987 · compute 588); wave-wide 1,892 (0 before); 0 DXC errors
wavetest       300 / 300 random wave-wide programs exact on WARP; 150 / 150 on the card with the real intrinsics
wavecost       two host waves through LDS take 1.41× the doubled GPU time (8 real programs, card; 0.98×–2.48×)
```

## Next

- Wave-wide refusals that are wave work: descriptor words read as data in pixel shaders (~830), descriptors moved
  through `v_readfirstlane` in vertex shaders (~370), SGPR spills of descriptors through `v_writelane` (~150).
- The rest of the buckets are opcode coverage (SDWA ~1,900 first): lane `shadercov`.
- Mesh / ES / GS programs (9,155, all wave-wide): need stage IO first; a mesh program is compute-like and can
  double the same way.

## For the lead

- `lift.rs` `wrote()`: in wave32 it ended the lane mask of SGPR `s - 1` too (a wave64 pair rule). Fixed (only in
  wave64); agnostic compute went 335 → 361 compiled in the same run.
- Agnostic wave64 compute already branches on `WaveActiveAnyTrue` over a host wave that holds half the console
  wave: exact only when the branch is uniform over the console wave (VCC from uniform compares). Unchanged here.
- Wave32 graphics programs (`Native32`) need a host wave of at most 32 lanes (NVIDIA, Intel; AMD may run pixel
  shaders as wave64).

- Lock incident: a failed `wavetest --card` run exited through `process::exit` and left `agents\CARD` (pid 26820)
  for ~30 min; I removed it after checking the pid was gone. `wavetest` and `wavecost` now drop the lock first.
