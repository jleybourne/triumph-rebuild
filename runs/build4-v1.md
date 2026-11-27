# Build 4 — v1: structurized control flow (lane shader-cf, PR #26), coverage (lane shadercov)

`REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- v1` (outputs outside git; 717 s,
at most 4 DXC processes at once).

| stage | compiled | one block (v0) | several blocks | before shadercov |
|:--|--:|--:|--:|--:|
| PS | 6,733 / 6,789 (99.2 %) | 1,795 / 1,798 | 4,938 / 4,991 | 6,097 |
| VS | 1,448 / 1,654 (87.5 %) | 946 / 951 | 502 / 703 | 970 |
| CS | 618 / 685 (90.2 %) | 130 / 132 | 488 / 553 | 335 |
| **all agnostic** | **8,799 / 9,128 (96.4 %)** | 2,871 / 2,881 | 5,928 / 6,247 | 7,402 |

0 DXC errors. Library from these outputs (`pipelines build`): 8,799 programs, 0 refused, 120 own root signatures
(0 refused), 544 library root signatures, 116.3 MB, built in 700 ms.

- `crates/sir/src/lift/cfg.rs`: the CFG structurized into HLSL loops, if/else, break/continue (Ramsey, "Beyond
  Relooper"; a jump variable only where a branch crosses more than one construct). EXEC stays data: every console
  branch becomes a host branch on a wave-uniform value (SCC, or any lane of VCC/EXEC via `WaveActiveAnyTrue`);
  divergence is predication by `ex`. Lifter knowledge (descriptors, constants, mask SGPRs) is tracked per path,
  merged at joins, iterated around loops to a fixed point.
- Checked by execution on 17,603 random reducible graphs (14,912 with loops): the HLSL visits exactly the graph's
  block sequence. Only 3 programs refused for control flow (irreducible compute).
- Fixed on the way: fp16 interpolation components (oracle `InputReads` 62 → 0); a pixel shader with no outputs is
  `void main` (v0 interface diffs against the oracle: **0**); `image_gather4_lz` writes only active lanes (lead).
- Left after PR #26, by first refusal: VS `s_buffer_load` from a descriptor built in code from a 64-bit address
  (638), `image_atomic_umin` (239), SDWA forms (230+), `v_ffbl_b32` (86), `s_getpc_b64` in PS/CS (80), packed f16
  `v_pk_*` (25), typed buffers by byte offset (19), wave32 masks through plain SGPRs (54).

## Coverage (lane shadercov): 7,402 → 8,799

| step | compiled (lifted, then DXC) |
|:--|--:|
| baseline (PR #26) | 7,402 |
| SDWA, image atomics (32/64-bit), bit scans, `v_perm_b32`, window addressing of code-built V#s, `tbuffer_load_format_*`, stride/records from the shadow table, typed element 0 | 8,161 |
| 16-bit and packed ops, carries, 64-bit compares and shifts, buffer and LDS atomics, scalar bit ops, VCC as data | 8,491 |
| `s_getpc_b64` data in code, wave32 mask fix, flat 16-bit inputs, global memory, 8/16-bit LDS, d16 sampling, gather offsets, `image_load_mip`, VS 16-bit outputs | 8,799 |

- Window addressing (`docs/design/shader-ir.md`, "Memory"): a V# held as data (built from a 64-bit address, or
  loaded from memory) reads `ResourceDescriptorHeap[WINDOW_HEAP_BASE + (address >> 32)]` at the 32-bit offset,
  with GFX10's `OOB_SELECT` range checks (folded when word 3 is a constant). `WINDOW_HEAP_BASE` = 1,000,000 −
  32,768: the runtime does not place window views there yet.
- Buffer views follow d3d12x's heap (lead, root-seam items 1-2): CBV, UAV and typed views at the shadow entry's
  alternate; raw SRV and raw UAV accesses add the entry's lead.
- Checked by execution (`sirtest`, 64 lanes each, against a console reference): bit scans, `v_perm`, SDWA selects
  and merges, all 61 one-dword buffer formats, 16-bit ops with op_sel, packed f16/u16/i16 (fma included), carry
  chains, 64-bit shift and compare, `v_cvt_pk_u8_f32`, `v_mul_hi_i32`: exact on the card (with the 300 random
  programs); on WARP (`sirtest --warp`) 64 exact, the 2 that round to f16 skipped (WARP rounds toward zero).
- Left (329), by first refusal:
  - descriptor word 3 read as data (229: VS 173, CS 41, PS 15): the code extracts the buffer format (bits 12-15)
    of a heap descriptor; the shadow table does not carry word 3;
  - a sampler whose words differ between loop iterations at the sample (29);
  - M0-relative VGPR indexing, `v_movrels_b32` / `v_movreld_b32` (16);
  - VS export target 13, the second position export (11);
  - lane masks read as data (8: wave-wide, not agnostic), descriptor words 4 or unknown read as data (13);
  - small: `v_med3_f16` (6), irreducible control flow (3), `image_sample_o` (2), `image_gather4_l` (2), d16
    `image_load` (2), `ds_write_b16_d16_hi` (2), `global_load_ushort` (2), 16-bit integer VALU (2),
    `s_setpc_b64` (1), `v_interp_p2_f32` of a packed input (1).
