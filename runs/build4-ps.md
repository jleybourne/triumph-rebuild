# Build 4 — pixel shaders with inputs and outputs, compiled by DXC

`REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- ps`
(raw: [`build4-ps.txt`](build4-ps.txt); the generated HLSL and DXIL go to `D:\victory-data\translate`, outside git).

**Built** (`crates/sir/src/lift/io.rs`, `crates/shader/src/signature.rs`):
- The pixel input VGPR layout from `SPI_PS_INPUT_ADDR`: barycentric pairs per mode, position, front face.
- `v_interp_p1/p2_f32` and `v_interp_mov_f32` read the interpolated input, with the HLSL modifier of the
  barycentric pair they use (centroid, sample, `noperspective`, `nointerpolation`).
- `exp mrt0-7/mrtz/null` from the export-patched code. Lanes inactive at the final export are discarded.
- Inputs and outputs named by the container's own `ISG1`/`OSG1` signature, per element and channel (rows can
  hold several elements), so pipelines link by semantic as the title's did.
- `s_wqm_b64 exec, exec` is a no-op while EXEC is untouched (the host's helper lanes already run for
  derivatives), and a quad-any once EXEC has narrowed.
- A program that reads user data (an SGPR read before any write: root constants, descriptor addresses) is
  refused, never run with zeros. None of the compiled ones reads any.

**Update, same day: resources.** User data arrives as root constants at `b0`. Descriptor loads from tables
and from SM 6.6 heap parameters become heap indices (`ResourceDescriptorHeap[]`/`SamplerDescriptorHeap[]`),
traced through the scalar arithmetic that computes them, register offsets included. `s_buffer_load` reads
constant buffers or raw buffers, and `image_sample/_lz/_l/_b`, `image_load`, `image_store` and
`image_gather4_lz` (1D, 2D, 3D, arrays) go through the heap. **1,360 of 1,798 v0 pixel shaders (75.6 %) now
translate and compile.** The rest are cube maps, 16-bit interpolation, NSA and 16-bit image forms,
`s_brev_b32`, `ds_swizzle` and a few others (`build4-ps.txt`).

**First measurement, before resources (1,798 programs):**

| result | programs |
|:--|--:|
| translated and compiled by DXC (`ps_6_6`, `-O3`) | **427 (23.7 %)** |
| blocked by `s_load_dwordx8` (descriptor loads) | 785 |
| blocked by `s_load_dwordx4` | 579 |
| 16-bit interpolation (`v_interp_p1ll_f16`) | 6 |
| `v_cmp_class_f32` | 1 |

**Update, same day: NSA and unorm.** NSA image addressing (address registers named one by one) and `unorm` on
loads, stores and atomics (it only changes filtering) are translated, and refusals name the form.
**1,468 of 1,798 (81.6 %) now translate and compile.** The largest block left is immediate samplers (158
programs): the sampler's descriptor is baked into the code as constants, and the root layout lists it as
`ImmediateSampler`.

**Update, same day: immediate samplers and the primitive mask.** Samplers built from constants decode to static
samplers (`crates/sir/src/sampler.rs`); constants fold through `s_mov`, `s_movk`, `s_brev` and bitwise/shift/add
ops; the primitive mask SGPR reads 0 (the host interpolates); `s_brev_b32` and `v_cmp_class_f32` translate.
**1,684 of 1,798 (93.7 %) now translate and compile.** Left: `v_interp_p1ll_f16` (62), untraced samplers (14),
16-bit image addresses (11), `ds_swizzle_b32` (9), cube maps (6), and single programs.

**Update, same day: 16-bit interpolation and mixed precision.** VOP3 fp16 interpolation and the VOP3P
`v_fma_mix*` forms translate; a channel a packed fp16 element lacks reads 0. **1,748 of 1,798 (97.2 %).**

**Not yet proven correct:** these compile, but a program counts toward the 25,694 only when the execution
check matches the oracle's output (tasks.md, Build 4). Next: resource tracing (descriptor loads from root
parameters, decision 5), which unblocks 1,364 of the remaining 1,371.
