# Tasks — the build, in order, with the number that closes each one

Every line ends in something measurable, not in "done". States: `done` · `doing` · `todo`.
Order and decisions are `plan.md`'s; overall progress is in [`progress.md`](progress.md).

## Proven

| state | work | number / evidence |
|---|---|---|
| done | address space: 4 GiB windows, 64 KiB pages, placement, sharing by address, lifetime by user count | 8 tests green |
| done | GPU writes into CPU-mapped memory (L0 custom heap), CPU reads it back — the GPU→CPU region of decision 1 | `RESULT the GPU wrote into memory the CPU reads, with no copy` |
| done | control: a dispatch with no bindings completes (never blame the machine) | `gputest --bare` |
| done | raw UAV rule: a `RWByteAddressBuffer` needs `R32_TYPELESS` + `RAW` or the dispatch removes the device | recorded, fixed |

## Build 0 — the three answers before more code

| state | work | closes when |
|---|---|---|
| done | (a) dirty-chunk upload benchmark: write-watch + 64 B diff (shadow and hash) + PCIe upload (copy-per-run and compute scatter) at 115 MB/frame | `uploadbench` — results in [`runs/build0a-upload.md`](runs/build0a-upload.md) |
| done | (b) per-draw translation microbenchmark: synthetic frame in the title's shape (4,500 draws, 20,311 calls, 1,433 pipelines) through our translator (`crates/xlist`, 5 tests) into real D3D12 lists, plus 12,027 descriptor writes from a dirty list | `listbench`: 0.43 µs a draw on one thread, 1.9 ms a frame; descriptors 2.0 ms — [`runs/build0b-lists.md`](runs/build0b-lists.md) |
| blocked | (c) the title's real GPU time from per-list timestamp pairs, RT on and off. Needs the title rendering on our code, so it closes in Build 5; until then the fork's 43-63 ms (RT off) stands | a measured replacement for the soft ~45 ms |
| todo | owner: fill the minimum spec in `plan.md` (GPUs, VRAM, ReBAR, output resolution, upscaler mode) | filled before Build 7 |

## Build 1 — tiled VRAM at console addresses  ✅ closed — [`runs/build1-tiles.md`](runs/build1-tiles.md)

| state | work | closes when |
|---|---|---|
| done | reserved 4 GiB buffer per window; 64 KiB tiles mapped onto 64 MiB heaps as pages commit; card pages reused after release | `gputest --tiled`: tile N written by a shader reads back N, in 2 windows |
| done | console addresses: a title pointer maps to the tile that covers it, and a shader reads it | CPU pattern → shader doubles it at another console address → CPU reads doubled |
| done | page sharing by address: two resources at one address see each other's writes | second buffer and a render target aliased onto the same card pages: writes and clears seen through the window |
| done | arena for untileable resources, released after last user | only stencil formats need it on this card; 10 GiB commit run, 0 wrong, no device removal |
| done | straddle refusals (window, arena) | unit tests: window straddle, arena partial overlap |
| done | upload destination = the tiled windows | `uploadbench --tiled`: no measurable penalty (clock-state noise dominates) |

## Build 2 — CPU-written memory on an async ring; the GPU→CPU region  ✅ closed — [`ring`](runs/build2-ring.md) · [`labels`](runs/build2-labels.md)

| state | work | closes when |
|---|---|---|
| done | 64-byte chunk diff and gather (`crates/uma/src/chunks.rs`) | measured in Build 0 (a); hash + scatter chosen |
| done | write-watch on the title's CPU-written regions, reset per flush (`ring::Watched`) | unit test: exactly the pages written; first touch of a fresh page counts (recorded) |
| done | upload path: hash diff + compute scatter, one dispatch per window, on a persistent pool | 34 MB / 342k runs a flush lands in 19.4 ms, verified byte for byte after 60 frames |
| done | async compute queue + fence; the title's queue waits on it; 3 staging slots | submit thread blocked 32 µs median, 72 µs worst |
| done | never copy inline: the submit thread is marked, and a copy of title memory on it panics | unit test, kept in every build |
| done | GPU→CPU region: the title's memory opened as a heap; labels written by the GPU (`WriteBufferImmediate`, MARKER_OUT) straight into it; CPU relay only as a fallback | 200/200 in order, never early; seen 65 µs before the fence (relay: 17 µs after) |
| todo | Build 5: per resource, direct GPU read of title memory over PCIe vs the ring (no diff, no upload) for rarely-written data | chosen by the frame line |

