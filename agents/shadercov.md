# Lane shadercov — Build 4 coverage of the agnostic class

Branch `agent/shadercov`. Files touched: `crates/sir/src/lift.rs`, `crates/sir/src/lift/{res,io,fmt,check,tests,tests_cov}.rs`
(`fmt.rs`, `check.rs`, `tests_cov.rs` new), `crates/pipelines/src/translate.rs` (passes the code bytes),
`crates/uma/src/tiles.rs` (`Tiles::new_sized`, additive), `apps/rebuild/src/bin/{sirtest,translate}.rs`,
`apps/rebuild/Cargo.toml` (`Win32_Graphics_Dxgi`), `runs/build4-v1.md`, `progress.md` (one row).

## Measured

| stage | before | after |
|:--|--:|--:|
| PS | 6,097 / 6,789 | 6,733 / 6,789 |
| VS | 970 / 1,654 | 1,448 / 1,654 |
| CS | 335 / 685 | 618 / 685 |
| **agnostic** | **7,402 / 9,128** | **8,799 / 9,128 (96.4 %)** |

- `translate v1`: 717 s, 0 DXC errors. Outputs in `D:\victory-data\shadercov\v1` (refusals per program in
  `refused.<stage>.txt`).
- `pipelines build` from them into `D:\victory-data\shadercov\pipelines\library.bin`: 8,799 programs, 0 refused,
  120 own root signatures (0 refused), 544 library root signatures, 116.3 MB, 700 ms.
- `sirtest` (card, under `agents\CARD`): 300 random programs + 66 lifted checks exact. `sirtest --warp`: 64 lifted
  checks exact; the wave32 random programs need 32-lane waves (WARP has 4) and the 2 checks that round to f16 are
  skipped (WARP rounds f32 → f16 toward zero).
- `cargo test --release --no-fail-fast`: exit 0, 0 warnings.

## Built

- SDWA; image atomics (32/64-bit); bit scans, `v_perm_b32`; 16-bit VALU (results keep the other half, op_sel) and
  `v_pk_*`; carries, 64-bit compares and shifts; buffer, LDS and global memory atomics/loads/stores; scalar bit ops;
  VCC as data; d16 sampling; gather offsets; `image_load_mip`; 8/16-bit LDS.
- Window addressing of V#s held as data (`WINDOW_HEAP_BASE + (address >> 32)`, GFX10 `OOB_SELECT`), typed fetches
  with the format in the instruction (`fmt.rs`, all GFX10 buffer formats), descriptor stride/records from the
  shadow table.
- The root-seam fix: CBV/UAV/typed views at the shadow entry's alternate, `lead` on raw accesses.
- `s_getpc_b64` data embedded in the code (`lift_program_code`); the wave32 mask fix (+84); VS 16-bit outputs
  (the oracle's packed layout); flat 16-bit pixel inputs.
- `translate`: `REBUILD_TRANSLATE_LIFT_ONLY`, `REBUILD_TRANSLATE_HASHES`, `refused.<stage>.txt`.

## Left (329)

Descriptor word 3 read as data (229; needs the runtime), loop-variant samplers (29), M0-relative VGPR indexing
(16), VS position export 2 (11), wave-wide mask reads (8), other descriptor words (13), and 23 small ones
(`runs/build4-v1.md`).

## For the lead

1. **Window views in the heap.** Window addressing reads `ResourceDescriptorHeap[WINDOW_HEAP_BASE + (address >>
   32)]` (`crates/sir/src/lift.rs`, `WINDOW_HEAP_BASE` = 1,000,000 − 32,768, `WINDOW_COUNT` = 32,768). d3d12x does
   not place raw UAVs of the 4 GiB windows there yet: until it does, the ~640 VS programs that read through a
   code-built V# (and all global memory access) read whatever that range holds. Either reserve that range in
   `crates/d3d12x/src/heap.rs` (null views where no window is mapped), or tell me another base.
2. **Descriptor word 3 in the shadow table.** 229 programs extract a buffer descriptor's format (word 3, bits
   12-15) with `s_bfe_u32`. The shadow entry is (stride, records, alternate, lead); with word 3 somewhere (a fifth
   dword, or `lead` packed into fewer bits), the lifter can serve it as it serves stride and records.
3. **`f32tof16` is device-dependent.** On WARP it rounds toward zero and clamps overflow to 65504; the card rounds
   to nearest even. `sir_f16_rtz` (`crates/sir/src/lift.rs`, PRELUDE) assumes nearest rounding, and
   `v_cvt_f16_f32`, `v_fma_mix*` and the interp16 path use `f32tof16` directly. My new 16-bit ops use `sir_f16`
   (a native `float16_t` conversion, round to nearest even).
4. **16-bit results zero the high half in the existing ops.** `v_cvt_f16_f32` (lift.rs, `f32tof16({x})` as the
   whole dword) zeroes bits 16-31; GFX10 keeps them (the oracle: `lower.rs` `low_half`, "16-bit results keep the
   register's high half (GFX10)"). My new 16-bit ops keep them. That belongs to the execution-check lane.
5. **Translate output used by `pipelines` tests.** `translate::tests::a_container_translates_to_what_translate_v1_writes`
   compares against `REBUILD_TRANSLATE_OUT` (default `D:\victory-data\translate`). This branch changes the DXIL
   (alternate views), so that folder needs a fresh `translate v1` after merging, or the test fails. I ran the gate
   with `REBUILD_TRANSLATE_OUT=D:\victory-data\shadercov\v1`.
6. **A stale card lock.** `agents\CARD` held `rebuild-pipelines pid 26820` for about an hour after that process
   had exited (2026-11-28 00:01 to about 00:33). The tool that writes it may not remove the lock on every exit path.
