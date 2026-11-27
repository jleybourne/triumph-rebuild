# Build 4 — wave-wide programs

Lane `wave64`, 2026-11-27 night. RTX 4070 Ti (card), WARP, DXC 1.8.2502.11. Code: `crates/sir/src/lift/wave.rs`,
`crates/sir/src/lift/wavegen.rs`, `apps/rebuild/src/bin/{translate,wavetest,wavecost}.rs`.

## The mapping

Until now only agnostic programs were translated (9,128 of the corpus). Each program now gets a wave mapping
from its census class, its console wave size and its stage (`Wave::choose`):

```text
agnostic, or every v_readfirstlane operand provably uniform   Agnostic    one console lane per host lane (as before)
console wave32, any stage                                     Native32    host wave32 = console wave ([WaveSize(32)] in compute)
console wave64 pixel / vertex                                 Partial64   the host wave (<= 64 lanes) is a partly filled console
                                                                          wave64: graphics packs lanes as it likes and leaves
                                                                          waves partly filled, so the console can run this too
console wave64 compute                                        Doubled     host lane i carries console lanes i and i + 32
```

No LDS, no barriers, no second host wave in any of them. Graphics does not double (one pixel is one thread) and
needs no 64-thread group: a wave64 pixel or vertex program sees the lanes the host wave holds, which is a valid
console wave. Compute packs threads into waves by index, so wave64 compute doubles.

## Compiled, per class and stage (`translate v2`, every program, DXC `-O3`)

```text
                         pixel              vertex            compute
agnostic              6,191 / 6,789        971 / 1,654       361 / 685
readfirstlane w32        10 / 47              —               47 / 480
readfirstlane w64     1,339 / 3,266         16 / 536          95 / 302     (not provably uniform)
wave-wide w32           113 / 959             —                4 / 939
wave-wide w64           187 / 995            0 / 1            81 / 276
all                   7,840 / 12,056       987 / 2,191       588 / 2,682
```

Wave-wide programs compiled: **1,892** (0 before). 0 DXC errors in the whole run. Agnostic programs rose too
(compute 335 → 361) from one lifter fix: in wave32 a write to SGPR `s` no longer ends the lane mask in `s - 1`.

The mesh (8,765), ES (195) and GS (195) programs are all wave-wide but have no stage IO in the lifter yet (not a
wave question); they are not in the table.

### Biggest refusals left (first reason, wave-wide classes)

```text
SDWA forms (v_cvt_f32_f16, v_xor_b32, v_cmpx_gt_u32, ...)      ~1,900   opcode coverage (lane shadercov), not wave work
s_cmp_eq_u32 / s_bfe_u32 / s_add_i32 / s_and_b32 (no reason)      ~830   a descriptor word read as data, in pixel shaders
s_buffer_load_dwordx4 (descriptor not traced), vertex            ~370   descriptors moved through v_readfirstlane
v_cvt_pk_u8_f32, v_add_co_ci_u32, image_atomic_umin, 16-bit
  image addresses, s_getpc_b64, tbuffer_load_format_xy             ~1,200  opcode coverage
v_writelane_b32 of a descriptor word (SGPR spill to a VGPR lane)    ~150   descriptors spilled through VGPR lanes
```

## Correctness: random wave-wide programs against a 64-lane console (`wavetest`)

`wavegen` writes random GFX10 compute programs that use every cross-lane form the lifter lowers: masks read as
numbers and numbers read as masks, `v_mbcnt_lo/hi`, `v_readlane`, `v_readfirstlane`, `v_writelane`, DPP16 (every
row pattern, row and bank masks, `bound_ctrl`, `fi`), DPP8, `v_permlane16/x16`, `ds_bpermute`, `ds_swizzle`,
64-bit bit counts and scans, EXEC narrowed by `s_and_saveexec` with `s_cbranch_execz` skips, nested twice.
`wavegen::run` is a plain 64-lane (or 32-lane) reading of the ISA that shares no code with the lifter. Every
output VGPR of every lane and every output SGPR of every wave is compared.

```text
WARP, 40 instructions each        doubled 100 · native32 100 · partial64 100    300 / 300 exact
  group sizes                     doubled 64 and 40 (lanes past the group start with EXEC off) · native32 32 · partial64 32
card (RTX 4070 Ti, real wave      doubled 50 · native32 50 · partial64 50       150 / 150 exact
  intrinsics, [WaveSize(32)])     doubled 64, 128, 96, 40 · native32 32, 64 · partial64 32
```

WARP's waves are 4 lanes wide, so it cannot create a `[WaveSize(32)]` pipeline. On WARP the test therefore
runs one host wave per group and maps the wave intrinsics to a 32-lane wave in groupshared memory: this checks the
lifter's lane logic (halves, masks, lane ids, orders). `wavetest --card` runs the same programs with the real
intrinsics on the card (and doubled groups of 128 and 96); its first card run read zeros everywhere because the
test buffer had no state transitions (WARP forgives that, the card does not), now fixed. That failed run also
left `agents\CARD` behind (`process::exit` skips the lock's destructor); both binaries now drop the lock first.

Found and fixed on the way: a wave intrinsic inside a short-circuit (`a && WaveReadLaneAt(...)`) is not reached
by every lane; a doubled `ds_bpermute` whose destination is its data must read both halves before either half
writes; a lane mask written on one path and data on the other met at a join (the pair's data form is now kept
current after every mask write, by a ballot DXC drops when unused).

## Cost: doubled against two host waves

`wavecost` picks real wave64 compute programs from the corpus (wave-wide, 64-thread groups, no loops: 94
candidates), translates each twice and times both on the card:

```text
doubled    one host wave of 32 per console wave; host lane i carries console lanes i and i + 32
two-wave   two host waves of 32 per console wave, exchanging halves through groupshared memory with barriers
           (the previous layer's design; Wave::TwoWave, for this measurement only)
```

Every heap access is pointed at a valid resource of its type, user data is zero, and both versions touch the
same resources: the difference is the wave mapping. The 8 with the most cross-lane work, on the card (lock
held, 5 s fence bound, one small dispatch of each first):

```text
2048 groups of 64 console threads × 8 dispatches, median of 5, GPU µs
program            cross   doubled   two-wave   two-wave / doubled
f0c91b4d2578f5bf    563      16.4       20.4     1.25×
78002af174ef11fc    266      25.6       63.5     2.48×
b4db8c3785b6fbc5    192      44.0       43.0     0.98×
a929109552ce1e84    113      57.3      119.8     2.09×
3e55d5ea47dbcfb1    105      72.7       75.8     1.04×
0f303afc7d91c03a    101     428.0      455.7     1.06×
52a86f2c41d1882b     98     217.1      441.3     2.03×
10efcf59eb60cca7     97    4746.2     5067.8     1.07×
geometric mean: two-wave takes 1.41× the doubled time (+40.5 %)
```

**Doubling is the faster design**: never more than 2 % behind, and up to 2.5× ahead where the program crosses
lanes often (each exchange through groupshared memory costs two barriers; a doubled exchange is a register
read). The doubled program's cost is its larger register count, which these numbers already include.
