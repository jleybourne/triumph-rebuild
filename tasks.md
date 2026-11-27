# Tasks

Actionable list. One line per task, with its state and the artefact that proves it when it is done.
States: `todo` · `doing` · `done` · `blocked` (with the blocker named).

## Proven so far

| state | task | proof |
|---|---|---|
| done | address space bookkeeping: windows, 64 KiB pages, placement, sharing by address, lifetime by user count | `cargo test` — 8 tests green |
| done | refusal without partial commit (no card memory left mid-placement) | `a_refusal_leaves_the_block_untouched` |
| done | project scaffold, naming, licence, workspace | repo builds |
| done | control: a bare dispatch with no bindings completes, so a device removal is never blamed on the machine | `gputest --bare` |
| done | a `RWByteAddressBuffer` needs a raw UAV (`R32_TYPELESS` + `RAW`); without it the dispatch removes the device | found by this proof, recorded in `thoughts.md` |

## M0 — the GPU proof

| state | task | proof wanted |
|---|---|---|
| done | compute shader writes into a custom-heap (WRITE_BACK, L0) buffer the CPU has mapped; CPU reads it back with no copy | `apps/gputest` prints `RESULT the GPU wrote into memory the CPU reads, with no copy` |
| done | root signature + compute PSO built from HLSL at runtime | same binary, no error path taken |
| done | fence wait (polled, 1 ms) that returns as soon as the queue signals | the proof completes in well under a second |
| todo | tiled proof: reserved 4 GiB buffer, 64 KiB tiles mapped onto a DEFAULT heap, GPU writes a known pattern per tile | `gputest --tiled` verifies tile N maps to page N |
| todo | report adapter, driver, ReBAR state and tile count in the proof | `gputest --info` |

## M1 — title memory

| state | task | proof wanted |
|---|---|---|
| todo | reservations become sparse windows; commits become tiles | synthetic title commits 2 GB, all readable by a shader |
| todo | write-watch on committed pages; `ResetWriteWatch` per frame | dirty count matches a known write pattern |
| todo | 64-byte chunk diff against last-sent hash | only changed chunks sent; unchanged bytes stay GPU-authored |
| todo | async upload ring + fence; title queue waits on it, CPU never stalls | frame line shows upload time off the submit thread |
| todo | placement at console addresses with page sharing | two resources at one address observe each other's writes |
| todo | bounded arenas for untileable resources, released after last user | no device removal over a 10 GB commit run |
| todo | page-straddle and window-straddle refusals | unit tests |

## M2 — loader and runtime

| state | task | proof wanted |
|---|---|---|
| todo | PE loader: relocations, imports by name/ordinal, delay-load, TLS callbacks, exception tables | a console DLL loads and its exports resolve |
| todo | kernel: threads, events, waits, timers, TLS, module queries | title's own code runs its startup path |
| todo | file system: console paths to game folder, write overlay | title opens its data files; game folder unchanged (hash check) |
| todo | crash logging with backtrace | a deliberate crash produces a readable trace |
| todo | name every unimplemented call instead of faulting | a run that lists what is missing |

## M3 — shaders

| state | task | proof wanted |
|---|---|---|
| todo | RDNA2 decode: VALU, SALU, LDS, scratch, images, samplers, control flow | decoder round-trips a corpus of real programs |
| todo | wave64 -> wave32 rules, including non-reconverging reductions | divergence tests; refusals named, not silent |
| todo | DXIL emission validated by the platform compiler | N programs compile with 0 errors |
| todo | census of the title's whole shader pack | every program: translated, or refused with a reason |

## M4 — first frame

| state | task | proof wanted |
|---|---|---|
| todo | D3D12.X device: slots, PIPs, root signatures from title state | creation path completes |
| todo | console descriptor heaps: slots shaders index, PC descriptors beside them | a shader reads a texture through a title-made descriptor |
| todo | present/vsync/window plumbing | window shows the title's own frame |
| todo | capture the first non-black frame | a PNG that is not black |

## M5 — performance

| state | task | proof wanted |
|---|---|---|
| todo | frame line: preparing, copies, copy-back, GPU busy, list calls, dropped, draws | one line per second, parseable |
| todo | per-pass card timing via timestamp queries | top passes by ms |
| todo | confirm the mirror is gone: copy-back line reads ~0 for placed surfaces | before/after frame line |
| todo | upscaler: the title's own FSR runs, or a documented refusal with the reason | either a lower internal resolution in the log, or the exact failing call |
| todo | state the ceiling with arithmetic | a number someone else can re-derive |

## Known traps (do not re-learn these)

| state | trap |
|---|---|
| done | a live game process makes the next launch exit instantly; the log parser then reads a stale file — three false conclusions came from this in one night |
| done | enabling several unmeasured switches at once cannot be attributed; the batch that did it crashed the title at startup |
| done | whole-page copies can overwrite GPU-authored bytes (counters, indirect args); send chunks |
| done | a heap per address range turned 3.5 GB into 15 GB of arenas and removed the device; page-map instead |
| done | precompiled pipelines built from containers at runtime cost 4x per draw versus the pipeline library |
| done | `rep movsb` copies with a write-only fault handler crash the title on read faults |
