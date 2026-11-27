# Build 2 — the GPU→CPU region: labels the title polls

`cargo run --release --bin gputest -- --labels` · 2026-11-27 · RTX 4070 Ti. Code: `crates/uma/src/labels.rs`
(`LabelMemory`). Raw output: [`build2-labels.txt`](build2-labels.txt).

## The design

A console title marks GPU progress with bottom-of-pipe writes into **its own memory** and spins on them. The
fork's trap was a CPU relay that signalled them up to a frame late. Here the title's memory is opened as a
D3D12 heap (`OpenExistingHeapFromAddress`), a buffer is placed over it, and each label is a
`WriteBufferImmediate` in `MARKER_OUT` mode: the GPU itself writes the value into title memory once every
earlier command has completed.

## The run

```
opens     plain VirtualAlloc, 64 KiB
opens     plain VirtualAlloc, 16 MiB
opens     write-watched VirtualAlloc, 64 KiB
work      each label follows a dispatch of ~1.15 ms; 200 labels per method
GPU label (MARKER_OUT into title memory): seen 64.6 µs before the fence median, 72.5 µs at p95, 25.1 µs least
CPU relay (worker waits on the fence, writes the label): 17.0 µs after the fence median, 39.8 µs p95, 42.4 µs worst
PROVEN    200/200 GPU labels arrived in order, after their work, never early
```

## Verdict

- **Labels are written by the GPU into title memory.** No CPU thread relays them, so they are exact and
  ~80 µs earlier than a relay: about 65 µs before the fence even reports completion. The CPU relay is
  kept only as a fallback for memory that cannot be opened.
- **Title memory of every kind tried opens**, write-watched memory included. Labels can live anywhere
  the title puts them.

## What this opens (measured later, not assumed)

Write-watched title memory can be a GPU heap. So for data the title writes rarely and the GPU reads once
(constants), the GPU could **read title memory directly over PCIe**, with no diff and no upload. The ring
stays for everything else. This is a Build 5 measurement: per-resource, direct read vs the ring, chosen by
the frame line.
