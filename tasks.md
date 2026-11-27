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

## Build 1 — tiled VRAM at console addresses

| state | work | closes when |
|---|---|---|
| doing | reserved 4 GiB buffer per window; 64 KiB tiles mapped onto a VRAM heap as pages commit | `gputest --tiled` writes tile N from a shader and reads page N |
| todo | console addresses: a title pointer maps to the tile that covers it, and a shader reads it | synthetic title: shader reads a value at a console address through the tiles |
| todo | page sharing by address: two resources at one address see each other's writes | unit + GPU test |
| todo | arena for untileable resources (unaligned, D32S8, MSAA), released after last user | 10 GB commit run without a device removal |
| todo | straddle refusals (window, arena) | unit tests |
| todo | upload destination = the tiled windows (Build 0 (a) used a plain default-heap buffer) | `uploadbench` re-run against tiles, within 10 % of Build 0 (a) |

## Build 2 — CPU-written memory on an async ring; the GPU→CPU region

| state | work | closes when |
|---|---|---|
| doing | 64-byte chunk diff and gather (`crates/uma/src/chunks.rs`, 7 tests) | measured in Build 0 (a); method chosen from its table |
| todo | write-watch on the title's CPU-written regions, reset per frame | dirty-page count matches a known write pattern |
| todo | upload path chosen by Build 0 (a): hash diff + compute scatter on workers, a frame ahead (copy-per-run collapses: 146 ms to record 440k copies) | at the title's real shape (~226k ranges, ~31 MB): ≤ 30 ms worker wall, off the submit thread |
| todo | async queue + fence; the title's queue waits on it; persistent worker pool (no per-frame thread spawn) | submit thread blocked ~0 ms while the upload lands |
| todo | never copy inline: a run asserts the submit thread does no `memcpy` of title memory | assertion in a test build |
| todo | GPU→CPU region: fence labels, queries, bottom-of-pipe writes the title polls | title-polled labels advance; no hang after frame 2 |

## Build 3 — loader and runtime

| state | work | closes when |
|---|---|---|
| todo | PE loader: relocations, imports by name/ordinal, delay-load, TLS callbacks, exception tables | console DLL loads, exports resolve |
| todo | kernel: threads, events, waits, timers, TLS, module queries | title's startup path runs |
| todo | files: console paths → game folder, write overlay, never modify the dump | title opens its data; game folder hash unchanged |
| todo | GDK discovery loop: name every unimplemented call instead of faulting | a run lists what is missing; the list shrinks to zero |
| todo | the title's own DLLs: Bink video, its FSR module, GDK HTTP | each loads or is replaced with a stated stub |
| todo | input (controller + keyboard/mouse) and audio | title responds to input; audio plays without stalling the frame |

## Build 4 — shaders

| state | work | closes when |
|---|---|---|
| todo | RDNA2 decode: VALU, SALU, LDS, scratch, images, samplers, control flow | corpus of real programs decodes byte-exact against reference dumps |
| todo | wave64 → wave32 lowering for every vendor, reductions that cannot reconverge named | divergence tests; refusals carry reasons |
| todo | DXIL emit, validated by the platform compiler | N programs compile, 0 errors |
| todo | census of the whole shader pack; pipelines into a hash-keyed library at start | 100 % translated or refused-with-reason; 0 runtime compiles |

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
| todo | find the title's temporal upscale pass and its inputs **by bindings** — colour, depth (2560x1440), motion vectors (2560x1440 R16G16_FLOAT), jitter and vector scale from its FSR constant block. Method 0x218 is pipeline creation, not an upscale call (fork `ee0b94a3`); plan.md decision 5 needs correcting | inputs logged and visualised for one frame |
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
