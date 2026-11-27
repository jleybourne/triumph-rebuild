# Build 4 — the lifter, first step: scalar and vector ALU

`REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin translate -- lift`
(raw: [`build4-lift.txt`](build4-lift.txt)). Translator: `crates/sir/src/lift.rs`, design in
`docs/design/shader-ir.md` "Translation from GFX10".

**Built:** registers as HLSL variables (DXC builds the SSA), lane masks and EXEC as per-lane booleans with
predication once EXEC narrows, the console's float semantics where HLSL differs (legacy multiply, round-toward-zero
half packing, saturating float→int, sin/cos in revolutions, VOP3 abs/neg/clamp/omod), about 150 SALU, VALU and
compare forms. Five unit tests, one of which compiles every translated form with DXC (SM 6.6, wave32,
16-bit types, `-O3`).

**Measured on the v0 set (2,881 programs):**
- 5 programs (all CS) translate completely. Everything still missing is memory, resources, interpolation and
  exports: **79 opcodes, none of them ALU**.
- **461 programs miss only `exp`** (the export), so exports come next.
- The next largest blockers: scalar loads (`s_load_dwordx4/x8`, `s_buffer_load_*`), interpolation
  (`v_interp_p1/p2_f32`, 1,376 each), `s_wqm_b64` (884), `image_sample` (876) and the vertex fetch call
  `s_swappc_b64` (765). These all depend on the interface (decision 5: the fork's root bindings and IO).

**Approximations named, not hidden:** `s_setreg` float modes are not modelled. `v_fma_f32` becomes `mad`,
which DXIL lowers to a fused multiply-add, because HLSL's `fma` is double-only and doubles are slow on
consumer GPUs. Correctness is proven only when the execution check against the oracle runs.
