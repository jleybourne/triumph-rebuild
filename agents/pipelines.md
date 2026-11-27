# Lane `pipelines`: root signatures, the program library, and pipeline creation off the critical path (Build 4/5)

Crate `crates/pipelines` (package `rebuild-pipelines`), binary `pipelines` (`build`, `load`, `check`, `async`).
Task 1 (PR #22, branch `agent/pipelines`): root signatures and the hash-keyed program library, below.
Task 2 (this, branch `agent/pipelines-2`): asynchronous PSO creation, a persistent `ID3D12PipelineLibrary1` cache,
prewarm from the last session, and the translator's sampler sidecars. Nothing vendored; nothing game-derived in git.

## Task 2: async creation, pipeline library cache, prewarm

All numbers on the RTX 4070 Ti, 16 logical processors, over the PR #22 set: **117 compute + 1,374 graphics = 1,491
pipelines** (program library rebuilt in format 2 at `D:\victory-data\pipelines-2\library.bin`). `pipelines async`
requests every pipeline in a burst (as the title creates them), then waits for each in order (as first draws would).
"Cold" salts every root signature with one extra root constant (`b<random>, space900`) so the driver's own disk cache
misses: the cold numbers match PR #22's first-ever run (7.6 / 2.2 ms), so the salt makes a true cold run without
deleting the driver's cache.

```
request             1.6-3.1 us each, returns at once; Key::of 1.1 us a description
cold, 1 worker      compute 7.47 ms · graphics 2.67 ms each · 1,491 in 4,542 ms
cold, 4 workers     1,285 ms (3.5x)            per pipeline per worker: 8.4 / 3.0 ms
cold, 8 workers       768 ms (5.9x)                                     9.9 / 3.6 ms
cold, 12 workers      598 ms (7.6x, default)                           11.6 / 4.2 ms (the driver contends)
driver-warm, 1      compute 0.763 · graphics 0.147 ms each · 301 ms;  12 workers: 44 ms
library-warm, 1     compute 0.725 · graphics 0.155 ms each · 308 ms;  12 workers: 42 ms (1.24-1.37 / 0.24-0.25 each)
second run          1,491 / 1,491 from the library (compute 117 / 117, graphics 1,374 / 1,374), 0 mismatches, 0 created
cache open          1,491 keys, 1.4 MB: read + BLAKE3 check 1.1-1.5 ms, CreatePipelineLibrary 1.1-1.7 ms
cache save          1.5 MB: Serialize 0.8-1.2 ms, hash + write 2.7-3.3 ms (atomic rename)
session record      1,491 pipelines saved in 2.2-3.1 ms
prewarm (cold)      title's first request 1,786 ms after launch (the fork's timing):
                      no prewarm   605 ms blocked in first-draw waits
                      prewarm         0 ms blocked: all 1,491 ready before the first request (12 workers; 4 workers too)
                    first request at 300 ms with prewarm: 282 ms blocked
tests               26 in crates/pipelines (13 new): dedup, async completion, wait-creates-inline, failures,
                    prewarm priority and recording, key and description round trips, session merge/age/damage,
                    sidecars, comparison samplers; on the GPU: library round trip (store -> reload -> hit), a
                    changed description refused by the library and recreated, a driver-refused library rebuilt
                    and reloaded clean, a damaged file never reaching the driver, the pool over real pipelines;
                    workspace 380 passed, 0 failed, 8 ignored, 0 warnings (cargo test --release)
```

### Built

- **`state.rs`**: a pipeline as plain data. `Desc::Compute { cs }` or `Desc::Graphics { vs, ps: Option, state }`;
  programs by hash (from the program library), state in host enumerations (`GraphicsState`: blend x8, raster,
  depth-stencil, input layout with owned names, strip cut, topology, target and depth formats, samples).
  `Key { compute, shaders: [u64; 2], state: u64 }`; `Key::of(&desc)` = program hashes + the first 8 bytes of the
  BLAKE3 of the state's encoding. The device may pass its own key (e.g. a hash of the console description) as long
  as equal keys mean equal descriptions. `Desc::encode/decode` is deterministic (the session record uses it).
- **`pool.rs`**: `Pool<C: Creator>`, worker threads over two queues (the title's first, prewarm when idle).
  `request` dedups by key under one mutex and returns a `Handle` (u32); a slab of 4,096-slot segments that never
  move (1,048,576 pipelines) makes `get` lock-free and lets it return `&ID3D12PipelineState`. `wait` on a pipeline
  no worker has started **creates it on the calling thread**, so a draw never queues behind thousands of others; a
  wait on one being created blocks on a condition variable (Dekker-safe fences, 2 ms timeout as a backstop). A title
  request for a prewarmed key jumps the prewarm queue. Every key the title asked for is kept with its description
  for the session record. `default_threads(limit)`: logical processors (capped by `REBUILD_LIMIT_CPU_CORES`, read
  from the environment because the runtime will depend on this crate), less two, 1..=12.
- **`pso.rs`** (Windows): `Programs` (bytecode and root signatures from the program library, one
  `ID3D12RootSignature` per layout or layout pair, created on first use from the library's serialized blobs; a
  VS-only pipeline gets its root serialized once), `PsoCache` (the `ID3D12PipelineLibrary1` file) and `PsoCreator`
  (the pool's creator: from the cache when its key list has the key, else the driver creates it and it is stored;
  saved every 30 s while storing, and on drop). Counters per kind: from cache, created, mismatches, refused, time.
  ```text
  cache file  "RBPSOCAC" · version · keys · blob length · BLAKE3 of the rest · the driver's library · keys (25 B each)
  ```
  A missing file is an empty cache. A damaged file (length, hash) never reaches the driver. A blob the driver
  refuses (`D3D12_ERROR_DRIVER_VERSION_MISMATCH`, `D3D12_ERROR_ADAPTER_NOT_FOUND`, anything else) is logged in
  `report.refused` / `events` and replaced by an empty library; the next save overwrites the file. Default path
  `%LOCALAPPDATA%\triumph-rebuild\pipelines\pso-cache.bin` (`REBUILD_PSO_CACHE_DIR` overrides the folder).
- **`session.rs`**: `Record` of the pipelines a session used (`session.bin` beside the cache): merged at save, this
  session's at age 0, others one older, dropped after 8 sessions unused, so a menu-only session does not forget the
  world. BLAKE3-checked; a bad file is ignored with a reason.
- **`sidecar.rs`** (PR #22 item 1, inside this crate): `pipelines build` reads `<hash>.<stage>.samplers` beside each
  DXIL (lane shader-cf's format: count, 4 words per sampler, comparison mask) on 8 threads and lifts from the
  archive only the programs without one. **The comparison mask is carried into the root signature**: a sampler
  read by `SampleCmp*` gets a comparison filter (`StageLayout::compare`; the program library format is now version
  2, so rebuild `library.bin` with `pipelines build`).
- `pipelines async` (the bench above) and `build --lift-all` (lift every program and compare with the sidecars).

### Measured (sidecars)

```
D:\victory-data\cf\v0 (2,837 programs from lane shader-cf)
  sidecars             2,837 read in 37-43 ms (8 threads; 213-255 ms on one)
  --lift-all           2,837 / 2,837 agree with the lifter's words; 0 differ
  comparison samplers  0 programs in v0 (the mask is wired regardless)
  build from sidecars  ~115 ms in all (sidecars + 67-83 ms build), was ~0.9 s with the corpus walk
```

### Decided

- **Creation is off the title's threads; the first draw waits only for its own pipeline.** The title's create call
  returns a handle in ~2 us; `wait` creates inline if no worker took it, so the worst case is one creation, never
  a queue.
- **12 workers by default** (logical - 2, capped): 7.6x on cold creation; the driver itself contends past ~8 (per-
  pipeline time rises from 2.7 to 4.2 ms), but wall time still falls.
- **Prewarm the last sessions' pipelines at launch.** The fork's first pipeline came 1.79 s after start; cold, the
  whole v0 set is ready in 0.6 s on 12 workers (1.3 s on 4), so first draws wait 0 ms instead of 605 ms.
- **Keep the pipeline library even though NVIDIA gains nothing from it today.** NVIDIA's serialized library is
  ~1 KB per pipeline (1.5 MB for 1,491; their DXIL alone is 13.7 MB), so it holds references into the driver's own
  disk cache, not code: a library load costs what a driver-warm create costs (0.155 vs 0.147 ms). It costs ~3 ms at
  start and ~4 ms per save. It is what keeps a second run warm where the driver's cache is small or evicted, and on
  AMD (Build 9), whose libraries carry the binaries. Not measured: library load with the driver cache cleared
  (`ShaderCacheControl` needs developer mode, and the owner's 33 GB NVIDIA cache was not deleted).

### For the lead

1. **The API the device calls** (Build 5, device slots 0x50 / 0x58 / 0x1f0 / 0x2b0):
   ```rust
   // at device creation
   let programs = Programs::new(&device, Library::load(&lib_path, 4, true)?.0, None);
   let cache = PsoCache::open(&device, &pso::default_cache_path())?;          // logs a refused file, never fails on it
   let pool = Pool::new(PsoCreator::new(&device, programs, Some(cache)), default_threads(cpu_core_limit()));
   let (record, _) = session::Record::load(&session::default_dir().join("session.bin"));
   for e in &record.entries { pool.prewarm(e.key, e.desc.clone()); }       // background, at launch
   // the title creates a pipeline: convert its console description to a Desc, return at once
   let h: Handle = pool.request(Key::of(&desc), desc);                     // keep h in the title's pipeline object
   // at the first draw that binds it
   let pso: &ID3D12PipelineState = pool.get(h).map_or_else(|| pool.wait(h), Ok)?;
   // at title exit (the title process may be killed: periodic saves every 30 s cover a crash)
   pool.creator().cache().map(|c| c.save());
   record.merge(&pool.requested()).save(&session_path)?;
   ```
   The console-to-`Desc` conversion is the device's (stream subobjects at 0x2b0, console types from 25). A
   pipeline that fails gives `Err(reason)` from `wait` to every caller; the title should see a valid object either
   way (fail at draw, not at create).
2. **Sidecar: one more word, please.** `pipelines build` cannot check that the 16 root constants cover every
   user-data dword a program reads without lifting the corpus (the sidecar does not carry the highest dword read).
   If `translate` appended it (u32, `!0` for none), the build would be complete from sidecars; `sidecar::parse`
   already accepts both lengths.
3. The program library format is now **version 2** (layout records carry the comparison mask): re-run
   `pipelines build` before using a PR #22 `library.bin`; an old file is refused with "not a pipeline library of
   this version", never misread.
4. Earlier items 3 and 4 from task 1 stand (below).

### Raw output (abridged; the full runs printed each line below)

```
> pipelines async --cold --threads 1
pipelines async — 1491 pipelines (117 compute, 1374 graphics) · device luid 00000000:0000a95d · pipeline libraries supported (shader cache flags 0x7f) · 1 workers · cold (root signatures salted: b357600, space900)
requests                 1491 in 2.71 ms (1.81 us each, returned at once) · 0 deduplicated
compute                  0 from the cache (0.000 ms each) · 117 created by the driver (7.468 ms each, per worker) · 0 cache mismatches · 0 refused
graphics                 0 from the cache (0.000 ms each) · 1374 created by the driver (2.671 ms each, per worker) · 0 cache mismatches · 0 refused
title's view             all ready 4541.5 ms after the first request (3.046 ms a pipeline on 1 workers) · first draw waited 28.89 ms · blocked in wait 4538.3 ms over 1481 waits · 5 created inline
> pipelines async --cold --threads 4         all ready 1285.1 ms · compute 8.383 · graphics 3.001 ms each per worker
> pipelines async --cold --threads 8         all ready  767.9 ms · compute 9.925 · graphics 3.576
> pipelines async --cold                     all ready  598.2 ms · compute 11.607 · graphics 4.156 (12 workers)

> pipelines async --fresh                    (first unsalted run this session: driver cache partly warm)
cache open               0 keys · 0.0 MB · read + check 0.1 ms · CreatePipelineLibrary 0.1 ms
compute                  0 from the cache · 117 created by the driver (5.747 ms each, per worker)
graphics                 0 from the cache · 1374 created by the driver (1.576 ms each, per worker)
title's view             all ready 253.7 ms after the first request
cache save               1491 keys · 1.5 MB · Serialize 1.2 ms · hash + write 3.0 ms

> pipelines async                            (second run)
cache open               1491 keys · 1.4 MB · read + check 1.4 ms · CreatePipelineLibrary 1.4 ms
in the cache at start    compute 117 / 117 · graphics 1374 / 1374
compute                  117 from the cache (1.366 ms each) · 0 created by the driver · 0 cache mismatches · 0 refused
graphics                 1374 from the cache (0.239 ms each) · 0 created by the driver · 0 cache mismatches · 0 refused
title's view             all ready 42.1 ms after the first request (0.028 ms a pipeline on 12 workers)
cache save               1491 keys · 1.5 MB · Serialize 0.8 ms · hash + write 3.3 ms
> pipelines async                            (third run) all ready 42.3 ms · 1491 from the cache · open 1.5 + 1.7 ms

> pipelines async --no-cache --threads 1     compute 0.763 · graphics 0.147 ms each · all ready 301.0 ms
> pipelines async --threads 1                compute 0.725 · graphics 0.155 ms each from the cache · all ready 308.4 ms
> pipelines async --no-cache                 all ready 43.9 ms (12 workers, driver-warm)

> pipelines async --cold --title-delay 1786            blocked in wait 605.1 ms over 46 waits
> pipelines async --cold --prewarm --title-delay 1786  prewarm 1491 queued at launch · blocked in wait 0.0 ms over 0 waits
> pipelines async --cold --prewarm --title-delay 300   blocked in wait 281.8 ms over 35 waits
> pipelines async --cold --threads 4 --prewarm --title-delay 1786   blocked in wait 0.0 ms over 0 waits

> pipelines build --lift-all   (REBUILD_PIPELINES_IN=D:\victory-data\cf\v0)
sampler sidecars         2837 read in 257.4 ms · 0 malformed (lifted instead)        (one thread; 8 threads: 37-43 ms)
sidecar vs lifter        2837 agree · 0 differ (the sidecar is used)
comparison samplers      0 programs
programs                 2837 in the library (cs 117 · ps 1794 · vs 926) · 0 refused
```

## Task 1 (PR #22): root signatures and the hash-keyed program library

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

### Built

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

### Decided

- **Root constants are 16 dwords per stage, not 64.** The HLSL declares `uint4 sir_ud[16]` (256 bytes), but the
  runtime and the debug layer accept root constants shorter than the cbuffer (measured: 0 messages). The corpus
  reads at most dword 10 (VS), 14 (PS), 8 (CS). Two stages of 64 would not fit the 64-dword root signature at
  all. `pipelines build` refuses any program that reads a dword past 15 (0 today).
- **The shadow table is a root SRV with `DATA_VOLATILE`**: it is written as descriptors are, like the heap
  itself, so the driver may not assume it is static while set. 2 dwords, no table indirection.
- **Pipelines from the library are created from DXIL at the title's create call, and their cost is the
  driver's**: cold 2-8 ms each, warm (driver cache) 0.15-0.6 ms. See "for the lead" 2.

### For the lead

1. *(Done in task 2: lane shader-cf writes the sidecars, `pipelines build` reads them.)* **Immediate sampler words are not in the translator's output.** `pipelines build` lifts the corpus again
   (0.6-0.9 s) only to get `Lifted::immediate_samplers` and the highest user dword read. If `translate` wrote
   them beside each `.dxil` (e.g. `<hash>.<stage>.samplers`, 16 bytes each, plus the top dword), the library
   build would need no corpus walk and would be ~70 ms in all.
2. *(Built in task 2, above.)* **Pipeline creation cost for Build 5.** Cold PSO creation measured 7.6 ms (compute) and 2.2 ms (graphics)
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

- Done in task 2: the pipeline library cache and parallel creation, measured on the same 1,491 pipelines.
- Re-run `build`, `check` and `async` as the translator grows toward 25,694 (v1 from lane shader-cf: 7,402
  programs with sidecars in `D:\victory-data\cf\v1`).
- With the device (Build 5): measure the title's real pipeline stream (dedup rate, how many of the fork's ~60,000
  creations are distinct, prewarm hit rate on a second session).
