# Lane `pipelines`: root signatures and the hash-keyed pipeline library (Build 4)

Branch `agent/pipelines`, crate `crates/pipelines` (package `rebuild-pipelines`), binary `pipelines`
(`build`, `load`, `check`). Inputs: the lead's translator output, produced from this worktree with
`translate ps|vs|cs` into `D:\victory-data\pipelines-in` (2,834 DXIL: PS 1,791, VS 926, CS 117). Library at
`D:\victory-data\pipelines\library.bin` (outside git). Nothing vendored. Raw output: [`pipelines-run.txt`](pipelines-run.txt).

```
root signatures      every program's own root signature serialized by the host: 37 distinct (stage, layout), 0 refused
                     on the device: 82 / 82 accepted (10 compute + 72 vertex x pixel pairs), 0 refused, 3-4 us each
                     debug layer on: 0 errors, 0 warnings for root signatures and compute pipelines
library build        62-106 ms for 2,834 programs (read DXIL + bindings 39-65 ms, roots 2-6 ms, write 16-27 ms); 13.8 MB
                     (+ 0.6-0.9 s to lift the corpus again for the sampler words: see "for the lead" 1)
library load         3.5-5.5 ms unverified, 4.2-6.3 ms with the content hash checked (8 threads; 1 thread
                     4.4-5.7 / 7.4-9.0 ms); warm file cache. Lookup hash -> blobs 36-46 ns (binary search in place, no index built)
compute pipelines    117 / 117 created, 0 refused. Cold (driver cache empty) 7.6 ms each; warm 0.63 ms
graphics pipelines   1,374 / 1,374 created (default state), 0 refused; 417 pixel shaders had no linking vertex
                     shader in the v0 set (not tried). Cold 2.2 ms each; warm 0.15 ms
tests                13 in crates/pipelines (root construction, host serializer round trip, store/load, corruption);
                     workspace 334 passed, 6 ignored, 0 warnings
```

## Built

- `bindings.rs`: what a compiled program binds, read from its `PSV0` part (the table the D3D12 runtime itself
  checks a root signature against), plus `ISG1`/`OSG1`.
- `layout.rs`: `StageLayout` (user data yes/no, shadow table yes/no, immediate sampler words) from the PSV0
  bindings; any binding outside the layout is refused by name. `RootDesc::compute` / `RootDesc::graphics`. **Every
  root signature has the same parameters**, so per-draw code never branches and indices are constants
  (`layout::param`):

  ```text
  compute    0  32-bit constants b0 space0 (16)   ALL        1  root SRV t0 space101 (shadow table)  ALL
  graphics   0  32-bit constants b0 space0 (16)   VERTEX     1  32-bit constants b0 space0 (16)       PIXEL
             2  root SRV t0 space101               ALL
  flags      CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED always; graphics also input
             layout + deny HS/DS/GS/AS/MS root access
  samplers   immediate samplers -> static samplers s{k} space100 (rebuild_sir::sampler::decode), stage-visible
  ```
  34 of 64 dwords for graphics, 18 for compute. Only the static samplers differ between root signatures: 26
  distinct layouts in all.
- `library.rs`: one file. Programs sorted by (hash, stage), 32-byte records; DXIL blobs 16-byte aligned; one
  root signature per distinct compute layout and **one per (vertex layout, pixel layout) pair the library's
  shaders use** (72), so any VS + PS pair the title makes at run time has its root signature ready, with no
  serialization at pipeline creation. Content hash: BLAKE3 over per-1 MiB-chunk BLAKE3s, so each load thread
  verifies what it read. Written to a temporary file and renamed. Tables are bounds-checked once at load; after
  that, lookups cannot panic.
- API for Build 5: `Library::load(path, threads, verify)`, `get(hash, stage) -> Entry { dxil, layout, root }`
  (compute entries carry their root), `graphics_root(&vs, &ps)`, `roots()`, `layout(i)`.
- `d3d.rs`: host serializer (version 1.1), a deserializer read-back for tests, and the acceptance check device
  (`Gpu`: root signatures, compute pipelines, graphics pipelines with a default state and debug-layer reasons).

## Decided

- **Root constants are 16 dwords per stage, not 64.** The HLSL declares `uint4 sir_ud[16]` (256 bytes), but the
  runtime and the debug layer accept root constants shorter than the cbuffer (measured: 0 messages). The corpus
  reads at most dword 10 (VS), 14 (PS), 8 (CS). Two stages of 64 would not fit the 64-dword root signature at
  all. `pipelines build` refuses any program that reads a dword past 15 (0 today).
- **The shadow table is a root SRV with `DATA_VOLATILE`**: it is written as descriptors are, like the heap
  itself, so the driver may not assume it is static while set. 2 dwords, no table indirection.
- **Pipelines from the library are created from DXIL at the title's create call, and their cost is the
  driver's**: cold 2-8 ms each, warm (driver cache) 0.15-0.6 ms. See "for the lead" 2.

## For the lead

1. **Immediate sampler words are not in the translator's output.** `pipelines build` lifts the corpus again
   (0.6-0.9 s) only to get `Lifted::immediate_samplers` and the highest user dword read. If `translate` wrote
   them beside each `.dxil` (e.g. `<hash>.<stage>.samplers`, 16 bytes each, plus the top dword), the library
   build would need no corpus walk and would be ~70 ms in all.
2. **Pipeline creation cost for Build 5.** Cold PSO creation measured 7.6 ms (compute) and 2.2 ms (graphics)
   each on the RTX 4070 Ti; warm 0.63 / 0.15 ms. At the fork's ~60,000 creations in the first 20 s that is
   ~130 s cold single-threaded, 9-38 s warm. Suggest: create pipelines on worker threads (the device is
   free-threaded) and keep the title's create call non-blocking (return the object, finish creation before its
   first use); and an `ID3D12PipelineLibrary` keyed by (VS, PS, state hash) beside this library so the second
   run is warm whatever the driver cache does. This lane can build that next.
3. `SirUserData` could be declared `uint4 sir_ud[8]` (32 dwords, the GFX10 user SGPR limit) instead of 16: the
   runtime accepts the shorter root constants either way, so this is tidiness, not a fix.
4. 417 of 1,791 pixel shaders have no vertex shader in the v0 VS set whose outputs link (by name, index,
   register, type and mask). They are not refusals; they need the VS (or ES/GS) programs not yet translated.

## Next

- `ID3D12PipelineLibrary` (item 2) and parallel creation, measured on the same 1,491 pipelines.
- Re-run `build` and `check` as the translator grows toward 25,694; the library format does not change.
