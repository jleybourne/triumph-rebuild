# Plan

The path from an empty tree to a title running at a steady frame rate, in milestones that each end in
something measurable. Nothing here is a promise of a number; each milestone states what it must prove.

## Ground rules

- **Measure before promoting.** A mechanism is "in" only when a run beats the previous one at the same
  draw count, reported as `fps · ms · draws · dropped`.
- **One change per measurement.** Batches cannot be attributed.
- **Proof before prose.** Every milestone below names the artefact that proves it — a test, a capture, or
  a run — not a document.

## M0 — The architecture, proven on hardware  *(in progress)*

The question this answers: *can this machine's GPU write into memory the CPU reads, with no copy?*

```
[x] address space bookkeeping: windows, 64 KiB pages, sharing by address, lifetime by user count (8 tests)
[ ] GPU proof: a compute shader writes into a custom-heap (WRITE_BACK, L0) buffer the CPU has mapped
[ ] tiled proof: a reserved 4 GiB buffer whose 64 KiB tiles map onto video memory, written by the GPU
```

Exit: `apps/gputest` reports the value the shader wrote, read straight from the CPU mapping, and a tiled
window accepts a placement and a write.

## M1 — A title's memory, not ours

Answer: *does the address space behave like the console's when a real title commits and places?*

```
[ ] reservations become windows; commits become tiles
[ ] write-watch on every committed page, chunks on an async ring behind a fence
[ ] 64-byte chunk uploads, hashed against what was last sent
[ ] placement at console addresses, sharing pages by address
[ ] bounded arenas for what cannot be tiled (unaligned, D32S8, MSAA), released after last user
```

Exit: a synthetic title that commits 2 GB, writes patterns from the CPU, and reads back from shaders is
byte-exact, with zero copies for GPU-owned ranges and chunk uploads for CPU-written ones.

## M2 — Something loads and runs

Answer: *can we execute a console binary far enough to see it complain intelligently?*

```
[ ] PE loader: relocations, imports by name and ordinal, delay-load, TLS callbacks, exception tables
[ ] kernel surface: threads, events, waits, timers, TLS, module queries
[ ] file system: console paths to the game folder, write overlay that never touches it
[ ] logging that survives a crash, with a backtrace
```

Exit: the title starts, loads its DLLs, opens its data and reaches its own code, with every missing call
named rather than crashing.

## M3 — Shaders

Answer: *can we translate the title's programs?*

```
[ ] RDNA2 decoding: VALU/SALU, LDS, scratch, images, samplers, control flow
[ ] wave64 -> wave32 rules, including the reductions that cannot reconverge
[ ] DXIL emission, validated by the platform compiler
[ ] a census: every program in the title's shader pack decoded, translated, or refused with a reason
```

Exit: a census with refusal reasons, not a percentage. The sibling layers reach ~100 %; that is the bar.

## M4 — A frame

Answer: *does the title's own renderer produce a picture through our translation?*

```
[ ] D3D12.X device, lists, PIPs, root signatures from the title's own state
[ ] descriptor heaps: console slots that shaders index, PC descriptors beside them
[ ] present, vsync, window and display plumbing
[ ] a screenshot of the title's own first frame
```

Exit: a capture of a rendered frame that is not black.

## M5 — Performance

Answer: *where does a frame actually go, and what do we remove?*

```
[ ] a frame line that reports: preparing, copies, copy-back, GPU busy, list calls, dropped, draws
[ ] per-pass card timing via timestamp queries
[ ] the upscaler route: the title's own FSR running natively, or a documented refusal
[ ] the arithmetic check: does the frame approach the card's own work?
```

Exit: a measured frame with the mirror gone, and a stated ceiling with the arithmetic behind it.

## What is deliberately out of scope

- **Instruction-level emulation.** The title is x86-64; it runs natively.
- **Copying either sibling layer's source.** Design is read; code is written here.
- **Multi-title support.** One title, one path, measured. Generality is a later problem.

## The honest expectation

```
memory model (M0-M1)      removes the 85 ms of copies and the 68 ms present wait
shader + frame (M2-M4)    what makes a picture exist at all
upscaler (M5)             the only thing that halves the title's own 45 ms of card work
```

The first gives roughly 10-12 fps. The last is what makes 30 possible. Any plan that claims 30 from
memory work alone is skipping the 45 ms the title spends drawing.
