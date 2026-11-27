# Progress

```
overall   ████████████░░░░░░░░  61 %   work toward 30 fps, weighted by build step
fps       frames 1-2 presented (black); runs paused after a GPU hang · target 30 · baseline 7.6
updated   2026-11-27
```

The percentage is **work done**, not fps. Once Build 5 shows a frame, the measured fps goes on the second line
and becomes the number that counts.

## At a glance

| track | progress | | note |
|---|---|--:|---|
| **Build 5 · first frame** | `███████████░░░░░░░░░` | 55 % | frames 1-2 presented; runs paused: a GPU hang once lists executed |
| shaders compiled (every class) | `███████░░░░░░░░░░░░░` | 9,415 / 25,694 | pixel 7,840 · vertex 987 · compute 588 · of them wave-wide 1,892 ([wave64](runs/build4-wave64.md)) |
| shaders counted | `████░░░░░░░░░░░░░░░░` | 5,195 / 25,694 | bit-exact vs the oracle on WARP ([exec](runs/build4-exec.md)) |
| runtime functions ours | `███████████████████░` | 69 / 73 | 624 more bind to the host directly |
| launcher | `██████████████░░░░░░` | 5 / 7 | C2 and L5 left · not part of the 59 % |

| | fps | frame time | draws |
|---|--:|--:|--:|
| baseline (the layer being replaced) | 7.6 | 131.3 ms | ~4,500 (356 dropped) |
| target | 30 | 33.3 ms | same |

## By build step

| # | build | progress | done | weight | state |
|--:|---|---|--:|--:|---|
| 0 | three answers before more code | `██████████████░░░░░░` | 70 % | 10 | (a) upload and (b) translation done; (c) needs a frame |
| 1 | tiled VRAM at console addresses | `████████████████████` | 100 % | 10 | closed |
| 2 | CPU-written memory ring; GPU→CPU | `████████████████████` | 100 % | 8 | closed |
| 3 | PE loader + console runtime | `████████████████████` | 100 % | 15 | closed: the title's code runs |
| 4 | shader recompiler RDNA2 → DXIL | `███████████████░░░░░` | 75 % | 15 | 9,415 compile, 5,195 bit-exact; wave64 built |
| 5 | device, lists, descriptors, present | `███████████░░░░░░░░░` | 55 % | 12 | frames 1-2 presented; GPU safe mode next |
| 6 | list translation | `██░░░░░░░░░░░░░░░░░░` | 10 % | 8 | translator core measured |
| 7 | upscaler: DLSS + FSR | `██████░░░░░░░░░░░░░░` | 30 % | 10 | both standalone; wiring to the title open |
| 8 | frame pacing | `░░░░░░░░░░░░░░░░░░░░` | 0 % | 6 | 30 fps on NVIDIA closes here |
| 9 | AMD validation by testers | `░░░░░░░░░░░░░░░░░░░░` | 0 % | 6 | 30 fps on AMD closes here |
| | **total** | `████████████░░░░░░░░` | **61 %** | 100 | 61.65 points earned |

## Build 5 · first frame, piece by piece

