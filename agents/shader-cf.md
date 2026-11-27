# Lane shader-cf — v1: structured control flow

Branch `agent/shader-cf`. Task: Build 4 "Translation" v1, structurize the reducible control flow of the agnostic set.

## Built

- **`crates/sir/src/lift/cfg.rs`** turns the decoded instructions into basic blocks (`s_branch`, `s_cbranch_scc0/1`,
  `vccz/vccnz`, `execz/execnz`, `s_endpgm`). It computes reverse postorder and dominators, checks reducibility (every
  retreating edge must target a dominator), and structures the graph with Ramsey's algorithm ("Beyond Relooper", ICFP 2022).
  A loop is `for (;;) { … break; }` and a merge is `do { … } while (false)`. A branch becomes `break` / `continue`
  where it can, and otherwise a jump through `sir_to` with a dispatch after each construct it crosses. Branches that
  only fall through are dropped, and blocks nothing leaves are not emitted, so a diamond comes out as a plain `if`/`else`.
- **EXEC stays data** (shader-ir.md decision 3). Every console branch becomes a host branch on a wave-uniform value:
  `scc`, or `WaveActiveAnyTrue(vcc|ex)` for `vccz/execz`. Divergence is predication by `ex`, as in v0. Host control
  flow is therefore always uniform: quad ops and derivatives see every lane, and `s_cbranch_execz` still skips a
  region no lane of the host wave runs.
- **Flow-sensitive lifter facts** (`FlowState`): descriptors, constants, table offsets, must- and may-written SGPRs,
  which SGPRs hold a current lane mask (`m{n}`) and which data variables are stale, and whether EXEC is narrowed. These
  are met at joins and iterated to a fixed point around loops (at most 16 passes; it settles in 2-3). A lane mask
  read as data is refused ("a lane mask read as data") instead of silently reading a stale `s{n}`.
- **Lane-mask ops**: the whole save-exec family (`s_{and,or,xor,xnor,andn1,andn2,orn1,orn2,nand,nor}_saveexec`,
  `*_wrexec`), `s_{and,or,xor,andn2,orn2,nand,nor,xnor}_b64`, `s_mov_b64` of masks/constants, `s_cselect_b64` of
  masks or 0/-1, `s_wqm` on any mask, and the `_b32` forms for wave32 programs. SCC from a mask op is
  `WaveActiveAnyTrue(result)`, emitted only where SCC is live (backward liveness over the graph).
- Beside control flow, to unblock the multi-block set: `s_addk_i32`, `s_mulk_i32`, `s_cmpk_*`;
  `image_sample_c(_lz)` (heap and immediate comparison samplers), `image_sample_d` (SampleGrad), `image_get_lod`
  (pixel only); DPP16 `quad_perm` as `QuadReadLaneAt` (with `fi` / `bound_ctrl` semantics for lanes EXEC has off).
  SDWA / DPP refusals are now named.
- **`translate v1`**: every agnostic VS, PS and CS through `lift_program`, reported per stage and split into one
  block and several.

### The lead's two io.rs fixes (in this branch)

1. fp16 interpolation: component `c = 2 * chan + high`, from `row` when `c < 4`, else `row2` at `c - 4`; the read
   mask follows. oraclediff `InputReads`: **62 → 0**.
2. A pixel shader with no exports and an empty OSG1 is `void main` with no output (68954fce6cd4d34e): `SigOut` **1 → 0**.

Interface diffs over the lead's original 2,834 v0 programs: **0**. Two of the three programs that are new to v0 (PS
59896277ae8f8dac, b5f5c17aea75dd85, unblocked by mask `s_cselect_b64`) show `SigIn`: element TEXCOORD22.w shares
input register 6 with TEXCOORD3 in ours, while the oracle packs it at r13. This comes from signature packing (DXC
assigns registers from our `In` struct), not from control flow. It is for the lead.

### Sampler sidecar (for crates/pipelines)

Each `<hash>.<stage>.dxil` that `translate` writes gets a `<hash>.<stage>.samplers` beside it, in little-endian:
`u32 count`, then `count × 4 u32` (the console sampler words of `sir_imm0…`, in register order), then one `u32` whose
bit k is set when `sir_imm<k>` is read by a comparison (`SamplerComparisonState`; its static sampler needs a
comparison filter). A reader that stops after the words still works.

## Measured

`REBUILD_SHADERS=D:\victory-data\shaders.rpf REBUILD_TRANSLATE_OUT=D:\victory-data\cf\v1 translate v1` (outputs and
raw text outside git, `D:\victory-data\cf`):

