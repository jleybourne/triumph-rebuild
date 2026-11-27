# Build 4 — vertex shaders with inputs and outputs, compiled by DXC

`REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- vs`
(raw: [`build4-vs.txt`](build4-vs.txt); HLSL and DXIL under `D:\victory-data\translate`, outside git).

**Built** (`crates/sir/src/lift/io.rs`, `lift/res.rs`): the v0 vertex shaders that run as the hardware VS.

- The call into the console's fetch subroutine (`s_swappc_b64`) becomes the attribute loads it would do: channel
  k of each attribute into its VGPR + k, from the host's input assembler by the input signature. `s_getpc_b64`
  (the subroutine's address) and the return address are dead values.
- v0 is `SV_VertexID`, v3 `SV_InstanceID` (the fork's `io.rs`: the hardware VS gets the vertex id in v0, the
  primitive id in v2, the instance id in v3).
- `exp pos0` is `SV_Position`; `exp param<n>` is the output row the program's output table gives parameter n,
  typed by the output signature. A program with no output rows (depth only) drops its parameter exports.
- Untyped buffer loads (`buffer_load_dword`, `x2`-`x4`) through heap descriptors as `ByteAddressBuffer` loads.
  The indexed form reads the stride and record count from the descriptor shadow table (plan decision 9).

**Measured (951 programs):**

| step | compiled |
|:--|--:|
| first run (fetch, exports) | 595 (62.6 %) |
| buffer loads, offset form | 613 (64.5 %) |
| indexed buffer loads (shadow table) | 860 (90.4 %) |
| depth-only parameter exports | 911 (95.8 %) |
| 16-bit integer image addresses | 923 (97.1 %) |
| later re-run, the raw output beside this write-up ([build4-vs.txt](build4-vs.txt)) | **926 (97.4 %)** |

Left (28): `v_add_nc_u32` / `v_lshlrev_b32` forms (8), typed buffer loads
(4), `exp pos1` (3), `s_buffer_load` forms (6), single programs.

**Not yet proven correct:** a program counts toward the 25,694 only when the execution check matches the oracle.
