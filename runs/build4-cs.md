# Build 4 — compute programs, compiled by DXC

`REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- cs`
(raw: [`build4-cs.txt`](build4-cs.txt); HLSL and DXIL under `D:\victory-data\translate`, outside git).

**Built** (`crates/sir/src/lift.rs`, `lift/io.rs`, `lift/res.rs`): the v0 compute programs.

- `numthreads` and the LDS size from the dispatch settings (`COMPUTE_PGM_RSRC2`); thread ids in v0-v2 as
  `SV_GroupThreadID`, group ids in the system SGPRs after the user data as `SV_GroupID`.
- LDS as `groupshared uint sir_lds[]`: `ds_read`/`ds_write` of 1-4 dwords, `read2`/`write2` (and `st64`),
  `ds_add_u32`; `s_barrier` as `GroupMemoryBarrierWithGroupSync`.
- Untyped buffer stores (`RWByteAddressBuffer`, indexed ones bounded by the shadow table's record count).
- Typed buffer loads and stores (`buffer_*_format_*`, indexed, no byte offset) through `Buffer<float4>` /
  `RWBuffer<float4>`: the runtime builds the host's typed view from the console descriptor, which carries the
  format. Stores merge only the channels the instruction names.
- `a16` image stores, `v_rcp_iflag_f32`.

**Measured (132 programs):** first run 99 (75.0 %); typed buffers, a16 stores, `v_rcp_iflag` **117 (88.6 %)**.
Left (15): typed access by byte offset (9), single forms.

**Not yet proven correct:** a program counts toward the 25,694 only when the execution check matches the oracle.
