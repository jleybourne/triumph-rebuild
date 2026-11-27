# Design — the shader core (plan decision 4)

Status: **designed 2026-11-27. Built: the IR, the classifier, the lane model, HLSL emission (proven on the card, `sirtest`, 300 runs exact), the v0 lifter (straight-line programs) and v1 (structurized control flow, PR #26: 7,402 / 9,128 agnostic programs compile, [`runs/build4-v1.md`](../../runs/build4-v1.md)).**
Inputs: the fork's census and its cost analysis (`docs/research/native-60-gpu.md` and `docs/per-lane.md` on
`jleybourne/Triumph`), and this repository's Builds 1-2.

## The pipeline, offline, once per program

```
decode        GFX10 instructions (lane `shader`, S3: byte-exact against the whole corpus)
   ↓
CFG           basic blocks from branches; registers stay HLSL variables (no SSA here: DXC builds it)
   ↓
structurize   reducible, structured control flow (if/loop/break); an irreducible graph is refused
   ↓
classify      what the program needs from the wave (below): most programs need nothing
   ↓
uniformity    every value: Uniform (one per wave) or Varying (one per lane)
   ↓
lower         memory → window addressing; wave64 → wave32 by the class; native 16-bit, quad and wave ops
   ↓
HLSL          one compute/graphics entry, SM 6.6, [WaveSize(32)], -enable-16bit-types
   ↓
DXC -O3       Microsoft's compiler (1.8, in the Windows SDK) optimises and signs the DXIL
   ↓
library       hash-keyed; pipelines built at start; 0 compiles in a frame
```

**Why HLSL and DXC rather than our own DXIL writer:** DXC runs the full LLVM optimiser on every program,
which is worth more GPU time than any emitter we could write. Writing DXIL bitcode by hand is weeks of work
that carries no performance. Compile time doesn't matter, because it is offline and cached. Checked:
`dxc -T cs_6_6 -enable-16bit-types` accepts `[WaveSize(32)]`, `WaveActiveSum`, `QuadReadLaneAt` and
`float16_t` (DXC 1.8.2502.11).

## Classify first: most programs never see the wave size

A console wave is 64 lanes, and NVIDIA's is 32. That matters only to code that can *observe* the wave:

```
class            what the program does                                  how it runs here
agnostic         no cross-lane op wider than a quad, no wave-size        plain wave32: 0 overhead
                 dependence (pixel derivatives are quad-level)
wave-wide        ballots, reductions, readlane, DPP row/bcast across     lane doubling (compute) or
                 the full 64, or LDS laid out by wave id                 a 64-thread group (graphics)
```

The fork runs **every** wave64 program as two host waves exchanging halves through LDS (16.5 % of
instructions in divergence overhead, and refusals where a reduction meets inactive lanes). Classifying
removes that cost from every `agnostic` program. The census (lane `shader`, S2) counts each class.

## Lane doubling: one host lane carries two console lanes

For a `wave-wide` compute program, host lane *i* of a 32-wide host wave carries console lanes *i* and
*i + 32*:

```
VGPR value      a pair (lo, hi)                                   2 registers instead of 1
EXEC            a pair of booleans (lo, hi), carried as DATA       exact, never approximated
SGPR value      one value per host wave (uniform)                  scalar work done once, not twice
reduction       WaveActiveOp(op(lo if exec.lo else id, hi if exec.hi else id))
ballot          WaveActiveBallot(exec.lo && p.lo) | (… hi) << 32
readlane k      k < 32 ? WaveReadLaneAt(lo, k) : WaveReadLaneAt(hi, k - 32)
DPP across 32   a swap inside the thread: no LDS, no barrier
```

This is the per-lane model the fork's own `per-lane.md` asks for ("EXEC as data, live-mask analysis"):

- Inactive console lanes contribute the identity element, so a reduction "where not every lane runs" is
  exact. That removes the fork's two refusals and its approximate `WAVE_MASKED` mode.
- Cross-half exchanges are register moves, not LDS round trips and barriers.

The cost is twice the vector registers in wave-wide programs only, which the census can size before it
matters.

**Proven in `crates/sir`** (`lanes.rs`): the doubled model, using only host-wave operations, matches a
reference 64-lane console bit for bit on **100,000 random programs**. Their EXEC masks cover the full
wave, one half only, single lanes and random masks. The fork's refusal case (a reduction where only
lanes 3, 40 and 63 run) is exact.

Two conditions came out of building it, and both are rules for emission:

1. ✅ **Wave ops run in host-uniform control flow** (built: `hlsl::plan`, proven on the card). EXEC is data, so a console branch that is divergent and
   contains a wave op is emitted as predication (both sides run, selects merge), not as a host branch.
   Live-mask analysis decides where a real branch is safe: no wave op inside, or the condition is uniform.
2. **A swizzle reads another host lane with a per-lane index** (`WaveReadLaneAt(x, h ^ m)`). NVIDIA
   hardware has this shuffle, but the HLSL documentation asks for a uniform index. **Checked on the card
   (`sirtest`): exact.** No LDS fallback is needed.

Graphics stages cannot double (one pixel is one thread). A `wave-wide` pixel or vertex program runs as
two host waves, with the exchange done through wave intrinsics where the operation allows, and LDS
otherwise. The census says how many such programs exist before any work goes into them.

