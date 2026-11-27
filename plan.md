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

### Minimum spec — owner decision (2026-11-27): the dev machine

```
CPU        Intel i7-10700KF (8 cores / 16 threads), 64 GB RAM, Windows 11
GPU        NVIDIA RTX 4070 Ti, 12 GB VRAM, driver 610.88      AMD: deferred (owner, 2026-11-27)
PCIe       3.0 x16 (~15.8 GB/s): the floor for every memory-path number
ReBAR      on (BAR1 16 GB): assumed present
output     3840 x 2160 (the dev machine's display, 239 Hz), DLSS Quality: 2560 x 1440 internal, the title's own
           internal resolution on the console, so the upscale costs no detail the console had
```

The target is 30 fps at that output and mode. Faster DLSS modes stay options in the registry (`upscaler.mode`).
Intel GPUs are unsupported.

The dev machine's PCIe 3.0 link (~15.8 GB/s) is the floor for every memory-path number in this plan.

### Hardware we can and cannot test

```
can test     NVIDIA (RTX 4070 Ti) — DLSS, and FSR 3.1 (vendor-agnostic) on the same card
cannot test  AMD — FSR 4, AMD drivers, AMD memory/tiling behaviour, AMD device removals
```

Everything that can be proven on NVIDIA is proven there first. AMD is closed by tester runs (Build 9), **deferred by the owner (2026-11-27)**: the work proceeds NVIDIA-first, and the FSR backend stays built and tested standalone.

## The nine decisions

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

### 3. Command lists: translated where the title records them, no cache (corrected by Build 0 (b))

The CPU budget at the target, if serial: **33.3 ms / 4,500 draws ≈ 7.4 µs per draw**. Build 0 (b) measured
our translator plus D3D12 at **0.43 µs a draw, 1.9 ms a frame on one thread**. The old layer's 29 µs a
draw was memory bookkeeping hung on every bind, which decision 1 removes. So:

