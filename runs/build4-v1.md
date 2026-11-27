# Build 4 — v1: structurized control flow (lane shader-cf, PR #26)

`REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- v1` (outputs outside git).

| stage | compiled | one block (v0) | several blocks |
|:--|--:|--:|--:|
| PS | 6,097 / 6,789 (89.8 %) | 1,794 / 1,798 | 4,303 / 4,991 |
| VS | 970 / 1,654 | 926 / 951 | 44 / 703 |
| CS | 335 / 685 | 117 / 132 | 218 / 553 |
| **all agnostic** | **7,402 / 9,128 (81.1 %)** | 2,837 / 2,881 | 4,565 / 6,247 |

- `crates/sir/src/lift/cfg.rs`: the CFG structurized into HLSL loops, if/else, break/continue (Ramsey, "Beyond
  Relooper"; a jump variable only where a branch crosses more than one construct). EXEC stays data: every console
  branch becomes a host branch on a wave-uniform value (SCC, or any lane of VCC/EXEC via `WaveActiveAnyTrue`);
  divergence is predication by `ex`. Lifter knowledge (descriptors, constants, mask SGPRs) is tracked per path,
  merged at joins, iterated around loops to a fixed point.
- Checked by execution on 17,603 random reducible graphs (14,912 with loops): the HLSL visits exactly the graph's
  block sequence. Only 3 programs refused for control flow (irreducible compute).
- Fixed on the way: fp16 interpolation components (oracle `InputReads` 62 → 0); a pixel shader with no outputs is
  `void main` (v0 interface diffs against the oracle: **0**); `image_gather4_lz` writes only active lanes (lead).
- Left, by first refusal: VS `s_buffer_load` from a descriptor built in code from a 64-bit address (638, needs
  window addressing: lead), `image_atomic_umin` (239), SDWA forms (230+), `v_ffbl_b32` (86), `s_getpc_b64` in PS/CS
  (80), packed f16 `v_pk_*` (25), typed buffers by byte offset (19), wave32 masks through plain SGPRs (54).
