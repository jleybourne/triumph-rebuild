# Plan — a speed-first rewrite, targeting 30+ fps in the baseline build

The target is not "a layer that works". It is **30+ fps in the baseline build**, measured in-world, and
then more. Everything below is chosen for that number and nothing else. Where the two layers this learns
from made a choice that costs frame time, we take the opposite choice.

Code is ours, in Rust, written from their design as the reference. No copying — not out of caution about
the licence, but because copying their structure would copy their ceilings, and the point is to be faster
than both.

## The frame we are building toward

```
baseline target     30 fps in-world  (33.3 ms)
stretch             60 displayed     (45+ fps native, then frame generation)
```

The layer this replaces sits at 131.3 ms / 7.6 fps and spends **85 ms of that on copies and copy-back
that exist only to fake shared memory**, plus **243 ms of list translation on the submitting thread** —
which is why the title waits ~68 ms in its own present call.

## The five decisions that produce the number

### 1. One memory. No mirror. Ever.

The title's addresses *are* GPU-visible addresses: 4 GiB sparse windows over the aperture, backed by
64 KiB card pages mapped as the title commits. Nothing is copied in or back, because nothing is ever in
two places.

```
removes    copies 31-48 ms · copy-back 48-54 ms · guard faults 13-69 ms · the 68 ms present wait
proven     apps/rebuild/gputest — GPU writes, CPU reads the same bytes, no staging, no readback
```

### 2. CPU writes are chunks on a ring, never inline

Write-watch notices what the CPU touched; only changed 64-byte chunks are sent, on an async upload queue
the title's queue waits on by fence. The submitting thread never copies. Chunking is also correctness:
the rest of a page may hold values the GPU wrote.

### 3. Descriptors live where the shaders read them

Console slots are real memory in the title's address space, in the layout its shaders compute against;
PC descriptors sit beside them. Updates keep a **dirty list** — the old layer scans **203,127 slots a
frame to mirror 12,027 records**.

### 4. Command lists: recorded, translated in parallel, then cached

Each list is written down as data and translated on N worker threads. Beyond that, **identical lists are
cached and replayed** — most frames repeat list shapes, and neither reference layer exploits it. That is
where the 243 ms goes, and the cache is our addition.

### 5. Nothing compiles during a frame

Shaders: RDNA2 → DXIL offline, verified by census. Pipelines: built into a library keyed by hash at
start. Zero runtime compiles, no container parsing mid-frame (measured 4× worse per draw when done at
runtime).

## Order of work — each step ends in a number

| # | build | done when |
|---|---|---|
| 0 | address space + GPU/CPU shared memory proof | GPU writes into CPU-mapped memory, no copy — **DONE** |
| 1 | sparse windows + tiled placement at console addresses | a shader reads and writes memory the CPU wrote, through tiles, no copy-back |
| 2 | write-watch + 64-byte chunk upload on a ring | CPU bytes reach the GPU with the submit thread blocked for ~0 |
| 3 | PE loader + console runtime (threads, events, files, overlay FS) | the title's own code runs to its first render call |
| 4 | shader recompiler RDNA2 → DXIL + census | every program: translated, or refused with a reason |
| 5 | D3D12.X device, lists, descriptor heaps, present | a non-black frame from the title |
| 6 | list cache + parallel translation | translation measurably off the critical path |
| 7 | the title's own FSR running natively | internal resolution drops, picture correct |
| 8 | frame pacing: submit-ahead, fences, no stalls | frame time ≈ the card's own work |

Steps 3-5 are large but mechanical, and there are two working implementations to read while writing them.
Steps 1, 2, 6 and 7 are the ones that decide whether a frame can ever be 33 ms.

## What we do not build

- No mirror, no host copies, no copy-back path — not even as a fallback, because a fallback becomes the
  path that ships.
- No guard-page write detection; write-watch only.
- No runtime pipeline compilation.
- No per-frame descriptor scan.
- No compatibility switches borrowed from the reference layers; those flags exist to work around
  ceilings we are not inheriting.

## Acceptance, measured identically for every step

```
report    fps · ms · draws · dropped · copies · copy-back · translation ms
baseline  131.3 ms / 7.6 fps at ~4,500 draws
target    33.3 ms / 30 fps at the same draw count, FSR doing the upscaling
```

A step is done when its number is on that line, not when its code compiles.