| stage | agnostic programs | compiled | one block (v0) | several blocks |
|:--|--:|--:|--:|--:|
| PS | 6,789 | **6,097 (89.8 %)** | 1,794 / 1,798 | **4,303 / 4,991 (86.2 %)** |
| VS | 1,654 | **970 (58.6 %)** | 926 / 951 | 44 / 703 (6.3 %) |
| CS | 685 | **335 (48.9 %)** | 117 / 132 | 218 / 553 (39.4 %) |
| all | 9,128 | **7,402 (81.1 %)** | 2,837 / 2,881 | **4,565 / 6,247** |

- 0 DXC errors: everything that translates compiles (`-O3`, SM 6.6).
- v0 modes unchanged or better: `translate ps/vs/cs` = **1,794 / 926 / 117** (was 1,791 / 926 / 117; +3 from mask
  `s_cselect_b64`).
- Control flow itself refuses only **3 programs** (CS, irreducible). There is no `s_setpc`, `s_call`, other branch
  kind or unsettled dataflow in the agnostic VS/PS/CS set.
- Tests: workspace **335 passed**, 6 ignored, 0 warnings. New in `crates/sir`: the structurizer is run against
  **17,603 random reducible graphs** (14,912 with loops, 5,084 needing `sir_to`). Each runs with 4 branch-decision
  sequences, and the HLSL-level statements must visit the same blocks in the same order as the graph. 2,397
  irreducible graphs are refused. Hand-built programs compile with DXC: a divergent if/else (wave64 and wave32), a
  uniform counted loop, nested loops with a divergent exit, an early `s_endpgm` inside a loop, and a quad DPP. An
  irreducible graph and a mask read as data are refused with their reason.

## Top remaining first refusals (v1)

| stage | reason | programs |
|:--|:--|--:|
| VS | `s_buffer_load_dword/x2/x4` (descriptor not traced: the descriptor is built in code from a 64-bit address plus constant words, not loaded from a table) | 638 |
| PS | `image_atomic_umin` | 239 |
| PS/CS/VS | SDWA forms (`v_and_b32`, `v_cvt_f32_u32`, `v_xor_b32`, `v_add_nc_u32`, …) | 230+ |
| PS | `v_ffbl_b32` | 86 |
| CS/PS | `s_getpc_b64` (in PS/CS: the code's own address, e.g. a constant table in the code) | 80 |
| CS | `s_buffer_load_*` (descriptor not traced) | 41 |
| CS | `buffer_store_format_xyzw` (typed access by byte offset) | 19 |
| PS/CS | `s_andn2_b32` / `s_mov_b32` (lane mask not traced, wave32) | 54 |
| CS | `v_pk_add/mul/max_f16` | 25 |

## For the lead

1. **VS descriptors built in code** (638 VS, the biggest block): `s_mov_b32 s2, -1; s_mov_b32 s3, 0x110124ac;
   s_or_b32 s1, s10, 0x10000; s_mov_b32 s0, s9` then `s_buffer_load` through `s[0:3]`. That is a raw buffer
   descriptor made from a user-data address. It needs the window addressing (decision 5 / Build 1), which is yours.
2. **Signature packing** for two new v0 PS (above): TEXCOORD22.w after TEXCOORD3.xyz in one register.
3. **Predication cost**: in a divergent region, `if (ex) v = …` still issues the work (texture samples included) for
   lanes EXEC has off, because the host branch is uniform. This is exact, and the console does the same (inactive
   lanes cost cycles, not fetches). A later step could branch per lane around side-effect-free work that has no
   derivatives (SampleLevel, loads) when the region holds no quad op.
4. **Loop exits nest**: Ramsey's algorithm places the code after a loop's exit inside the loop when the exit block
   is dominated by the header and has one predecessor (it falls off with `break`). DXC handles it, but the shapes are
   deeper than needed. If the oracle diff's `control` histogram matters, a pass that hoists a single-predecessor
   exit out of the loop is straightforward.
5. `image_gather4_lz` still writes all four registers without the `ex` guard (`res.rs`). That is harmless when EXEC
   is full. I left it alone because changing it changes v0 output.

## Next (if the lane continues)

SDWA, `image_atomic_*`, `v_ffbl/ffbh`, packed f16 (`v_pk_*`) and wave32 mask copies through plain SGPR moves would
take PS past 95 %. The dispatcher fallback for the 3 irreducible CS (and the ES set) is the next control-flow task.
