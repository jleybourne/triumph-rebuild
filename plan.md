# Plan — a full rebuild of the layer, 30 fps on supported hardware, DLSS on NVIDIA, FSR on AMD

The target is **30 fps in-world on every supported PC**, measured, at a stated output resolution and
upscaler mode — then more. The build must be **better than both layers it learns from on every line of
the frame line**: fps, ms, dropped, picture. Both repos are the reference for every build step — we read
their source to learn how each part works, then write our own. We do not build or run their current
builds; we measure only ours, against the baseline already recorded from victory. Supported means: an NVIDIA or AMD GPU at or above the minimum spec below.
Intel GPUs are not supported.

## What this is

A rebuild of the whole Xbox-Series-to-Windows layer in Rust: loader, console runtime, memory, shader
recompiler, D3D12.X device, lists, descriptors, present, upscaler.

**Code is ours.** It is written fresh from the *design* of `xcl` and `fxceless-victory` — how they work,
what the title calls, what broke — not translated from their files line by line. A line-by-line port is
still a copy (and `xcl` is GPL-2.0; this workspace is GPL-3.0-only). Read, understand, close the file, write.

## The target, stated so it can be checked

```
baseline   131.3 ms · 7.6 fps · ~4,500 draws · 356 dropped     (fxceless-victory, dev machine)
target     33.3 ms · 30 fps · same draw count · stated output resolution and upscaler mode
           on the minimum spec, on NVIDIA (DLSS) and on AMD (FSR)
stretch    60 displayed
```

A "30 fps" with no resolution and no upscaler mode beside it is not a result.

### Minimum spec — owner decision, to be filled before Build 7

```
GPU        NVIDIA  ____________      AMD  ____________      (Intel GPUs: unsupported)
VRAM       ___ GB
PCIe       3.0 x16 assumed as the floor (the dev machine: i7-10700KF, RTX 4070 Ti, PCIe 3.0)
ReBAR      required / not required: ___
output     ____ x ____, upscaler mode ________
```

The dev machine's PCIe 3.0 link (~15.8 GB/s) is the floor for every memory-path number in this plan.

### Hardware we can and cannot test

```
can test     NVIDIA (RTX 4070 Ti) — DLSS, and FSR 3.1 (vendor-agnostic) on the same card
cannot test  AMD — FSR 4, AMD drivers, AMD memory/tiling behaviour, AMD device removals
```

Everything that can be proven on NVIDIA is proven there first. AMD is closed by tester runs (Build 9).

## The six decisions

### 1. GPU-owned memory lives in VRAM; CPU-written memory is uploaded as dirty chunks

Corrected from the previous plan, which said "one memory, no mirror, ever". On a discrete GPU that is not
physically possible: CPU-mappable memory is either system RAM (`MEMORY_POOL_L0`, GPU reaches it over
PCIe) or BAR-mapped VRAM (CPU reads are very slow; GPU upload heaps measured 2× slower here). Tiled
VRAM cannot be CPU-mapped at all.

So the model is three regions, each with one rule:

```
GPU-owned      render targets, depth, UAV-written buffers, textures after first upload
               → tiled VRAM (reserved resources, 64 KiB tiles at console addresses, shared by address)
               → never copied back
CPU-written    the title's CPU-side data the GPU reads
               → host memory with write-watch; changed 64-byte chunks uploaded on an async copy
                 queue behind a fence; the submit thread never copies
GPU→CPU        fence labels, query results, anything the title's CPU polls or reads after the GPU
               → a small explicit CPU-visible region (L0, write-back); measured, named, bounded
```

What this removes, honestly: the per-frame copy-back of GPU-owned data, inline copies on the submit
thread, and full-range uploads. What it keeps: an upload of whatever the CPU really changed. Its cost is
measured in Build 0, before anything else is built on it.

### 2. Descriptors live where the shaders read them

Console slots are real memory in the title's address space, PC descriptors beside them, with a **dirty
list** — never a per-frame scan (the old layer examined 203,127 slots to mirror 12,027 records).

### 3. Command lists: recorded, translated in parallel, cached where it pays

The CPU budget at the target, if serial: **33.3 ms / 4,500 draws ≈ 7.4 µs per draw**, and
**33.3 ms / 20,293 list calls ≈ 1.6 µs per call**. The old layer measured 0.029 ms (29 µs) per draw.
That gap — about 4× — is the CPU half of the problem and is measured as its own number.

