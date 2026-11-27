# triumph-rebuild

**A self rebuild of the compatibility layer for Xbox Series, in my own method, focused on performance.**

[**Progress: `[██░░░░░░░░░░░░░░░░░░]` 8 %**](progress.md) — work toward 30 fps, by build step · no title frame measured yet

A console-to-Windows layer for a current-generation title, built around one decision that the layers it
learned from make differently: **GPU-owned memory lives in VRAM at the title's addresses and is never
copied back; only what the CPU really changed goes up, in 64-byte chunks, off the submit thread.**

```
crates/uma        the address space: windows, 64 KiB pages, placement, sharing
apps/rebuild     the launcher
apps/gputest      the proof: the GPU writes, the CPU reads, nothing is copied
apps/uploadbench  Build 0 (a): write-watch + 64 B diff + upload, timed at the old layer's volume
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
no per-frame descriptor scans — plus two things neither of them does: command lists cached by content and
replayed, and descriptor updates tracked as a dirty list instead of a 203,127-slot scan.

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
crates/uma unit tests   8 tests green: page-per-page backing, sharing by address, lifetime by user
                        count, refusal without partial commit, window-relative addresses, covering page

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