## Uniformity: scalar work stays scalar

SGPRs are wave-uniform by construction on the console. In the IR every value is tagged `Uniform` or
`Varying`, propagated forward, and the tag decides lowering:

- uniform loads use `WaveReadLaneFirst` on the address, so the driver uses its uniform datapath;
- uniform branches stay plain branches (no per-lane masking);
- in doubled programs, scalar ops are emitted once per host lane rather than once per half.

Fork estimate for scalar work done per lane: 3-6 ms.

## Memory: console addresses are window addresses

Build 1 put the title's memory at its own addresses in 4 GiB windows. A global access is therefore:

```
window  = address >> 32          usually uniform → one descriptor (WaveReadLaneFirst)
offset  = address & 0xFFFF_FFFF  a raw-buffer offset: 32 bits are exactly one 4 GiB window
load    = ResourceDescriptorHeap[windows + window].Load(offset)
```

There is no page-directory load, no bounds clamp and no fault store. Unmapped tiles read zero and drop
writes (tiled resources tier 2+, proven in Build 1). Fork estimate for its current per-access lookup:
2-5 ms. **Built and proven on the card** (`sirtest`: loads and stores through two tiled windows, exact).

## Native operations

| console | here | why |
|:--|:--|:--|
| 16-bit ALU (`v_pk_*`, `*_f16`) | `float16_t` / `uint16_t`, `-enable-16bit-types` | the fork widens to f32 with conversions (1-3 ms) |
| DPP `quad_perm`, `ds_swizzle` quad modes | `QuadReadLaneAt`, `QuadReadAcrossX/Y/Diagonal` | the fork emulates with lane arithmetic (~1 ms) |
| DPP `row_*` within 16 lanes, `ds_permute` | `WaveReadLaneAt` with a computed lane | one intrinsic |
| `s_barrier`, LDS | `GroupMemoryBarrierWithGroupSync`, `groupshared` | direct |
| root signatures | always `SAMPLER_HEAP_DIRECTLY_INDEXED` + `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` | the fork's 311-refusal trap |

## Correctness: the oracle

Every program is run twice on the same inputs: our translation, and the fork's translation of the same
hash (data, not code). Outputs are compared per program, and a program counts only when they match, or
differ with a named reason (the fork's approximate masked reductions are a known reason).

Before the corpus there are **lane-model tests** in `crates/sir`: a reference wave64 interpreter
against the doubled wave32 model, on reductions, ballots and readlanes under random EXEC masks. They must
agree bit for bit.

## Translation from GFX10 (decided 2026-11-27, second lead session)

The decoder (lane `shader`, PRs #5 and #10) gives every instruction of all 25,694 programs, byte-exact. The
translation decisions, each for performance or for the shortest correct path to it:

1. **Registers become HLSL variables; DXC builds the SSA.** Each SGPR and VGPR the program touches is a local
   variable, and each instruction becomes a statement over them. DXC's `-O3` runs mem2reg, GVN and dead-code
   removal, so its output equals hand-built SSA, without writing an SSA pass. `sir`'s own IR stays for the
   wave-level decisions (classify, uniformity, lane doubling), where DXC can't help.
2. **EXEC is a per-lane boolean** for agnostic programs: each host lane is one console lane and knows only its
   own bit. Lane masks used only as masks (VCC, compare results, saved EXEC in SGPRs) are per-lane booleans
   too, and `s_and_saveexec_b64` and friends become per-lane boolean operations. A mask read as a number
   (`s_bcnt1`, a mask moved to a VGPR) makes a program wave-wide, which the classifier already catches.
3. **Divergence by predication, branches only where the console's are uniform.** VALU writes are guarded by
   the lane's EXEC. Only scalar branches (`s_cbranch_scc*`, `vccz`, `execz`) become HLSL control flow.
4. **Control flow, in steps:** v0 handles straight-line programs. v1 structurizes the reducible CFG (99.18 %
   of programs, S4) into if/loop. A loop-plus-switch dispatcher is the fallback for the 20 irreducible compute
   programs and the 195 ES programs that end in an indirect jump.
5. **The interface is the fork's** (its root bindings, user data and IO, read from the oracle's `.vsh`
   entries), so the oracle comparison is like for like. Whether another binding layout is faster is decided
   in Build 5 by the frame line, not now.

**v0 target, from `agents/shader-census.csv`:** 2,881 programs that are straight-line (one block) and
agnostic: 1,798 PS, 951 VS, 132 CS. 1,813 of them use 16-bit work, 17 use LDS, none use scratch.

## Order of work

1. ✅ `crates/sir`: IR types, uniformity analysis, the wave-size classifier, and the lane-doubling model
   with its interpreter tests (100k random programs, bit-exact).
2. ✅ The lifter from decoded GFX10: v0 (one block), then v1 (structurized control flow, PR #26). No SSA of our
   own (DXC builds it); irreducible graphs are refused, not node-split.
3. ✅ HLSL emission and DXC compilation, both modes, proven on the card ([`runs/build4-sirtest.md`](../../runs/build4-sirtest.md)).
   Then census progress once decoded programs arrive.
4. Lane doubling emission, then the graphics fallback, by census weight.
5. The oracle diff across all 25,694 programs.