| piece | progress | state |
|---|---|---|
| device, queues, fences, allocators, lists, resources, DXGI | `████████████████████` | done |
| root signatures, query heaps, command signatures | `████████████████████` | done |
| compute pipelines (library + pool) | `████████████████████` | done; programs outside the library translated on demand and cached (PR #31) |
| graphics and stream pipelines (0x50, 0x2b0) | `████████████████████` | done (PR #30): 14 created, 0 refused |
| title start-up to its first submission | `████████████████████` | done: ExecuteCommandListsX and PresentX called at 1.44 s |
| ExecuteCommandLists + PresentX | `████████████████████` | done (PR #32): submit, borderless window, flip swap chain, PNG capture |
| a non-black frame, captured | `██████████░░░░░░░░░░` | frame 1 captured (black, 1920×1080); frame 2 crashes: the title draws with no program bound (`exe+0xb5a5f8`) |
| frame line (fps, ms, draws, …) | `██████████████████░░` | written each second + summary.toml (PR #32); upload/xlate/upscale fields to come |
| Build 0 (c): the title's GPU time | `░░░░░░░░░░░░░░░░░░░░` | needs a frame |

## Helper lanes running

| lane | build | working on |
|---|---|---|
| `shaderexec` | 4 | our shaders against the oracle's, on the GPU: the "shaders counted" bar |
| `shadercov` | 4 | the last 1,726 agnostic programs (7,402 → 9,128) |
| `wave64` | 4 | wave-wide programs, the other ~16,500 |
| `ffx` | 7 | the title's FidelityFX upscaler calls, onto our DLSS / FSR |
| waiting | 6, 8 | list translation cost and pacing need real frames |

The weights reflect size and risk: the loader, the runtime and the shader recompiler are the largest pieces
of work. A step counts as 100 % only when its closing number from `tasks.md` is measured.

## Done

- Address space: 4 GiB windows, 64 KiB pages, placement, sharing by address, lifetime by user count, card-page reuse.
- GPU writes into CPU-mapped memory, and the CPU reads the same bytes back (`gputest`), plus the control (`gputest --bare`).
- **Build 0 (a) — upload benchmark** (`uploadbench`, [`runs/build0a-upload.md`](runs/build0a-upload.md)):
  12.64 GB/s ceiling over PCIe 3.0. At the title's real shape (~226k ranges, ~31-48 MB) the upload costs
  ~16-30 ms of worker wall time, ~1-7 ms of it on the GPU. It must use hash diff + compute scatter and
  run a frame ahead, off the submit thread. Every config verified byte-exact.
- 64-byte chunk diff, gather and chunk index (`crates/uma/src/chunks.rs`, 7 tests).
- **Build 0 (b) — translation benchmark** (`listbench`, [`runs/build0b-lists.md`](runs/build0b-lists.md)):
  our translator plus D3D12 costs 0.43 µs a draw, 1.9 ms for the title-shaped frame on one thread, which is 18×
  under budget. 12,027 descriptor writes from a dirty list cost 2.0 ms. List translation is not the CPU problem.

- **Build 1 — tiled VRAM** ([`runs/build1-tiles.md`](runs/build1-tiles.md)): 4 GiB reserved windows with 64 KiB tiles
  on 64 MiB heaps. Shaders read and write at console addresses. Two buffers, or a buffer and a render target,
  at one address see each other's writes. Only stencil formats need the arena. 10 GiB committed at random
  addresses with 0 wrong. The upload has no measurable tile penalty.

- **Build 2 — the upload ring** ([`runs/build2-ring.md`](runs/build2-ring.md)): the submit thread calls `flush()` and
  queues a GPU wait in 32 µs. The coordinator and 4 workers send 34 MB in 342k runs in 19.4 ms. Both regions
  verified byte for byte after 60 frames.
- **Build 2 — labels** ([`runs/build2-labels.md`](runs/build2-labels.md)): title memory (write-watched too) opens as a
  GPU heap. The GPU writes the title's labels into it directly, 65 µs before the fence reports; 200/200 in order.

- **Shader core, first piece** ([`docs/design/shader-ir.md`](docs/design/shader-ir.md), `crates/sir`): the design
  (classify, uniformity, lane doubling, window addressing, HLSL → DXC offline). The lane-doubling model is
  bit-exact against a 64-lane console on 100,000 random programs, including the fork's refusal case.

## Not done

- Build 0 (c): the title's real GPU time, with RT on and off. Blocked: it needs the title rendering, so it moved to Build 5.
- Minimum spec in `plan.md` (owner decision, needed before Build 7).
- Everything from Build 3 onward: see [`tasks.md`](tasks.md).

## Open problems that could move the target

1. ~~Plan decision 5 is built on a wrong premise.~~ **Corrected 2026-11-27:** the title's upscale pass is
   recognised by its bindings and replaced; lowering the internal resolution is its own task.
2. **No known way to lower the internal resolution.** The fork's attempt to resize the title's targets made
   it assert at load. If the upscaler cannot lower 2560x1440, it buys little GPU time.
3. **The title's GPU time is probably above 45 ms** (the fork measured 43-63 ms, RT off). Above ~33 ms of
   pure title work, 30 fps needs the upscaler to actually cut pixels — see 2.
4. **Detection floor.** Watching and diffing costs ~13 ms of 4 worker threads every frame, whatever changed.

## Updates — one entry per upload, newest first

Each upload adds an entry: its own bar, what changed, what was measured, and what moved the percentage.
Older entries are never edited.

### 2026-11-27 (night, later) — Build 5: compute pipelines, query heaps, command signatures; the title loads content

```
[███████████░░░░░░░░░]  57 %   (56.55 → 57.75: Build 5 25 % → 35 %)
title       1.2 s+ past the device: 95 compute pipelines, 6 stream pipelines asked for, 7.1 GiB of content loaded
measured fps   none yet (next: a null lookup in a game system at exe+0xb5a5f8; stream pipelines 0x2b0)
```

- **Built:** compute pipelines (device 0x58, 0x1f0) through the pipeline library and pool (`runtime/src/pipeline.rs`);
  a program the library lacks (the BVH builder's 80) still gets a valid object, and the lists drop its dispatches
  instead of running them with the previous pipeline (`ListStats::skipped`, a bug fixed in `crates/xlist`).
- **Built:** query heaps (0x138) and command signatures (0x148) in `crates/d3d12x`; ones the host refuses (a console
  argument type 0x69) are valid objects with no host side.
- **Fixed:** `clzero` (AMD only, absent on the Intel host) detoured per site to a stub that zeroes the line, tested
  end to end on the host; descriptor copies into unmapped memory refused and named, not written.
- **Rebuilt:** the program library in format 2 (2,837 programs, 82 root signatures, 0.8 s).
- **Diagnostics:** host frames in crash reports named by module and symbol (`debug = "line-tables-only"`). 445 tests green.

### 2026-11-27 (night) — Build 5: the title passes its render setup; root signatures; the BVH builder starts

```
[███████████░░░░░░░░░]  56 %   (53.55 → 56.55: Build 5 0 % → 25 %)
title       device setup (1 queue, 5 fences, 4 X-queues, 439 allocators, 581 lists), back buffers, root signatures
measured fps   none yet (next: compute pipelines 0x1f0 for the BVH builder, then Execute and Present)
```

- **Fixed:** the title's render setup opens an arena for D3D's memory (XMem allocator 0x80) and asserts its
  back buffers fill it; on console D3D takes resource memory through the title's XMem hooks. Ours now does too
  (each committed resource and heap is also an `XMemAlloc` with D3D's attributes, freed with it).
- **Fixed:** the BVH builder sets console-only root signature flags (0x1000000, 0x2000000) the host rejects;
  the serializer export drops them. **Built:** `CreateRootSignature` (device 0x80, `crates/d3d12x/src/root.rs`).
- **Built (diagnostics):** crash reports name thrown C++ types and values (HRESULTs), `REBUILD_PEEK` prints
  title globals at a crash, `REBUILD_THREADS` unwinds every other thread. Tests: 439 green.

### 2026-11-27 — Control flow: 7,402 of 9,128 agnostic programs translate and compile; round 3-4 lanes merged

```
[███████████░░░░░░░░░]  53 %   (51.6 → 53.55: Build 4 52 % → 65 %)
shaders     agnostic set 7,402 / 9,128 compile (v0 2,837 / 2,881; interface diffs vs the oracle on v0: 0)
measured fps   none yet (Build 5 in progress: device by the lead; list, pipelines, resources by lanes)
```

- **Built (Build 4, lane shader-cf, PR #26):** branches and loops structurized into HLSL with EXEC as data
  ([`runs/build4-v1.md`](runs/build4-v1.md)); checked on 17,603 random graphs. Two translator bugs found by lane S7
  fixed (fp16 components, no-output pixel shaders); v0 now matches the oracle's interfaces exactly.
- **Merged (round 3):** device research (#19), GDK local storage and HDR (#20), oracle classification and 4 swapped
  oracle files (#21), the pipeline library (#22: 82/82 root signatures, 1,491 PSOs, 0 refusals), built-in test
  profiles (#23), our BCPack decoder (#24: bit-exact, 1.7-3x the fork) and the D3D12.X command list (#25: 0.47 µs a
  draw).
- **Built (lead):** Build 3 closed (the title reaches `D3D12XboxCreateDevice`); session folders for diagnostics;
  `D3D12ConfigureVirtualMemoryX` fixed. Tests: 375 green.

### 2026-11-27 — Build 3 closed: the title reaches its first render call (1.4 s)

```
[██████████░░░░░░░░░░]  51 %   (49.8 → 51.6: Build 3 88 % → 100 %)
title       entry point → D3D12XboxCreateDevice in 1.4 s, exit 0, machine unaffected
measured fps   none yet (Build 5: the device)
```

- **Fixed:** the title's code assumes its thread-local block is slot 0 (as an exe's is); beside our host it had slot
  6, and read our memory as its allocator. Now the two blocks and indices are swapped on every thread (a TLS
  callback of our exe for new threads), at no cost per access; the fork patched every access into a call
  ([`runs/build3-reached.md`](runs/build3-reached.md)).
- **Built:** a crash reporter for title runs (code, address, what is mapped there, the unwound call chain).
- **Measured:** `REACHED the first render call: D3D12XboxCreateDevice` 1.4 s after the entry point.
- **Next:** Build 5, the device. Two GDK interfaces the title asks for are not served yet.

### 2026-11-27 — Our v0 DXIL against the oracle: interface diffs 2,557 → at most 42 of 2,834

```
[██████████░░░░░░░░░░]  49 %   (unchanged; closes the v0 row once the last 42 are read)
shaders     v0 set 2,834 / 2,881 compile; interface vs the oracle: SigIn 35, SigOut 3, Bindings 4
```

- **Fixed (lane tool, `crates/shader/src/bitcode.rs`):** DXC writes symbol names in LLVM's 6-bit characters, which
  the reader kept as raw numbers, so none of our `dx.op` calls resolved. The self-check never saw it (the oracle's
  emitter writes plain names); it still gives 0 diffs.
- **Built (Build 4):** our signatures mirror the console's: every element, in order, each output with its own
  mask, so registers match the oracle's (stages link by signature).
- **Named reasons** for differing (`oraclediff`): our shadow table and immediate-sampler spaces, and the order of
  the GPU-generated vertex and instance ids. Every injected mutation is still caught.
- **Measured** ([`runs/build4-oraclediff.md`](runs/build4-oraclediff.md)): SigIn 1,231 → 35, SigOut 801 → 3,
  Bindings 525 → 4.

### 2026-11-27 — Compute programs: the v0 set is 2,834 of 2,881 (98.4 %) translated and compiled

```
[██████████░░░░░░░░░░]  49 %   (49.5 → 49.8: +0.3 Build 4)
shaders     0 / 25,694 counted (oracle match needed); v0 set 2,834 / 2,881: pixel 1,791, vertex 926, compute 117
measured fps   none yet
```

- **Built (Build 4):** the compute stage (thread and group ids, `numthreads`, LDS, barriers), untyped buffer stores,
  typed buffer loads and stores through host typed views built from the console descriptor, `a16` image stores,
  `v_rcp_iflag_f32` ([`runs/build4-cs.md`](runs/build4-cs.md)).
- **Measured:** compute 99 → **117 of 132**; vertex **926 of 951**; pixel **1,791 of 1,798**.
- **Next:** the execution check (each translated program run on the card against the oracle's output), which
  is what makes programs count toward the 25,694; then programs with control flow (outside v0).

### 2026-11-27 — Hardware limits for testing (owner request); files test fixed for Developer Mode hosts

```
[██████████░░░░░░░░░░]  49 %   (unchanged: a testing feature, not a build step)
tests       317 green, 0 failing
```

- **Built:** user-set hardware limits, so people can test how the title runs on less than their machine has.
  Four settings in the launcher (`limits.ram_mb`, `limits.vram_mb`, `limits.cpu_cores`, `limits.cpu_rate`; 0 = no
  limit), passed to the layer and applied before the title starts (`crates/runtime/src/limits.rs`): RAM through
  the title's budget and the job object's commit cap, CPU cores through process affinity, CPU usage through a
  job hard cap. VRAM is recorded now and enforced by our GPU heaps in Build 5 (marked experimental until then).
- **Fixed:** the files F5 test compared one line too many on hosts that allow symbolic links (Developer Mode).

### 2026-11-27 — Vertex shaders: 911 of 951 compile (95.8 %); pixel 1,790; descriptor shadow table decided

```
[██████████░░░░░░░░░░]  49 %   (49.05 → 49.5: +0.45 Build 4)
shaders     0 / 25,694 counted (oracle match needed); v0 pixel 1,790 / 1,798, v0 vertex 911 / 951 translated and compiled
measured fps   none yet
```

- **Built (Build 4):** the vertex shader stage. The fetch subroutine call becomes the attribute loads it would do
  (from the host's input assembler), v0/v3 are vertex and instance ids, `exp pos0` and `exp param<n>` become the
  position and the output rows. Untyped and indexed buffer loads.
- **Decided (plan decision 9):** a descriptor shadow table (stride and record count per heap index, written by the
  runtime, read at `t0, space101`) for indexed buffer loads with the console's exact bounds.
- **Measured:** vertex 595 → 613 → 860 → **911 of 951** ([`runs/build4-vs.md`](runs/build4-vs.md)); pixel **1,790 of 1,798**.

### 2026-11-27 — Cube maps: 1,788 of 1,798 v0 pixel shaders compile (99.4 %)

```
[██████████░░░░░░░░░░]  49 %   (unchanged: the pixel shader set's last forms)
shaders     0 / 25,694 counted (oracle match needed); v0 pixel shaders: 1,788 / 1,798 translated and compiled by DXC
```

- **Built (Build 4):** `v_cubeid/sc/tc/ma_f32` as the ISA defines them, and cube sampling: the console's (s, t, face)
  coordinates are turned back into the direction `TextureCube` takes (each face checked by hand to return a vector
  proportional to the original). Left (10): `s_cselect_b64` (3), shifts on special operands (3), buffer loads and
  stores (3), `image_atomic_or` (1).

### 2026-11-27 — 1,782 of 1,798 v0 pixel shaders compile (99.1 %): 16-bit addresses, lane swizzles

```
[██████████░░░░░░░░░░]  49 %   (48.9 → 49.05: +0.15 Build 4)
shaders     0 / 25,694 counted (oracle match needed); v0 pixel shaders: 1,782 / 1,798 translated and compiled by DXC
measured fps   none yet
```

- **Built (Build 4):** `a16` sampling addresses (two fp16 per register, LOD included); `ds_swizzle_b32` (quad mode
  as four `QuadReadLaneAt` with constant lanes, bitmask mode as `WaveReadLaneAt`); a lane-mask write now ends a
  register's known constant at that point (it was refused program-wide before, which lost 14 immediate samplers).
- **Measured:** 1,748 → 1,759 → 1,773 → **1,782 of 1,798** ([`runs/build4-ps.md`](runs/build4-ps.md)). Left (16): cube
  maps (6), `s_cselect_b64` (3), shifts on special operands (3), buffer loads and stores (3), `image_atomic_or` (1).

### 2026-11-27 — 1,748 of 1,798 v0 pixel shaders compile (97.2 %): 16-bit interpolation and mixed-precision FMA

```
[██████████░░░░░░░░░░]  48 %   (48.6 → 48.9: +0.3 Build 4)
shaders     0 / 25,694 counted (oracle match needed); v0 pixel shaders: 1,748 / 1,798 translated and compiled by DXC
measured fps   none yet
```

- **Built (Build 4):**
  - **16-bit interpolation** (`v_interp_p1ll_f16`, `p1lv`, `p2_f16`, VOP3): `src0` packs attribute, channel and the
    high half of a packed fp16 pair (checked on the corpus against each program's input table). The host
    interpolates in float and rounds to fp16 in the low half. A channel the packed element lacks reads 0.
  - **VOP3P mixed precision**: `v_fma_mix_f32`, `v_fma_mixlo_f16`, `v_fma_mixhi_f16` (each source f32 or either fp16
    half; neg, abs, clamp).
  - Refused programs' dumps now carry the input table and the signature.
- **Measured:** 1,684 → 1,720 → **1,748 of 1,798** ([`runs/build4-ps.md`](runs/build4-ps.md)). Left (50): untraced
  samplers (14), 16-bit image addresses (11), `ds_swizzle` (9), cube maps (6), and single programs.

### 2026-11-27 — 1,684 of 1,798 v0 pixel shaders compile (93.7 %): immediate samplers, primitive mask, constant folding

```
[██████████░░░░░░░░░░]  48 %   (48.3 → 48.6: +0.3 Build 4)
shaders     0 / 25,694 counted (oracle match needed); v0 pixel shaders: 1,684 / 1,798 translated and compiled by DXC
measured fps   none yet
```

- **Built (Build 4):**
  - **Immediate samplers:** samplers the code builds from constants (root layout `ImmediateSampler`) are decoded from
    the console's 4 sampler words (`crates/sir/src/sampler.rs`: address modes, filter and reduction, anisotropy,
    LOD range and bias, border) into D3D12 static samplers at `register(s{k}, space100)` for Build 5's root
    signature. Approximations (half-border clamps, register border colours) are flagged per sampler.
  - **Constants** are tracked through `s_mov_b32`, `s_movk_i32`, `s_brev_b32` and the bitwise, shift, add and pack
    ops, which is how compilers build sampler words.
  - **The primitive mask** (the SGPR after a pixel shader's user data) only steers the console's attribute
    interpolation; the host interpolates, so it reads 0.
  - `s_brev_b32`, `v_cmp_class_f32` (decided from the float's bits, so denormal flushing cannot change it).
- **Measured:** 1,468 → 1,528 → 1,565 → **1,684 of 1,798** ([`runs/build4-ps.md`](runs/build4-ps.md)). Left: 16-bit
  interpolation (62), untraced samplers (14), 16-bit image addresses (11), `ds_swizzle` (9), cube maps (6), others.
  Tests: `sir` 24 green (4 new sampler decode tests).

### 2026-11-27 — Runs can no longer take the machine down (root cause found); five lane PRs merged; 1,468 pixel shaders compile

```
[██████████░░░░░░░░░░]  48 %   (was 47 %: +0.45 Build 3, +0.3 Build 4)
runtime     GDK runtime (xgameruntime.dll) is ours; CRT (13) and F5 (5) file functions bound; LoadLibraryExA overridden
safety      title memory budget, job object, watchdog, preflight; graphics placement no longer spins in the kernel
shaders     0 / 25,694 counted (oracle match needed); v0 pixel shaders: 1,468 / 1,798 translated and compiled by DXC
measured fps   none yet
```

- **Fixed (machine safety):** `runtitle` runs had to be ended by rebooting the PC, three times. Two causes, both ours:
  1. Physical pages were pagefile sections committed in full, with no cap. The title asks for its whole console
     budget, the system ran out of commit, and no program could start. Now a hard budget
     (`crates/runtime/src/budget.rs`, min(12 GiB, RAM/3)), a job object around the run (killed on exit, own
     commit capped), a lock-free watchdog, and a refusal to start when the system lacks commit.
  2. **Root cause of the unkillable process:** graphics reservations were placed with `VirtualAlloc2` address
     requirements plus write-watch. Into a 4 GiB window with no room, that call spins in the kernel instead of
     failing; the thread cannot be terminated, so neither can the process. Now we find the gap with
     `VirtualQuery` and ask for that exact address (`xmem::host::free_gap`). Measured with the new `exitprobe`
     (no title, each pattern in a capped child): the 17th 256 MiB reservation spun for minutes before; after,
     1 to 64 GiB all place, teardown ~5 ms each. This was also why the title looked like it waited at startup.
- **Merged:** lane PRs #14 (launcher idle, U6), #15 (shader S6 `oraclediff`), #16 (files F4-F5), #17 (our XMA2
  decoder, within 1 LSB of FFmpeg on all 1,818 streams), #18 (GDK runtime: task queues, XAsync, 11 interfaces).
- **Built (Build 3):** `xgameruntime.dll` is served by `rebuild-gdk`. The title's GDK stub loads it with
  `LoadLibraryExA`, which was not overridden, so it got the PC's own GDK runtime. The CRT file functions (13) and
  F5 (5) are bound.
- **Built (Build 4):** NSA image addressing, and `unorm` ignored where it has no effect (loads, stores, atomics).
  Refusals now name the form. **1,468 of 1,798 v0 pixel shaders translate and compile (81.6 %, was 1,360).**
  Next: immediate samplers (158 programs: the sampler is baked into the code as constants).
- **Open:** the files test `f5_…match_the_host` gives 46 vs 45 since the reboot (host-side difference; for the
  files lane). The title is not run again without the owner's OK.

### 2026-11-27 — Runtime bound to the merged lanes; settings never replaced; shader translation started: 427 pixel shaders compile

```
[█████████░░░░░░░░░░░]  47 %   (was 46 %: +0.75 Build 3, +0.75 Build 4)
runtime     files (27), audio (19), DirectStorage (1), xg (2) bound; drives G:/T:/S:; stack sampler in runtitle
shaders     0 / 25,694 counted (oracle match needed); v0 pixel shaders: 427 / 1,798 translated and compiled by DXC
measured fps   none yet
```

- **Built (Build 3):** the runtime binds what the lanes built. File overrides (27) go through one shared vfs
  with DirectStorage, audio (19), DirectStorage (page destinations over the title's physical frames, with the
  fork's retry), and the xg texture computer behind its 13-slot vtable. The drives are `G:` read-only, `T:`
  emptied every run and `S:` storage. `runtitle` has a stack sampler that names where the title's main
  thread waits.
- **Built (settings):** `crates/config` never replaces a user's setting. Explicit values are kept even when
  equal to the default, unknown, wrong-typed and refused entries are written back as they were, and the
  schema is never lowered. Six registry entries the lanes asked for were added.
- **Built (Build 4):** the GFX10 lifter (`crates/sir/src/lift.rs`): registers as HLSL variables, EXEC per
  lane, about 150 ALU forms with the console's float semantics. Pixel shader IO (`lift/io.rs`):
  interpolation, exports, discard, signatures by semantic.
- **Measured:** opcode coverage of the v0 set ([`runs/build4-coverage.md`](runs/build4-coverage.md)): 119
  opcodes cover 95.6 %. ALU: nothing left in the v0 set ([`runs/build4-lift.md`](runs/build4-lift.md)).
  **427 of 1,798 v0 pixel shaders translate and compile** ([`runs/build4-ps.md`](runs/build4-ps.md)).
  Tests: 250 green, 0 warnings.
- **Decided:** translation design in `docs/design/shader-ir.md` (registers as variables, DXC builds the SSA;
  predication; the fork's interface). BCPack (owner): our decoder, tables outside git, a decode cache.
- **Incident:** a helper overwrote a game file with a shell wildcard. The owner is re-dumping it, helpers
  are frozen out of the game folder, and the folder goes read-only once it is restored. The shader corpus
  is now read from a copy outside the game folder.
- **Next:** resource tracing (descriptor loads from root parameters), which unblocks 1,364 more pixel
  shaders; then vertex shaders. The title runs again once the file is restored.

### 2026-11-27 — Second lead session: all 11 helper PRs merged (shader decoder, FSR, files, audio, DirectStorage, xg, launcher); round 2 of the lanes

```
[█████████░░░░░░░░░░░]  46 %   (was 43 %: +0.75 Build 3, +1.5 Build 4, +1.0 Build 7)
runtime     32 / 73 of our functions bound; files (22), audio (19), DirectStorage (1) and xg (2) built in lanes, binding next
shaders     0 / 25,694 counted; all 25,694 decoded byte-exact (43.95M instructions) and the oracle set is complete
measured fps   none yet
```

- **Merged** PRs #5-#13: `shader` S1-S5 (census, GFX10 decoder, oracle set), `upscale` P3 (FSR 3.1.6 beside DLSS),
  `loader` L5 (76 run-time lookup names), `dstorage` D1-D4, `xg` X1-X4, `audio` A1-A4, `files` F1-F3, `launcher`
  U1-U5. Every diff checked for game bytes, the game path and vendor binaries first (only the OFL Inter fonts,
  credited). `CREDITS.md` is one table plus a fonts-and-libraries section.
- **Measured:** `cargo test --release` **238 green, 4 ignored, 0 warnings** (was 121).
- **Decided:** XMA is decoded by our own decoder (audio A5), not vendored, because it carries performance. A new lane
  `gdk` builds XTaskQueue, XAsync, XGame, XUser and XStore behind `xgameruntime!QueryApiImpl`. The CRT file
  functions get overrides (files F4). The shader lane builds the oracle diff tool (S6). Answers to items 26-62
  are in `secondary-agent.md` section 10.
- **What moved the %:** Build 3 75 → 80 % (files, audio and DirectStorage built; binding is next). Build 4
  25 → 35 % (the front end is closed: every program decodes byte-exact). Build 7 20 → 30 % (FSR works standalone).
- **Next:** bind the merged crates in `crates/runtime`, then the stack sampler (handoff step 1).

### 2026-11-27 — The title's own code runs: 16/16 DLL entry points, the exe's entry for 60 s; final handoff of this session

```
[█████████░░░░░░░░░░░]  43 %   (was 42 %: +0.75 Build 3, the title runs)
runtime     32 / 73 of our functions built
shaders     0 / 25,694 (core proven; waiting for lane shader's decoder)
measured fps   none yet
```

- **Built** `crates/runtime/src/title.rs`: prepared images loaded by the Windows loader, registered, and their
  IATs bound (title DLLs, our runtime, the host). Every unbuilt import goes to a generated **trap thunk** that
  records the call instead of crashing. Also `runtitle`, and the `d3d12_xs` exports: the device-creation stop
  point, the host's root-signature functions, and the virtual-memory setup.
- **Measured:** 17 images, 1,881 imports bound, 43 trapped. **All 16 title DLL entry points succeed. The exe's 2 TLS
  callbacks and its entry point run, for 60 s, without a crash.** It calls no unbuilt import and does not reach
  the first render call, so it is waiting on something. [`runs/build3-startup.md`](runs/build3-startup.md)
- **Handoff:** `claude-max-handoff.md` is now the final handoff of this lead session: the state in one screen,
  the owner's rules, how to work here, and the next actions in order (first, a stack sampler to find what the
  title waits on).
- **Shaders:** the core is ready, but real programs start counting only after lane `shader` delivers the census
  and the GFX10 decoder.

### 2026-11-27 — Prepared images: Windows maps the title's images with static TLS and runs nothing; handoff file

```
[████████░░░░░░░░░░░░]  42 %   (unchanged: a step inside Build 3's startup task)
runtime     32 / 73 of our functions built
shaders     0 / 25,694
measured fps   none yet
```

- **Found:** 4 title images use static TLS, the exe among them with 2 callbacks. Code mapped by hand cannot have
  static TLS on every thread; the Windows loader can.
- **Built** `crates/loader/src/prepare.rs` (the fork's idea, our code): a patched copy (DLL flag, no entry point,
  the import directory at its terminator, TLS callbacks cleared and their relocation neutralised) that Windows
  maps, relocates, gives static TLS and unwind tables, while running none of its code. A test prepares and
  loads the test binary itself: Windows assigns a static TLS index and this thread has a block.
- **Added** `claude-max-handoff.md`: the lead's in-flight state and exact next actions, so a Claude Max session
  can take over mid-step. It is updated every step.
- **Next:** bind the IATs of the loaded images, trap thunks for misses, then run the title's startup to its
  first render call.

### 2026-11-27 — Build 3: the title's images mapped and bound — 97.5 % of its imports

```
[████████░░░░░░░░░░░░]  42 %   (was 41 %: +0.75 Build 3, the title mapped)
runtime     32 / 73 of our functions built
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `maptitle`: every image in the title's folder parsed, placed, registered as a title module and
  mapped, with relocations applied and imports bound (title DLLs first, then our runtime, then the host).
  No title code runs; the game folder is only read.
- **Measured:** 17 images, 8,257 relocations, **1,876 of 1,924 imports bound (97.5 %)**. The 48 missing are
  exactly the work already assigned: files 21, audio 19, `d3d12_xs` 5, `xg_xs` 2, DirectStorage 1.
  [`runs/build3-maptitle.md`](runs/build3-maptitle.md)
- **Next:** Build 5's D3D12.X device (`d3d12_xs`, the 5 functions that are mine). Then run the title's startup
  (TLS callbacks, DLL entry points, the exe entry on its own thread) to its first render call.

### 2026-11-27 — Build 3: module, memory and process overrides; discovery is live (32 / 73 ours built)

```
[████████░░░░░░░░░░░░]  41 %   (was 41 %: +0.75 Build 3 overrides)
runtime     32 / 73 of our functions built (was 13)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/runtime/src/modules.rs`: `GetProcAddress`, `LoadLibrary*`, `GetModuleHandle*`,
  `GetModuleFileName*` and `FreeLibrary` over a registry of the title's mapped modules and our console DLLs
  (pseudo-handles above the user address space). `GetModuleHandle(NULL)` is the title's exe. The title sees
  its modules at console paths. **Every miss is logged**: that is the discovery list, live, including the GDK
  functions the title looks up by name.
- **Built** memory and process overrides: the title's `VirtualAlloc` with the console's GPU protection bits
  becomes graphics memory inside one window, write-watched (the fork found the title's heap arrives this
  way). `GetCommandLineA/W` returns the title's console path, never the host's arguments.
- **Measured:** 14 runtime tests. `runtimecensus`: **32 of our 73 built**. What remains of mine is
  `d3d12_xs` (4, Build 5); files (15), audio (19), DirectStorage (1) and the texture layout (2) are in lanes.
- **Next:** map the title's images through the real resolver (bound vs discovery, without running anything),
  then Build 5's D3D12.X device.

### 2026-11-27 — Build 3: all 13 `xmem` functions built and bound

```
[████████░░░░░░░░░░░░]  41 %   (was 40 %: +0.75 Build 3 xmem exports)
runtime     13 / 73 of our functions built (new bar)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/runtime/src/exports.rs`: the 13 `xmem.dll` functions with the console's exact signatures,
  bound by the resolver.
  - `XMemAlloc` does first-fit over write-watched graphics chunks, each inside one window; the title's own
    allocation hooks take over once installed.
  - `XMemVirtualAlloc` makes placeholders for physical pages.
  - `XMemVirtualQuery` reports the real flags.
  - The physical-page exports map, alias and unmap.
  - `XMemGetWorkingSetStatistics` is refused until its layout is known, never guessed.
- **Measured:** 11 runtime tests. `runtimecensus` now counts what is built: **13 of our 73**. A new bar on
  this page tracks it.
- **Next:** the 16 module overrides (`GetProcAddress` and friends, with discovery logging). Files are lane `files`.

### 2026-11-27 — Lane `upscale` merged: DLSS runs standalone; three more helper lanes

```
[████████░░░░░░░░░░░░]  40 %   (was 38 %: +2.0 Build 7, DLSS standalone)
shaders     0 / 25,694
measured fps   none yet
```

- **Merged PR #4 (`upscale`):** one `Upscaler` interface in the layer's own convention, a synthetic scene
  (jitter, exact motion vectors, normal or reversed depth, a supersampled truth image), and **DLSS through
  Streamline 2.14.16**. Streamline is declared from its MIT headers and loaded at run time; nothing NVIDIA's is
  committed. 0 NaN, 0 black frames, better than bilinear on every check. FSR 3.1 waits for the SDK download,
  which is approved (section 10).
- **Reviewed:** no binaries, DLLs or images committed; the DLSS DLL comes from `D:\tools\dlss` at run time.
- **More work for helpers** (`secondary-agent.md`): lane `files` (the 15 file overrides over `vfs`), lane `xg`
  (the console texture layout), and loader L5 (names the title looks up through `GetProcAddress`). Lanes
  `audio` and `dstorage` are waiting to start.
- **114 tests** in the workspace.

### 2026-11-27 — Build 3: the title's memory: `xmem` placement and physical pages, which the GPU reads in place

```
[████████░░░░░░░░░░░░]  38 %   (was 37 %: +0.75 Build 3 memory)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/runtime/src/xmem.rs`: the console's meaning of the arguments (from the fork's study), with
  our placement. Graphics memory never straddles a 4 GiB window, so its CPU address *is* its GPU window offset,
  exactly what the shader core lowers to. It is write-watched for the ring and recorded per region with its real
  flags (the fork reported "graphics" for everything). 32-bit memory stays below 4 GiB.
- **Built** `crates/runtime/src/physical.rs`: the console's physical pages as pagefile-section views in
  placeholder reservations. Remaps keep contents; two views of a frame share bytes.
- **Measured:** **the GPU opens a section view as a heap, an alias view too** (`gputest --labels`). Physical-page
  memory, where the fork found the title's GPU heap arrives, is read by the GPU in place: no write-watch, no
  upload. [`runs/build2-labels.md`](runs/build2-labels.md)
- **Next:** the 13 `xmem` exports bound through the resolver, then the file and module overrides.

### 2026-11-27 — Build 3: 89.5 % of the title's imports bind straight to the host

```
[███████░░░░░░░░░░░░░]  37 %   (was 36 %: +0.75 Build 3 runtime census)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/runtime`: a three-tier resolver (override / host / console-only) behind the loader's
  `Resolver` trait. Also `runtimecensus`, which looks every import up on the real host.
- **Measured:** of the 697 imports the title's own DLLs don't supply, **624 (89.5 %) bind straight to the
  host** with no wrapper, 34 are overrides (files 15, modules 16, memory 2, process 1), and 39 are
  console-only in 7 DLLs. [`runs/build3-runtime-census.md`](runs/build3-runtime-census.md)
- **Decided:** the runtime is 73 functions, not a kernel. Threads, synchronisation, timers, TLS, the C
  runtime, sockets and HTTP stay the host's. Audio (19) and DirectStorage (1) become helper lanes; `xmem`,
  the overrides and D3D12.X are mine.
- **Next:** `xmem` onto Builds 1-2's memory, then the file and module overrides.

### 2026-11-27 — Lanes `input` and `vfs` merged: the file system and input for Build 3

```
[███████░░░░░░░░░░░░░]  36 %   (was 34 %: +2.25 Build 3: files and input)
shaders     0 / 25,694
measured fps   none yet
```

- **Merged PR #2 (`input`):** XInput pads and the keyboard stand-in pad, menu isolation (neutral input while
  the menu is open, neutral until all keys are up on close), and the title's GameInput imports mapped. 16 tests.
- **Merged PR #3 (`vfs`):** console paths to host paths, a read-only game package, a write overlay, and mods as
  ordered layers with conflict listing. 18 tests. The game folder's hash is unchanged by a write-heavy run;
  10,000-file open+read costs +3.3 µs over the OS floor.
- **Reviewed:** both stayed inside their lanes; no game paths, nothing vendored. Both `Cargo.toml` member
  conflicts resolved.
- **Decided from their notes:** input is polled on a thread of its own (~170 µs a poll with focus, too much for
  the present thread). `T:` starts empty every run, as on the console. Storage gets its own drive root. Hash
  checks read `fs::metadata` per file (an NTFS directory-entry time lag fooled the first test).
- **95 tests** in the workspace (1 ignored), 0 warnings.
- **Next:** Build 3's runtime: kernel objects (threads, events, waits, timers, TLS) and the drives wired through `vfs`.

### 2026-11-27 — Shader core: memory through the tiled windows and native half precision, exact on the card

```
[███████░░░░░░░░░░░░░]  34 %   (was 33 %: +0.75 Build 4 memory and f16)
shaders     0 / 25,694
measured fps   none yet
```

- **Built:** `Load` / `Store` at console addresses and packed half-precision ops in `sir`, with a memory model
  in both interpreters (unmapped tiles read 0 and drop writes). Memory lowers to `ResourceDescriptorHeap[window]`
  plus a 32-bit offset: uniform windows through `WaveReadLaneFirst`, no page directory, no clamp. Half
  precision lowers to native `float16_t2`. Region masks are now kept in both modes, so stores are always
  masked.
- **Measured:** 50,000 programs bit-exact between the interpreters, outputs and memory. On the card, against
  two real Build 1 tiled windows: 600 runs, 342 loads + 331 stores + 650 half ops, every output lane and
  every word of the 4 mapped pages exact. [`runs/build4-sirtest.md`](runs/build4-sirtest.md)
- **This removes the fork's per-access page lookup** (its estimate: 2-5 ms of GPU time) and its f16 widening
  (1-3 ms), by construction.
- **Next:** section 9, step 3 waits for lane `shader`. Meanwhile, Build 3's second half: the console runtime.

### 2026-11-27 — Lane `loader` merged: the PE loader and the title's import census

```
[███████░░░░░░░░░░░░░]  33 %   (was 29 %: +4.5 Build 3 first half)
shaders     0 / 25,694
measured fps   none yet
```

- **Merged** PR #1 from helper lane `loader` (3 commits, +3,145). `crates/loader` is a PE32+ parser, a
  map-at-base, and binding through a resolver, with no dependencies and no title code run. `imports` is the census tool.
- **Their numbers:** 4,523 / 4,523 System32 PE32+ images parsed, 0 errors; 200,000 random corruptions, no
  panic; `dbghelp.dll` mapped beside the Windows loader with 9,912 relocations, 0 wrong. The title: 18 images,
  93 DLLs, 962 functions; **the GDK discovery list is 697 functions in 86 DLLs**. Game folder hash unchanged.
- **Reviewed:** the CSV holds names only (no paths, no bytes); `unsafe` appears only in the memory-reservation
  module; nothing is vendored. Merge conflicts in the two `Cargo.toml` files resolved (`config` + `loader`).
- **Answered the eight "For the lead" items** (`secondary-agent.md` section 10). The plan's FidelityFX
  wording is corrected; `GetProcAddress` misses are now part of discovery; the three build warnings are fixed;
  helpers never run `gputest`; the FidelityFX SDK download is approved (pinned, outside git).
- **59 tests** in the workspace, 0 warnings.
- **Next:** step 2 of section 9, memory lowering in `sir`.

### 2026-11-27 — Shader core: control flow and live masks, exact on the card

```
[██████░░░░░░░░░░░░░░]  29 %   (was 28 %: +0.75 Build 4 live masks)
shaders     0 / 25,694
measured fps   none yet
```

- **Built:** structured control flow in `sir` (`If` / `Else` / `EndIf` / `Phi`), an EXEC stack in both
  interpreters, a generator of nested branches, and `hlsl::plan`. A branch stays a real host branch when its
  condition is uniform or nothing inside reads other lanes; otherwise it is predicated with EXEC as data.
- **Measured:** 100,000 random branched programs are bit-exact between the 64-lane console and the doubled
  model. On the card, 601 runs (201 agnostic + 400 doubled) are exact on every lane: 182 host branches, 124
  predicated, nested to depth 2. [`runs/build4-sirtest.md`](runs/build4-sirtest.md)
- **Fixed on the way:** `ReadFirstLane` with no active lane is now lane 0 in both models. DXC needs `and()`
  for `bool2` masks.
- **Agent lanes:** `vfs`, `upscale` and `input` added to `secondary-agent.md`, plus shader S5 (the oracle
  set), ready for the coordinator to start.
- **Next:** step 2, memory lowering to window descriptors, native 16-bit and quad ops.

### 2026-11-27 — Launcher track: one settings registry; launcher lane opened

```
[██████░░░░░░░░░░░░░░]  28 %   (unchanged: the launcher track has its own bar)
launcher    [███░░░░░░░░░░░░░░░░░]  1 / 7 steps
shaders     0 / 25,694
measured fps   none yet
```

- **Owner request:** a proper launcher in front of the layer, with options, settings, features to enable and
  disable, mods, and an in-game menu.
- **Designed** ([`docs/design/launcher.md`](docs/design/launcher.md), plan decision 7): one registry drives
  three faces: the desktop launcher (egui), the layer at start, and the in-game menu (F10, input isolated).
  Mods are folders layered by the overlay file system; the game folder is never written.
- **Built** `crates/config`: 18 options in 8 groups (game, display, upscaler, title, memory, shaders,
  diagnostics, menu), each with its type, range, measured-best default, live/restart and stable/experimental.
  `rebuild.toml` load and save; unknown keys reported; bad values refused with reasons (`fsrQuality` 4: the
  fork's black screen). Stable-only profiles. 6 tests; 43 in the workspace.
- **Opened** helper lane `launcher` (L1-L4) in `secondary-agent.md`.
- **Next:** `sir` live-mask analysis (main line).

### 2026-11-27 — The shader core runs on the card; handover doc

```
[██████░░░░░░░░░░░░░░]  28 %   (was 27 %: +0.75 Build 4 emission proven)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/sir/src/hlsl.rs` (two emission modes: agnostic wave32, and doubled with EXEC as data) and
  `gen.rs` (random programs). Also `sirtest`, which runs IR → HLSL → DXC → GPU and compares with the interpreter.
- **Measured:** 300 runs (100 agnostic + 200 doubled) match the 64-lane console on every lane of every output.
  A per-lane `WaveReadLaneAt` index is exact on this card, which closes the design's open question.
  [`runs/build4-sirtest.md`](runs/build4-sirtest.md)
- **Handover:** `secondary-agent.md` now has a live "current state" (section 8) and "Taking over as lead"
  (section 9): the owner's working rules, the next eight steps in order, and the owner-only items. It is
  updated every step.
- **Next:** the launcher (owner request; designed next), then `sir` live-mask analysis and memory lowering.

### 2026-11-27 — The shader core: design, and the lane model proven

```
[█████░░░░░░░░░░░░░░░]  27 %   (was 26 %: +1.5 Build 4 design and lane model)
shaders     0 / 25,694   (programs count once they are emitted and match the oracle)
measured fps   none yet
```

- **Designed** the shader core ([`docs/design/shader-ir.md`](docs/design/shader-ir.md)): classify each program
  (most never observe the wave size, so they run as plain wave32 at no cost), then uniformity, lane doubling
  for the rest, window addressing, native 16-bit/quad/wave ops, and HLSL compiled offline by DXC.
- **Decided: HLSL + DXC, not our own DXIL writer.** DXC's optimiser runs on every program, and a bitcode
  writer carries no performance. Checked: DXC 1.8 (Windows SDK) accepts SM 6.6, `[WaveSize(32)]`, wave and
  quad intrinsics, and `float16_t`.
- **Built** `crates/sir`: IR, uniformity, the classifier, and two interpreters (a 64-lane console, and lane
  doubling on a 32-lane host using host-wave operations only). **100,000 random programs bit-exact**,
  and the fork's refusal case (a reduction with lanes 3, 40 and 63 running) is exact. 35 tests in the
  workspace.
- **Rules found:** wave ops must run in host-uniform control flow (predicate divergent branches that hold
  them). A per-lane `WaveReadLaneAt` is to be checked on the GPU.
- **Next:** HLSL emission from `sir`, run on the card against the interpreter.

### 2026-11-27 — Build 2 closed: GPU-written labels into title memory

```
[█████░░░░░░░░░░░░░░░]  26 %   (was 24 %: +1.6 Build 2 closed)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/uma/src/labels.rs`: the title's memory is opened as a D3D12 heap with a buffer placed
  over it. The title's bottom-of-pipe labels become `WriteBufferImmediate` (MARKER_OUT) straight into it.
- **Measured** with `gputest --labels`: plain and write-watched title memory both open. GPU labels are seen
  65 µs *before* the fence reports, against 17 µs *after* it for a CPU relay. 200/200 in order, never
  early. [`runs/build2-labels.md`](runs/build2-labels.md)
- **Decided:** labels are GPU-written. The relay is a fallback only. This designs out the fork's
  "a frame late" fence trap.
- **Opened:** write-watched title memory can be a GPU heap, so the GPU could read rarely-written data
  directly over PCIe, with no upload. That is a Build 5 measurement, per resource.
- **First run under the measurement lock** (`agents/MEASURING`).
- **Next:** Build 3's second half and Build 5's device need the loader (helper lane `loader`). Meanwhile:
  the shader IR design (decision 4), from the fork's census numbers until lane `shader` reports.

### 2026-11-27 — Build 2: the upload ring; helper-agent plan; README links

```
[█████░░░░░░░░░░░░░░░]  24 %   (was 19 %: +4.8 Build 2 ring)
shaders     0 / 25,694
measured fps   none yet
```

- **Built** `crates/uma/src/ring.rs`: write-watched regions, a coordinator thread, a persistent diff pool,
  3 staging slots, and one scatter dispatch per window on an async compute queue behind a fence.
  `flush()` returns at once.
- **Proven** with `gputest --ring`, 60 frames at the title's real shape (34 MB, 342k runs a flush): the submit
  thread is blocked 32 µs median, 72 µs worst. The upload lands in 19.4 ms. Both regions match byte for
  byte. [`runs/build2-ring.md`](runs/build2-ring.md)
- **Guard:** the submit thread is marked, and copying title memory on it panics in every build (tested).
- **Learned:** the first touch of a never-used page sets its write-watch bit, even a read. Harmless.
- **Fixed the README links:** every badge now links somewhere; the fork's `ROADBLOCK.md` is linked; the
  unpushed fork commit is labelled as local; `runs/README.md` index added; LICENSE (GPL-3.0) added. All
  links checked 200 on GitHub. (That fix was pushed without its own entry; it is recorded here.)
- **Added** `secondary-agent.md`: a temporary plan for helper agents. Lanes `loader` (Build 3 first half)
  and `shader` (the Build 4 census and decoder), each in its own worktree, branch and crate. A measurement
  lock (`agents/MEASURING`) keeps their builds off the lead's timed runs.
- **Next:** Build 2's GPU→CPU labels.

### 2026-11-27 — Decisions: our own shader core; plan decisions 3 and 5 corrected

```
[████░░░░░░░░░░░░░░░░]  19 %   (unchanged: decisions only)
shaders     0 / 25,694
measured fps   none yet
```

- **Decided: the shader core is ours**, chosen for performance. The fork's own analysis puts ~10-22 ms of
  the title's GPU frame in its translator's core choices: lane state, per-access address lookups, scalar work
  done per lane, emulated 16-bit and wave ops. Ours is designed around fixing those from the first line.
  Only the decode front end (no performance in it) may be vendored, with credit.
- **Corrected decision 3:** translate on the title's own threads, no list cache, ≤ 1 µs a draw (from
  Build 0 (b)).
- **Corrected decision 5:** the title's FSR is found by its bindings, not called through 0x218. DLSS/FSR
  replace that pass. Lowering the internal resolution is its own Build 7 task; tiles at console addresses
  may let a smaller target sit behind the title's address.
- **Next:** Build 2.

### 2026-11-27 — Build 1: tiled VRAM at console addresses

```
[████░░░░░░░░░░░░░░░░]  19 %   (was 11 %: +8.0 Build 1 closed)
shaders     0 / 25,694 (bar added; the census tool is the first Build 4 task)
measured fps   none yet
```

- **Built** `crates/uma/src/tiles.rs`: reserved 4 GiB windows, 64 MiB tile heaps, an `UpdateTileMappings`
  batcher, and aliasing for other resources at one address. Also `arena.rs` for untileable resources, and
  card-page reuse in `Uma` (it used to grow forever). 21 tests in `crates/uma`.
- **Proven on the card:** 7 `gputest --tiled` proofs; the `--arena` split (only D32S8 and D24S8 need it); a
  `--scale 10` run of 10 GiB in 5,120 random placements, 0 wrong, no device removal.
- **Measured:** `uploadbench --tiled` shows no penalty from tiles. Both destinations swing between ~7-10 and
  ~19 ms of GPU time with the card's clock state.
- **Shaders:** Build 4 rewritten as a census against the fork's 25,694 programs, used as an oracle. The goal
  is 0 refusals and wave32 everywhere. A separate shader bar is on this page.
- **Owner decision raised:** the fork's shader crate is GPL-3.0-or-later and could legally be reused here
  with credit. Fresh code or reuse?
- **Next:** Build 2, CPU-written memory on an async ring.

### 2026-11-27 — Build 0 (b): per-draw translation

```
[██░░░░░░░░░░░░░░░░░░]  11 %   (was 8 %: +3.0 Build 0 (b), +0.8 Build 6 translator core)
measured fps   none yet
```

- **Built** `crates/xlist`: list calls as records, and a lock-free translator (state elision, console→PC
  addresses, pipeline table). It writes to a counting sink or a real D3D12 list. 5 tests; 20 in the
  workspace, all green.
- **Built** `listbench`: a synthetic frame in the title's shape (4,500 draws, 20,311 calls, 48 lists, 1,433
  pipelines, 200k descriptor slots). It runs on a persistent pool of 1-6 workers, and every frame is
  executed on the GPU.
- **Measured:** our cost is 0.03 µs a draw; with D3D12 and the driver it is 0.43 µs a draw, 1.9 ms a frame on
  one thread (0.52 ms on 6). 12,027 descriptor writes from a dirty list cost 2.0 ms.
  [`runs/build0b-lists.md`](runs/build0b-lists.md)
- **Decided:** decision 3 is simplified. One thread fits, so translate on the title's own recording threads,
  with no extra pool. The **list cache is dropped**: it could not pay at 2 ms a frame. The old 29 µs a draw
  was memory bookkeeping, which decision 1 removes.
- **Moved:** Build 0 (c) needs the title rendering, so it closes in Build 5. Until then the fork's 43-63 ms stands.
- **Next:** Build 1, tiled VRAM at console addresses.

### 2026-11-27 — README: why we chose a rebuild

```
[██░░░░░░░░░░░░░░░░░░]   8 %   (unchanged: documentation only)
measured fps   none yet
```

- **Added** "Why a rebuild" to the README: where the old 131 ms goes, the 19 failed tuning experiments, why
  the memory model can't be retrofitted, and why not adopt xcl. Also a table of each main choice with its
  reason and its evidence, including the Build 0 (a) numbers, and the open questions stated plainly.
- **Fixed** two stale README lines: the test count (15, not 8), and list caching, which now reads "kept only if
  the hit rate pays", as the plan says.
- **Next:** unchanged — Build 0 (b).

### 2026-11-27 — Build 0 (a): the upload benchmark

```
[██░░░░░░░░░░░░░░░░░░]   8 %   (was 2 %: +4.0 Build 0 (a), +1.6 Build 2 chunk diff; Build 1 unchanged at 2.0)
measured fps   none yet
```

- **Built** `uploadbench` (`crates/uma/src/upload.rs`): write-watch on a 1 GiB region, 64 B diff (shadow and
  hash), gather into an upload heap, then copy-per-run or compute scatter. GPU timestamps, and a full
  readback check per config.
- **Built** `crates/uma/src/chunks.rs`: diff, run joining, gather and chunk indices, with 7 unit tests (15 in
  total, all green).
- **Measured** 40 configs, all verified byte-exact: 12.64 GB/s ceiling. Hash + scatter + 4 workers gives
  33.8 ms at 108 MB all changed, and 16-30 ms at the title's real shape. Copy-per-run is rejected (146 ms to
  record 440k copies). [`runs/build0a-upload.md`](runs/build0a-upload.md)
- **Decided:** decision 1 holds, with conditions: scatter, on workers, a frame ahead. Detection costs a
  floor of ~13-17 ms of worker time whatever changed.
- **Learned from the fork** (jleybourne/Triumph), only where it changes the rebuild: the title's real upload
  shape (226k ranges / 31 MB); 0x218 is not the upscale call; the internal resolution has not been lowered
  yet; title GPU time is 43-63 ms; six traps added to `tasks.md`.
- **Docs:** `tasks.md`, `HANDOFF.md` and `REVIEW-PROMPT.md` aligned to the six-decision plan. README now
  carries the bar.
- **Next:** Build 0 (b), the per-draw translation microbenchmark. Owner: correct decision 5 in `plan.md`,
  and fill in the minimum spec.

### before 2026-11-27 — address space and the GPU↔CPU proof

```
[█░░░░░░░░░░░░░░░░░░░]   2 %   (estimated after the fact, same weights: Build 1 bookkeeping, 2.0)
```

- `crates/uma` page bookkeeping, 8 tests; `gputest` and `gputest --bare` proven on the RTX 4070 Ti.
