# triumph-remux

**A self rebuild of the compatibility layer for Xbox Series, in my own method, focused on performance.**

A console-to-Windows layer for a current-generation title, built around one decision that the layers it
learned from make differently: **the title's memory is the GPU's memory.**

```
crates/uma        the address space: windows, 64 KiB pages, placement, sharing
apps/remux     the launcher
apps/gputest      the proof: the GPU writes, the CPU reads, nothing is copied
```

## The decision

A console has one memory. A PC has two. Layers usually bridge that by mirroring: keep a host copy, copy
into the card before the GPU reads, copy back after it writes. That is always correct and it costs a
per-frame copy in *and* a copy back — in the layer this project replaces those were 31-48 ms and 48-54 ms
of a 131 ms frame, at 5.5 µs per range of which only ~1 µs was bytes.

triumph-remux does the opposite. It gives the title addresses in sparse windows and backs them with card
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
```

## What is not built yet

```
the PE loader and the console runtime   months of work, and the layer cannot boot anything without it
the shader recompiler (RDNA2 -> DXIL)   the long pole in any codebase
the D3D12.X device, lists and heaps     the translation surface the title calls into
```

## The rules this project keeps

- **Measure, then promote.** A switch goes into a launcher only after a run that beat the baseline at the
  same draw count. `fps · ms · draws · dropped`, always together.
- **One change per run.** Batches cannot be attributed and have already cost a night.
- **No claiming a mechanism works until a test or a run shows it.** The tests in `crates/uma` exist for
  that reason, and the GPU proof is a binary rather than a paragraph.
- **Read other layers for design, never for code.** The trees this learned from are different licences.
