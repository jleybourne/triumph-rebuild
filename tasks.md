# Tasks — the build, in order, with the number that closes each one

Every line ends in something measurable, not in "done". States: `done` · `doing` · `todo`.

## Proven

| state | work | number / evidence |
|---|---|---|
| done | address space: 4 GiB windows, 64 KiB pages, placement, sharing by address, lifetime by user count | 8 tests green |
| done | GPU writes into CPU-mapped memory, CPU reads it back | `RESULT the GPU wrote into memory the CPU reads, with no copy` — cpu read `[CA,FA,BE,BA,…,2A,…]` |
| done | control: a dispatch with no bindings completes (never blame the machine) | `gputest --bare` |
| done | raw UAV rule, learned the hard way: a `RWByteAddressBuffer` needs `R32_TYPELESS` + `RAW` or the dispatch removes the device | recorded, fixed |

## Build 1 — sparse windows and tiled placement

| state | work | closes when |
|---|---|---|
| doing | reserved 4 GiB buffer per window; 64 KiB tiles mapped onto a VRAM heap as pages commit | `gputest --tiled` writes tile N from a shader and reads page N |
| todo | console addresses: a title pointer maps to the page that covers it, and a shader reads it | synthetic title: CPU writes a pattern, shader doubles it, CPU reads the doubled value with no copy-back |
| todo | page sharing by address: two resources at one address see each other's writes | unit + GPU test |
| todo | arena fallback for untileable resources (unaligned, D32S8, MSAA), released after last user | 10 GB commit run without a device removal |
| todo | straddle refusals (window, arena) | unit tests |

## Build 2 — CPU writes on a ring

| state | work | closes when |
|---|---|---|
| todo | write-watch on committed pages, reset per frame | dirty-page count matches a known write pattern |
| todo | 64-byte chunk diff against last-sent hash | only changed chunks sent; GPU-authored bytes untouched |
| todo | async upload queue + fence; title queue waits on it | submit thread blocked ~0 ms while 10 MB of CPU bytes land |
| todo | never copy inline: a run asserts the submit thread does no `memcpy` of title memory | assertion in a test build |

## Build 3 — loader and runtime

| state | work | closes when |
|---|---|---|
| todo | PE loader: relocations, imports by name/ordinal, delay-load, TLS callbacks, exception tables | console DLL loads, exports resolve |
| todo | kernel: threads, events, waits, timers, TLS, module queries | title's startup path runs |
| todo | files: console paths → game folder, write overlay, never modify the dump | title opens its data; game folder hash unchanged |
| todo | name every unimplemented call instead of faulting | a run lists what is missing |

## Build 4 — shaders

| state | work | closes when |
|---|---|---|
| todo | RDNA2 decode: VALU, SALU, LDS, scratch, images, samplers, control flow | corpus of real programs decodes byte-exact against reference dumps |
| todo | wave64 → wave32 lowering, with the reductions that cannot reconverge named | divergence tests; refusals carry reasons |
| todo | DXIL emit, validated by the platform compiler | N programs compile, 0 errors |
| todo | census of the whole shader pack | 100 % translated or refused-with-reason |

## Build 5 — first frame

| state | work | closes when |
|---|---|---|
| todo | D3D12.X device, lists, PIPs, root signatures from title state | creation path completes |
| todo | console descriptor heaps: slots shaders index, PC descriptors beside, dirty list (no scan) | a shader samples a texture made by title code |
| todo | present / vsync / window | a non-black frame, captured to PNG |

## Build 6 — remove the last serial work

| state | work | closes when |
|---|---|---|
| todo | list calls recorded as data, translated on N threads | translation ms on the submit thread ≈ 0 |
| todo | identical lists cached by content hash and replayed | cache hit rate reported; frame time falls on repeat frames |

## Build 7 — the upscaler

| state | work | closes when |
|---|---|---|
| todo | the title's own FSR4 runs: device method 0x218, its descriptors, its targets | log shows a lower internal resolution with a correct picture |
| todo | verification: full-size fidelity check against a native-resolution frame | capture comparison, no black, no smearing |

## Build 8 — pacing

| state | work | closes when |
|---|---|---|
| todo | submit-ahead and fence waits that never stall the title | frame time ≈ card's own work |
| todo | frame line: fps · ms · draws · dropped · copies · copy-back · translation | one parseable line a second |

## Rules that keep the number honest

- A step is closed by its number on the frame line, not by its code compiling.
- One change per measurement; batches cannot be attributed.
- Never report fps without draws and dropped beside it.
- If a run shows a device removal, close it with the control (`gputest --bare`) before blaming anything.
