# Thoughts

Why this project exists, what the measurements said, and what I concluded. Written after a night and a
day of measured work on a previous layer, and after reading two other implementations of the same idea.

## 1. What the problem actually is

A console title is an x86-64 binary written against **one** memory: the CPU and GPU share it, and the
code assumes that anything the CPU writes is visible to a GPU pass, and anything a GPU pass writes is
visible to the CPU immediately after. That is the whole contract, and it is the only part that is
genuinely hard on a PC.

Everything else — the API surface, the shader format, the file system — is translation work. It is large
but it is bounded and well understood:

```
console GPU command lists  -> D3D12 command lists     translation
console GPU ISA (RDNA2)    -> DXIL                    offline recompilation
shared RAM                 -> discrete VRAM + PCIe     THE HARD PART
```

The CPU code runs natively. Nothing is emulated instruction by instruction. So when a frame is slow, it
is one of three things: our translation overhead, the title's own GPU work, or the memory model.

## 2. What the previous layer measured

```
frame                  131.3 ms · 7.6 fps median · ~4,500 draws
  preparing            85-110 ms  (copies 31-48 ms for 83-115 MB, textures ~6 ms)
  copying back         48-54 ms   (60-65 MB)
  GPU busy             ~71-90 ms  (~26 ms of it our own hashing, ~45 ms the title's)
  list calls           20,293 a frame = 243.2 ms on the submitting thread
  copies               5.5 us each, of which ~1 us is bytes
  descriptor sync      203,127 slots examined to mirror 12,027 records
  present wait         ~68 ms, while the submit thread copies ~72 MB up
```

Nineteen A/B experiments followed. The important ones:

| attempt | result |
|---|---|
| copy coalescing / merging runs (4 tries) | neutral; or +63-70 % bytes |
| double buffering | refused from code: nothing to alternate |
| barrier de-duplication | ceiling ~18 ms; the sibling fork measured "nothing to 0.1 ms each" |
| kernel-bypass copies (`rep movsb` + fault handler) | no gain, and runs degraded 141 -> 467 ms |
| precompiled pipelines from the title's containers | 0.115 ms/draw vs 0.029 — 4x worse |
| GPU upload heaps | 2x slower, -1,370 draws |
| VRS enabled in the title's settings | zero shading-rate calls; inert |
| FSR quality 4 | black screen |
| guard epochs, present tickets, ordered copies, wide prepare | neutral (ratio 1.00) |
| wave64 shader translation | all divergence is 16.5 % of 4.64 M instructions; removing all of it = 13.2 fps ceiling |

**The conclusion that matters:** every cheap lever was already spent, and the expensive costs are all one
thing — the mirror. Copies cost because there are 11,542 of them, not because of bytes. Copy-back costs
because it exists at all. The 68 ms present wait is the title waiting for that work.

## 3. Why language and codebase quality are not the answer

The reviewer I asked to attack this agreed with the structure and disagreed with two numbers, but not
this: a per-range copy, a per-call translation and a PCIe transfer cost the same in Rust, C++, Zig or
anything else. The one place language mattered was the ~26 ms of hashing, where SIMD work could save
8-13 ms.

A greenfield rewrite therefore does not raise the ceiling by re-typing the same ideas. It raises it only
if it makes the *architectural* choices up front that a retrofit has to fight: memory placement at
allocation time, and command lists recorded and translated in parallel.

## 4. The design that removes the cost instead of tuning it

The sibling layer at `D:\Developer\xcl` (GPL-2.0, read for design only) demonstrates it, and its README
states the result on a retail title: **30 fps steady, FSR 4 running a whole frame, 26,698 of 26,698
shaders translating, title graphics memory shared by CPU and GPU without copies.**

Its mechanisms, in order of what they remove:

1. **Sparse windows.** The title's reserved-and-committed memory gets addresses in a unified block; on the
   GPU each window is a reserved (tiled) 4 GiB buffer whose 64 KiB tiles map onto video memory as the
   title commits pages. A GPU pass writing a heightmap writes into the title's address space. There is no
   readback because nothing was ever elsewhere.
2. **Write-watch + 64-byte chunk uploads.** The CPU's view stays the title's memory. What the CPU wrote is
   noticed with write-watch, hashed, and only changed 64-byte chunks are sent on an async ring behind a
   fence. Chunking matters for a correctness reason too: the rest of a page may hold what the GPU wrote
   (counters, indirect arguments) and must not be overwritten with the CPU's stale view.
3. **Tiled placement for GPU-owned resources.** Targets, depth buffers and UAV textures are placed at the
   title's addresses as reserved (tiled) resources, one 64 KiB card page per 64 KiB address page, shared
   by every resource that covers it. Untileable cases (unaligned, D32S8, MSAA) use bounded arenas that are
   released after the last user — after a naive heap-per-range attempt turned 3.5 GB of addresses into
   15 GB of arenas in two seconds and removed the device.

## 5. The honest arithmetic, including what it does not fix

```
card busy per frame         ~71 ms
  our hashing              ~26 ms   -> removed by 1 and 2
  the title's own work     ~45 ms   -> NOT removable by any memory work
copy + copy-back           85 ms   -> removed for placed surfaces, chunked for the rest
```

So memory work alone reaches roughly **10-12 fps**. 30 fps needs the title's 45 ms of GPU work reduced as
well, which means fewer shaded pixels — the title's own upscaler, which the sibling layer runs
successfully and the previous layer could not.

**Two limits, two different fixes. This project takes the first one seriously, and leaves room for the
second.**

## 6. Why build our own rather than fork

Three reasons, in order of weight:

1. **Placement is decided at allocation.** Retrofitting it into a layer that hands out plain pages has
   already been tried and placed **0 regions**. A new tree gets the address space right on day one.
2. **Ownership.** The design is understood well enough to write it ourselves, and a layer with one
   consistent memory model is easier to reason about than one with a mirror plus five optimisations that
   fight it.
3. **Licence hygiene.** The two layers this learned from are GPL-2.0 and GPL-3.0. Reading their design and
   reimplementing avoids a provenance problem for anyone who ever contributes.

## 7. What "done" means for the first milestone

Not "the game runs". The first milestone is a GPU proof that the architecture works on this machine:

> the GPU writes into a buffer the CPU has mapped; the CPU reads those bytes with no copy, no staging
> resource and no readback queue.

If that passes, the memory model is proven before a single console shader is translated, and every later
milestone stands on something real instead of a plan.

## 8. The traps already paid for

- Never overwrite the bytes of a page the GPU may have written; send chunks, not pages.
- Arenas for untileable resources must be bounded and released, or the device falls over.
- A range straddling two windows or two arenas is a bug generator; pages avoid it.
- A live game process makes the next launch exit instantly and the log parser reads a stale file. Three
  false conclusions in one night came from that.
- An unmeasured switch in a launcher cannot be attributed when a run regresses. One change per run.
