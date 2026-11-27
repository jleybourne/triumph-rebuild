# Lane `wave64` — wave-wide programs (Build 4)

Status: in progress (2026-11-27 night). Branch `agent/wave64`.

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

## Measured

See [`runs/build4-wave64.md`](../runs/build4-wave64.md).

## For the lead

- `lift.rs` `wrote()`: in wave32 it ended the lane mask of SGPR `s - 1` too (a wave64 pair rule). Fixed (only in
  wave64); agnostic compute went 335 → 361 compiled in the same run.
- Agnostic wave64 compute already branches on `WaveActiveAnyTrue` over a host wave that holds half the console
  wave: exact only when the branch is uniform over the console wave (VCC from uniform compares). Unchanged here.
- Wave32 graphics programs (`Native32`) need a host wave of at most 32 lanes (NVIDIA, Intel; AMD may run pixel
  shaders as wave64).
