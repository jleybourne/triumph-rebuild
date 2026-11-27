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

**Measured on the v0 pixel shaders (1,798):**

| result | programs |
|:--|--:|
| translated and compiled by DXC (`ps_6_6`, `-O3`) | **427 (23.7 %)** |
| blocked by `s_load_dwordx8` (descriptor loads) | 785 |
| blocked by `s_load_dwordx4` | 579 |
| 16-bit interpolation (`v_interp_p1ll_f16`) | 6 |
| `v_cmp_class_f32` | 1 |

**Not yet proven correct:** these compile, but a program counts toward the 25,694 only when the execution
check matches the oracle's output (tasks.md, Build 4). Next: resource tracing (descriptor loads from root
parameters, decision 5), which unblocks 1,364 of the remaining 1,371.
