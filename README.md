# triumph-rebuild

**A self rebuild of the compatibility layer for Xbox Series, in my own method, focused on performance.**

[**Progress: `[██░░░░░░░░░░░░░░░░░░]` 11 %**](progress.md) — work toward 30 fps, by build step · no title frame measured yet

A console-to-Windows layer for a current-generation title, built around one decision that the layers it
learned from make differently: **GPU-owned memory lives in VRAM at the title's addresses and is never
copied back; only what the CPU really changed goes up, in 64-byte chunks, off the submit thread.**

## Why a rebuild

**The layer we have runs the title at 7.6 fps (131.3 ms a frame), and the cost is in its shape, not its tuning.**

```
where 131 ms goes     copies in 31-48 ms · copies back 48-54 ms · ~68 ms waiting in present
                      while the submit thread copies · 243 ms of list calls on the recording threads
what was tried        19 measured A/B experiments against that structure: copy merging (4 ways),
                      double buffering, barrier de-duplication, rep movsb, upload heaps, wider
                      prepare, ordered copies ... every one neutral or worse
what that means       the copies exist because the layer gives the title plain CPU memory and keeps
                      a mirror; making copies cheaper cannot remove them
```

The layer has to give the GPU the title's memory **from the first allocation**. In the old tree that is a
retrofit into the allocator, the guard bookkeeping and the shadows all at once, and its own attempt failed
for exactly that reason: "0 regions placed", because the title's heap arrives page by page
(`ROADBLOCK.md` in the fork). Changing the memory model changes everything built on it, so we rebuild
around the right model instead of retrofitting the old one.

**Why not take the sibling layer (xcl) as it is:** its licence is GPL-2.0 and ours is GPL-3.0-only, and copying
its structure would also copy its ceilings. We read both layers to learn *how* each part works and *what the
title does*, then write our own.

**Why each main choice was made:**

| choice | why | evidence |
|---|---|---|
| GPU-owned memory in VRAM at the title's addresses, never copied back | the copy-back is 48-54 ms of the old frame and exists only to fake shared memory | old layer's frame line; `gputest` proves the GPU and CPU can share bytes |
| CPU changes sent as 64-byte chunks, scattered by one compute dispatch, a frame ahead | the old layer copied 115 MB to deliver ~48 MB of real change; one copy per range costs more than the bytes | **measured here:** 16-30 ms of worker time at the title's real shape, 12.64 GB/s link; one copy per range = 146 ms ([`runs/build0a-upload.md`](runs/build0a-upload.md)) |
| list calls translated where the title records them, no cache | the old layer's 29 µs a draw looked like translation, but it was memory bookkeeping | **measured here:** 0.43 µs a draw with the driver, 1.9 ms a frame on one thread ([`runs/build0b-lists.md`](runs/build0b-lists.md)) |
| descriptors as a dirty list | the old layer scanned 203,127 slots to update 12,027 | **measured here:** 12,027 writes from a dirty list = 2.0 ms |
| nothing compiles in a frame | runtime pipeline compiles measured 4× slower per draw | old layer's runs |
| DLSS on NVIDIA, FSR on AMD | the title's own GPU work is 43-63 ms; 30 fps needs fewer pixels, not just less overhead | fork's per-list timestamps; how to lower the internal resolution is still open ([`progress.md`](progress.md)) |

**What is still unproven, said plainly:** rebuilding versus keeping the old loader and shader translator
and replacing only the memory model has not been costed, and no title frame has run on this code yet.

## The tree

```
crates/uma        the address space: windows, 64 KiB pages, placement, sharing; 64 B chunk diff
apps/rebuild     the launcher
apps/gputest      the proof: the GPU writes, the CPU reads, nothing is copied
crates/xlist      list calls as records; the lock-free translator into D3D12
apps/uploadbench  Build 0 (a): write-watch + 64 B diff + upload, timed at the old layer's volume
apps/listbench    Build 0 (b): per-draw translation into real D3D12 lists
```


## The goal, in one block

```
1  rebuild the compatibility layer                        our own tree, our own code
2  informed by the two layers it learns from              their mechanisms + our measured knowledge
3  performance designed in from the first line of code    never retrofitted, never tuned later

success    30+ fps in-world (33.3 ms), measured as fps · ms · draws · dropped
baseline    131.3 ms · 7.6 fps · ~4,500 draws   (the layer being replaced)
route       GPU-owned in VRAM, CPU changes as dirty chunks + parallel list translation + DLSS / FSR
```

**"Better than both" is specific, not a slogan.** Where they made a choice that costs frame time we take
the opposite one: no host mirror, no copy-back path, no guard-page faults, no runtime pipeline compiles,
no per-frame descriptor scans — plus what neither of them does: descriptor updates tracked as a dirty list
instead of a 203,127-slot scan.

## The decision

A console has one memory. A PC has two. Layers usually bridge that by mirroring: keep a host copy, copy
into the card before the GPU reads, copy back after it writes. That is always correct and it costs a
per-frame copy in *and* a copy back — in the layer this project replaces those were 31-48 ms and 48-54 ms
of a 131 ms frame, at 5.5 µs per range of which only ~1 µs was bytes.

triumph-rebuild does the opposite. It gives the title addresses in sparse windows and backs them with card
memory one 64 KiB page at a time, so:

- a resource the GPU owns is written where the GPU writes it — nothing comes back;
- two resources at one address share memory, as on the console;
- a page nothing covers costs nothing;
- the CPU writes only what it changed, in chunks, on an async ring behind a fence.

## What is proven so far

```
crates/uma unit tests   15 tests green: page-per-page backing, sharing by address, lifetime by user
                        count, refusal without partial commit, window-relative addresses, covering page;
                        64 B diff (shadow and hash), run joining, gather, chunk indices

apps/gputest            PROVEN on the development card (RTX 4070 Ti):
                          "the GPU wrote into memory the CPU reads, with no copy"
                          cpu read [CA, FA, BE, BA, 00, 00, 00, 00, 2A, 00, 00, 00]
                        a compute dispatch writes 0xBABEFACA and 42 into a custom-heap
                        (WRITE_BACK, L0) buffer the CPU has mapped; the CPU reads those
                        bytes straight out of the mapping. No staging, no readback.

apps/gputest --bare     control: a dispatch with no bindings completes, so a device
                        removal is never blamed on the machine

apps/uploadbench        Build 0 (a), runs/build0a-upload.md: 12.64 GB/s over PCIe 3.0;
                        hash diff + compute scatter on 4 workers: 34 ms at 108 MB all
                        changed, ~16-30 ms at the title's real shape; 40/40 verified
```

## What is not built yet

```
the PE loader and the console runtime   without it the layer cannot boot anything
the shader recompiler (RDNA2 -> DXIL)   the largest single piece of work
the D3D12.X device, lists and heaps     the translation surface the title calls into
console descriptor heaps                T#/V# slots the title's shaders index
the title's own DLLs                    Bink video, the FSR upscaler, GDK HTTP
```

## The rules this project keeps

- **Measure, then promote.** A switch goes into a launcher only after a run that beat the baseline at the
  same draw count. `fps · ms · draws · dropped`, always together.
- **One change per run.** Batches cannot be attributed and have already cost a night.
- **No claiming a mechanism works until a test or a run shows it.** The tests in `crates/uma` exist for
  that reason, and the GPU proof is a binary rather than a paragraph.
- **Read other layers for design, never for code.** The trees this learned from are different licences.