## Build 3 — loader and runtime

| state | work | closes when |
|---|---|---|
| done | PE loader (`crates/loader`, lane `loader`, PR #1): headers, sections, relocations, imports by name/ordinal, delay-load, exports, TLS, `.pdata`; map at base, bind through a resolver | 4,523 / 4,523 System32 PE32+ images parsed, 0 errors; 200k random corruptions, no panic; dbghelp.dll mapped beside the Windows loader: 9,912 relocations, 0 wrong |
| done | import census of the title (lane `loader`) | 18 images, 93 DLLs, 962 functions; the title's own DLLs supply 265; **GDK discovery list: 697 functions in 86 DLLs** (`agents/loader-imports.csv`); game folder hash unchanged |
| done | runtime census (`crates/runtime`, `runtimecensus`): **624 of 697 imports (89.5 %) bind straight to the host**; 34 overrides (files 15, modules 16, memory 2, process 1); 39 console-only in 7 DLLs | [`runs/build3-runtime-census.md`](runs/build3-runtime-census.md) |
| done | `xmem` placement (`crates/runtime/src/xmem.rs`): graphics memory never straddles a 4 GiB window (CPU address = GPU window offset), is write-watched for the ring, and is recorded per region with its real flags; 32-bit memory below 4 GiB | host tests: 8 MiB graphics allocation in one window, write-watch on; 32-bit below 4 GiB |
| done | physical pages (`crates/runtime/src/physical.rs`): frames are pagefile-section views placed into placeholders; remaps keep contents; aliases share bytes; **the GPU opens these views as heaps** (`gputest --labels`), so they are read in place | host test; GPU open measured |
| todo | the 13 `xmem` exports bound through `crates/runtime` (including `__XMemAllocationHooks__` from the title's exe) | each export unit-tested against the console meaning in the fork's notes |
| todo | overrides: files through `vfs` (15), modules with discovery logging (16), memory (2), process (1) | each with a unit test against the host's own behaviour |
| todo | console audio `acphal`/`xapu`/`chad` (19): helper lane `audio` · DirectStorage `xcrdapi` (1): helper lane `dstorage` | their lanes' closing tests |
| done | files (`crates/vfs`, lane `vfs`, PR #3): console paths → host, read-only package, write overlay, mods as ordered layers with conflicts | 18 tests; game folder hash unchanged (`v2_game_folder_unchanged`); 10,000-file open+read +3.3 µs over the OS floor |
| todo | wire the drives (decided): `G:` = the game package, read-only; the title's storage drive gets its own root; **`T:` starts empty every run** (as on the console) | the title opens its data through `vfs` |
| todo | GDK discovery loop: name every unimplemented call instead of faulting, **including every `GetProcAddress` miss** (no image imports `XGame*`/`XUser*` directly, so the title reaches them by name at run time) | a run lists what is missing; the list shrinks to zero |
| todo | the title's own DLLs: Bink video, its FSR module, GDK HTTP | each loads or is replaced with a stated stub |
| done | input (`crates/input`, lane `input`, PR #2): XInput pads + keyboard stand-in pad, menu isolation, the title's GameInput imports mapped | 16 tests |
| todo | audio (from the import census: the title's audio DLLs) | audio plays without stalling the frame |

## Build 4 — shaders: all 25,694, wave32, offline

The fork (jleybourne/Triumph) translates **25,694 / 25,694** programs, but 7 pipelines are still refused
because they want 64-lane waves. The rebuild keeps the same corpus and the same 100 % bar, and raises it in
three ways:

```
census line, per program   translated · validated (DXC validator) · pipeline built · wave32 · refused (reason)
fork today                 25,694 translated · 7 pipelines refused (wave 64) · ~16.5 % wave64 divergence overhead
rebuild target             25,694 translated AND validated AND built, 0 refused, every program wave32
```

The fork's translations are the **oracle**. They are data, not code. For each program we diff our output's
behaviour against its output on the same inputs (the fork's `census` lists every program by hash). A
program counts as done only when it passes that diff.

**Decided (2026-11-27): the shader core is ours.** That covers the IR, the lowering and the passes, built around
lane state as data, direct window addressing, uniformity, native ops and wave32, because that is where the
fork's own analysis puts ~10-22 ms of GPU time. The container reader and GFX10 decoder may be vendored
from the fork's GPL-3.0-or-later `victory-shader` with credit; they carry no performance. See `plan.md`
decision 4.

| state | work | closes when |
|---|---|---|
| done | decide: our own core; vendor only the decode front end, with credit | recorded in `plan.md` decision 4 |
| done | design: [`docs/design/shader-ir.md`](docs/design/shader-ir.md): classify → uniformity → lower → HLSL → DXC 1.8 offline (SM 6.6, wave32, 16-bit) | DXC accepts `[WaveSize(32)]`, wave/quad intrinsics, `float16_t` |
| done | the core's IR (`crates/sir`): uniformity, the wave-size classifier, the lane-doubling model (EXEC as data) | 100,000 random programs bit-exact against a 64-lane reference; the fork's refusal case exact |
| done | live-mask analysis: divergent branches with cross-lane ops become predication; host branches elsewhere | 100k branched programs bit-exact; on the card 601 runs exact, 182 host branches + 124 predicated |
| done | memory lowering: console address = window (descriptor) + 32-bit offset; uniform windows via `WaveReadLaneFirst`; native `float16_t` packed maths | on the card: 342 loads + 331 stores through tiled windows and 650 half ops exact, every word of memory compared |
| done | HLSL emission + DXC, run on the GPU and compared with the interpreter; the per-lane `WaveReadLaneAt` check | `sirtest`: 300 runs (100 agnostic + 200 doubled) exact on every lane; per-lane `WaveReadLaneAt` exact — [`runs/build4-sirtest.md`](runs/build4-sirtest.md) |
| todo | census tool: every program in `shaders.rpf` by hash, with stage, size and wave64 use; the census line above | `census` prints 25,694 rows; progress.md shows the shader bar |
| todo | RDNA2 (GFX10) decode: VALU, SALU, LDS, scratch, images, samplers, control flow | corpus decodes; byte-exact re-encode |
| todo | wave64 → wave32 lowering for every vendor; reductions that cannot reconverge are named | the 7 wave-64 pipelines build; divergence tests pass |
| todo | DXIL emit, validated by the platform validator | N programs compile, 0 validator errors |
| todo | root signatures always set `SAMPLER_HEAP_DIRECTLY_INDEXED` (fork trap: 311 refusals) | 0 refusals from root signatures |
| todo | oracle diff against the fork's translation, per program | 25,694 / 25,694 match or differ with a named reason |
| todo | pipelines built into a hash-keyed library at start (fork: 8,169 pipelines) | 0 runtime compiles; library build time on the frame line |

## Build 5 — first frame

| state | work | closes when |
|---|---|---|
| todo | D3D12.X device, lists, PSOs, root signatures from title state | creation path completes |
| todo | console descriptor heaps (T#/V# slots shaders index), PC descriptors beside, dirty list (no scan) | a shader samples a texture made by title code |
| todo | present / vsync / window | a non-black frame, captured to PNG |
| todo | frame line: fps · ms · draws · dropped · upload MB · upload ms · translation ms · output res · upscaler/mode · vendor | one parseable line a second |
| todo | Build 0 (c), moved here: the title's GPU time from per-list timestamp pairs, RT on and off | measured; replaces the ~45 ms everywhere it is quoted |

## Build 6 — parallel list translation

| state | work | closes when |
|---|---|---|
| done | translator core: state elision, address and pipeline mapping, no locks (`crates/xlist`) | 0.43 µs a draw with the driver, synthetic frame (Build 0 (b)) |
| todo | translate each list on the title's own recording thread at Close, no extra pool | per-draw CPU cost on the real stream ≤ 1 µs (Build 0 (b) floor ~0.4 µs), frame line |
| dropped | list cache by content | Build 0 (b): translation is ~2 ms a frame; hashing could not pay |

## Build 7 — the upscaler: one interface, two backends

| state | work | closes when |
|---|---|---|
| todo | find the title's temporal upscale pass and its inputs **by bindings** — colour, depth (2560x1440), motion vectors (2560x1440 R16G16_FLOAT), jitter and vector scale from its FSR constant block. Method 0x218 is pipeline creation, not an upscale call (fork `ee0b94a3`, local branch `dlss-hook`); plan.md decision 5 needs correcting | inputs logged and visualised for one frame |
| todo | a way to lower the internal resolution: resizing the title's targets from the layer made it assert while loading (fork `docs/research/resolution.md`) | internal resolution below 2560x1440 with the title running |
| todo | DLSS through NVIDIA Streamline | correct picture on the RTX 4070 Ti; internal resolution drops |
| todo | FSR through the FidelityFX SDK (driver-upgradable to FSR 4) | FSR 3.1 correct on the RTX 4070 Ti beside DLSS |
| todo | fidelity check against a native-resolution frame, both backends | capture comparison: no black, no smearing, UI stable |

## Build 8 — pacing

| state | work | closes when |
|---|---|---|
| todo | submit-ahead and fence waits that never stall the title | frame time ≈ the card's own work |
| todo | 30 fps on NVIDIA at the stated output resolution and DLSS mode | the frame line, at ~4,500 draws |

## Build 9 — AMD validation by testers

| state | work | closes when |
|---|---|---|
| todo | tester build: frame line + crash log written to a file, one command to run | a tester returns both without help |
| todo | AMD cards at and above the minimum spec, FSR backend | 30 fps on the frame line from AMD hardware |

## After 30 — the stretch (60 displayed)

| state | work | closes when |
|---|---|---|
| todo | frame generation (DLSS FG / FSR FG through the same upscaler inputs) | two presented frames per rendered frame, UI does not wobble |

## Launcher track — settings, features, mods, in-game menu (parallel; does not move the fps number)

Design: [`docs/design/launcher.md`](docs/design/launcher.md). Its own bar in `progress.md`.

| state | work | closes when |
|---|---|---|
| done | C1 `crates/config`: the settings registry (18 options in 8 groups), `rebuild.toml` load/save, validation, refused values with reasons, stable-only profiles | 6 unit tests |
| todo | C2 the layer reads `rebuild.toml` at start; every switch lives in the registry (lead) | benches and proofs take options from it |
| todo | L1 launcher shell (egui): tabs, Features generated from the registry, profiles (helper lane `launcher`) | writes a `rebuild.toml` that `config` validates with 0 problems |
| todo | L2 setup and system check: GPU, driver, tiled tier, SM 6.6, DXC, VRAM, ReBAR (lane `launcher`) | each check has pass/fail and a fix line |
| todo | L3 title settings applied to a copy of `settings.xml` in the write overlay (lane `launcher`) | the original's hash is unchanged |
| todo | L4 mods tab: enable, order, conflicts (lane `launcher`) using `crates/vfs::mods` (merged) | conflicts listed; order respected |
| todo | L5 in-game menu on the present path, input isolated with `crates/input::menu` (lead, after Build 5). **Input is polled on its own thread** (a poll costs ~170 µs with focus); the present path reads the last state | live options toggle; isolation test passes; the present thread does no polling |

## Known traps, from the fork (jleybourne/Triumph) — each must be designed out, not rediscovered

| affects | trap | rule for the rebuild |
|---|---|---|
| 4, 5 | root signatures without `SAMPLER_HEAP_DIRECTLY_INDEXED` beside static samplers: 311 pipelines refused | always set it; count refusals by cause |
| 3, 4, 8 | the title's hang watchdog fires after 30 s without a frame heartbeat (the "crash" at title+0x23cf683) — e.g. building ~2,370 pipelines at world entry | nothing blocks the title's threads for seconds; pipelines are built before the world |
| 8 | device removal: a capture buffer freed while its copy was pending | the layer's own submissions keep their resources until their fence |
| 2 | GPU→CPU visibility done approximately → null reads in title code | labels and polled values are exact and ordered; fences signalled when the title's Signal says, not a frame late |
| 5, 8 | dropped draws: fork docs disagree (4 compute pipelines vs the RT builder stage) | report dropped by cause, never as one inherited number |
| after 30 | the title has its own 30 fps limiter (menus run at exactly 33-34 ms) | find and lift it before any 60 attempt |

## Rules that keep the number honest

- A step is closed by its number on the frame line, not by its code compiling.
- One change per measurement; batches cannot be attributed.
- Never report fps without draws, dropped, output resolution and upscaler mode beside it.
- If a run shows a device removal, close it with the control (`gputest --bare`) before blaming anything.