- each list is translated on the thread that records it (the title's own job threads), at Close; no
  extra pool;
- **no list cache**: at ~2 ms a frame, hashing could not pay;
- the rule for Build 5: per-draw work stays near 0.5 µs, and anything adding more than 1 µs a draw must
  be measured and justified.

### 4. Nothing compiles during a frame; shaders are ours, wave32 everywhere

RDNA2 → DXIL offline, for all 25,694 programs. Pipelines are built into a hash-keyed library at start.
**Wave64 is lowered to wave32 for every vendor**: one shader path, fully testable on NVIDIA.

**The shader core is ours, because that is where GPU milliseconds are decided** (decided 2026-11-27).
The fork's own analysis puts ~10-22 ms of the title's 43-63 ms GPU frame in choices made at the core of
its translator. The core is designed from the first line around:

```
lane state     each lane's EXEC bit carried as data, with live-mask analysis   (divergence: 3-7 ms)
memory         console addresses are window GPU addresses (Build 1): no page
               directory load, no fault store per access                         (2-5 ms)
uniformity     wave-uniform values found and kept scalar; scalar work once       (3-6 ms)
native ops     16-bit maths, quad and wave intrinsics instead of emulation       (2-4 ms)
waves          32 lanes only; no pipeline asks for 64
```

Parts that carry no performance (the DXBC/DXIL container reader, the GFX10 instruction decoder) may be
vendored from the fork's GPL-3.0-or-later `victory-shader` with credit to its authors. The fork's
translations are the **oracle**: every program is checked against them. A program is done when it is
translated, validated, built as a pipeline, wave32, and matches the oracle.

### 5. One upscaler interface, two backends (corrected: the title's FSR is found, not called)

The previous premise was wrong. Console method 0x218 creates pipelines; it is not an upscale call. The title's exe
imports `amd_fidelityfx_loader_xbox.dll` statically (4 functions; lane `loader`'s census), so it is mapped at
start. But run as a Series X, the title never calls into it to upscale, and nothing imports the upscaler DLL.
Its FSR is ordinary compute dispatches (fork
commit `ee0b94a3`, local branch `dlss-hook`, not pushed). So the layer **recognises the title's temporal upscale pass by its bindings**: the
dispatch that writes a colour target at an upscale ratio of the render-size textures it reads. Its inputs
are what the passes before it read: colour, depth (2560x1440), motion vectors (2560x1440
R16G16_FLOAT), and jitter and vector scale from its FSR constant block. That pass is replaced by:

```
NVIDIA   → DLSS, through NVIDIA Streamline
AMD      → FSR, through AMD's FidelityFX SDK (driver-upgradable to FSR 4), FSR 3.1 otherwise
```

**The upscaler only pays if the internal resolution drops.** The title renders 2560x1440 at every
`fsrQuality`, and resizing its targets from the layer made it assert at load (fork). Lowering the
internal resolution is therefore its own task in Build 7. Our memory model helps here: render targets
are tiles at console addresses, so a smaller PC target can sit behind the title's address without the
title seeing its size change.

The inputs are where upscalers break (wrong jitter sign, wrong motion-vector scale, missing reset), and
they are identical for both. **FSR 3.1 runs on NVIDIA**, so both backends are verified on the dev card.

### 6. Every number comes from the frame line

```
fps · ms · draws · dropped · upload MB · upload ms · translation ms · output res · upscaler/mode · vendor
```

### 7. One settings registry; a launcher and an in-game menu built from it (owner request, 2026-11-27)

Every option is declared once in `crates/config`: type, range, default (the measured best), live or
restart, stable or experimental. The desktop launcher (settings, features, title settings, mods, profiles,
system check), the layer at start, and the in-game menu (F10, input isolated) are all generated from it.
Unknown keys are reported, and bad values are refused with their reason. This replaces the old layer's
untyped environment variables. Mods are folders layered by the overlay file system; the game folder is
never written. Design: [`docs/design/launcher.md`](docs/design/launcher.md).

### 8. Streamed textures: BCPack decoded once, then cached (owner-approved, 2026-11-27)

BCPack is the console's lossless repacking of BCn texture blocks. The fork's CPU decoder ran 33-66 MB/s a thread,
6-12x slower than zlib, so decoding on every load would make it the main CPU cost of streaming. The output is
deterministic, so **each texture is decoded once, by our own decoder on DirectStorage's worker threads, and the
result is cached on disk** (outside git). Later loads are plain reads at disk speed, straight into the
GPU-owned tiles of decision 1. The ~78 KB format tables are loaded at run time from a file outside git. A GPU
decoder for cache misses is built only if the frame line shows first-load stutter.

### 9. A descriptor shadow table for what host views do not carry (2026-11-27)

Shaders reach resources through heap indices (decision 5), so they cannot read the console descriptor's own
words. Indexed buffer loads need two of them: the stride and the record count (`buffer_load_* idxen`: address =
index x stride + offset, and an index at or past the count reads 0). **The runtime keeps a shadow table beside the
heap: 16 bytes per heap index (stride, records, 0, 0), written when it builds the host view from the console
descriptor, read by shaders at `register(t0, space101)`.** One 8-byte load per indexed access, exact console
semantics, no per-stride shader variants. Build 5 writes it.

## Order of work — each step ends in a number

| # | build | done when |
|---|---|---|
| 0 | **the three answers before more code**, all on our own code: (a) dirty-chunk upload benchmark ✅; (b) per-draw translation microbenchmark ✅; (c) the title's real GPU time (needs a frame: closes in Build 5) | (a) [runs/build0a-upload.md](runs/build0a-upload.md); (b) [runs/build0b-lists.md](runs/build0b-lists.md); (c) a measured replacement for the soft ~45 ms |
| 1 | tiled VRAM at console addresses, page sharing, arena for untileable resources | a shader reads and writes tiles at console addresses; two resources at one address see each other |
| 2 | write-watch + dirty 64-byte chunks on an async ring; GPU→CPU region | submit thread blocked ~0 ms; title-polled labels work |
| 3 | PE loader + console runtime (threads, events, files, overlay FS) | the title's own code runs to its first render call |
| 4 | shader recompiler RDNA2 → DXIL: our core, wave32, census against the fork's oracle | 25,694 translated, validated, built, matching the oracle; 0 refused |
| 5 | D3D12.X device, lists, descriptor heaps, present | a non-black frame from the title |
| 6 | list translation on the title's threads (no cache) | per-draw CPU cost ≤ 1 µs on the real stream |
| 7 | find the title's upscale pass by bindings; lower the internal resolution; DLSS + FSR backends | DLSS and FSR 3.1 both correct on NVIDIA; internal resolution below 2560x1440 |
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
