# Progress

```
work toward 30 fps   [█████████░░░░░░░░░░░]  43 %
shaders (census)     [░░░░░░░░░░░░░░░░░░░░]   0 / 25,694   (fork reference: 25,694 translated, 7 pipelines refused)
runtime functions    [█████████░░░░░░░░░░░]  32 / 73 ours built (xmem, modules, memory, process); 624 more bind to the host directly
launcher track       [███░░░░░░░░░░░░░░░░░]  1 / 7 steps (C1 settings registry; the mods tab's file system is merged); does not count toward the fps %
measured fps         none yet — the title has not rendered a frame on this code
baseline to beat     7.6 fps · 131.3 ms · ~4,500 draws · 356 dropped   (the layer being replaced)
target               30 fps · 33.3 ms · same draws · stated output resolution and upscaler mode
updated              2026-11-27
```

The percentage is **work done, weighted by build step**. It is not fps. It cannot say how close the frame
is to 33.3 ms until Build 5 produces a frame to measure. From then on, the frame line goes in the
second row and is the number that counts.

## By build step

| # | build | weight | done | earned | state |
|---|---|---:|---:|---:|---|
| 0 | three answers before more code | 10 | 70 % | 7.0 | (a) upload **done**; (b) translation **done**: 0.43 µs a draw; (c) blocked until the title renders (moved to Build 5) |
| 1 | tiled VRAM at console addresses | 10 | 100 % | 10.0 | **closed**: tiles, console addresses, sharing (buffer and render target), arena, 10 GiB scale, upload onto tiles |
| 2 | CPU-written memory on an async ring; GPU→CPU region | 8 | 100 % | 8.0 | **closed**: the ring (submit thread 32 µs, upload 19 ms, verified) and GPU-written labels into title memory |
| 3 | PE loader + console runtime | 15 | 75 % | 11.25 | loader (PR #1), files (PR #3), input (PR #2) merged; runtime census (89.5 % host); `xmem` complete; module, memory and process overrides built (32 / 73 ours); files, audio, DirectStorage in lanes |
| 4 | shader recompiler RDNA2 → DXIL, wave32 | 15 | 25 % | 3.75 | IR, classifier, lane model, live masks, memory through the windows, native half precision, HLSL → DXC: all proven on the card; decoder in lane `shader` |
| 5 | D3D12.X device, lists, descriptors, present | 12 | 0 % | 0 | first frame |
| 6 | list translation | 8 | 10 % | 0.8 | translator core written and measured (5 tests); list cache dropped by measurement |
| 7 | upscaler: DLSS + FSR | 10 | 20 % | 2.0 | DLSS standalone through Streamline on a synthetic scene (PR #4); FSR waits for the SDK; wiring to the title's pass todo |
| 8 | frame pacing | 6 | 0 % | 0 | 30 fps on NVIDIA closes here |
| 9 | AMD validation by testers | 6 | 0 % | 0 | 30 fps on AMD closes here |
| | **total** | **100** | | **42.8 ≈ 43 %** | |

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