Lists are recorded as data and translated on worker threads (leave cores for the title — 8C/16T on
the floor machine). Caching identical lists is tried, not assumed: console lists reference per-frame
ring offsets, so the hit rate may be low and hashing has a cost. It ships only if the hit rate pays.

### 4. Nothing compiles during a frame; shaders are wave32 everywhere

RDNA2 → DXIL offline, verified by census. Pipelines built into a hash-keyed library at start.
**Wave64 is lowered to wave32 for every vendor**, including AMD (which could run wave64 natively): one
shader path, fully testable on NVIDIA. A native-wave64 AMD path is a later option, never the default.

### 5. One upscaler interface, two backends

The title already calls its own FSR (console device method 0x218). We capture that call's inputs once —
colour, depth, motion vectors, jitter, exposure, reset, render and output size — into one interface:

```
NVIDIA   → DLSS, through NVIDIA Streamline
AMD      → FSR, through AMD's FidelityFX SDK, integrated so AMD's driver upgrades it to
           FSR 4 where the card supports it (RDNA4; INT8 FSR 4.1 on RDNA3, RDNA2 announced)
           and FSR 3.1 otherwise
```

The inputs are where upscalers break (wrong jitter sign, wrong motion-vector scale, missing reset), and
they are identical for both. **FSR 3.1 runs on NVIDIA**, so the FSR backend and the inputs are verified on
the dev card side by side with DLSS. That covers most of the AMD risk without an AMD card.

### 6. Every number comes from the frame line

```
fps · ms · draws · dropped · upload MB · upload ms · translation ms · output res · upscaler/mode · vendor
```

## Order of work — each step ends in a number

| # | build | done when |
|---|---|---|
| 0 | **the three answers before more code**, all on our own code: (a) dirty-chunk upload benchmark at the old layer's write volume; (b) per-draw translation microbenchmark; (c) the title's real GPU time from per-pass timestamps | (a) write-watch + 64 B diff + PCIe 3.0 upload of 83-115 MB/frame timed; (b) µs per draw vs the 7.4 µs budget; (c) a measured replacement for the soft ~45 ms |
| 1 | tiled VRAM at console addresses, page sharing, arena for untileable resources | a shader reads and writes tiles at console addresses; two resources at one address see each other |
| 2 | write-watch + dirty 64-byte chunks on an async ring; GPU→CPU region | submit thread blocked ~0 ms; title-polled labels work |
| 3 | PE loader + console runtime (threads, events, files, overlay FS) | the title's own code runs to its first render call |
| 4 | shader recompiler RDNA2 → DXIL, wave32, census | every program translated or refused with a reason |
| 5 | D3D12.X device, lists, descriptor heaps, present | a non-black frame from the title |
| 6 | parallel list translation (+ cache if it pays) | per-draw CPU cost on the critical thread ≤ 7.4 µs |
| 7 | upscaler interface + DLSS + FSR backends | DLSS and FSR 3.1 both correct on NVIDIA; internal resolution drops |
| 8 | frame pacing: submit-ahead, fences, no stalls | frame time ≈ the card's own work |
| 9 | AMD validation by testers | frame line + crash log from AMD cards at/above minimum spec; 30 fps there |

Build 0 decides whether the memory model in decision 1 is worth building. If dirty uploads at real
volume do not fit comfortably inside a frame on PCIe 3.0, decision 1 changes before Build 1 starts.

## What we do not build

- No copy-back of GPU-owned resources, and no full-range mirror as a fallback — a fallback becomes the
  path that ships. CPU-written data is uploaded as dirty chunks only.
- No guard-page write detection; write-watch only.
- No runtime pipeline compilation.
- No per-frame descriptor scan.
- No Intel GPU support, no XeSS.
- No native-wave64 path until the wave32 path is complete and measured.

## Acceptance, measured identically for every step

```
report    the frame line above
baseline  131.3 ms / 7.6 fps at ~4,500 draws
target    33.3 ms / 30 fps at the same draw count, stated resolution and upscaler mode,
          on NVIDIA with DLSS and on AMD with FSR, on the minimum spec
```

A step is done when its number is on that line, not when its code compiles.
