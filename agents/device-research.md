# Lane `device-research`: the D3D12.X device (Build 5 design input)

Branch `agent/device-research`. Documents and data only; no code, no title run, no GPU work, game folder
untouched.

## Built

- [`docs/research/d3d12x-device.md`](../docs/research/d3d12x-device.md): creation parameters and IID, the
  device and child tables, inline object layouts, the startup order to the first Present, the fork's
  pitfalls, and a recommended minimal first set.
- [`agents/d3d12x-device-slots.csv`](d3d12x-device-slots.csv): all 160 device slots (offset, PC equivalent or
  console, args, which the title calls, first set or trap, fork source).
- [`agents/d3d12x-child-slots.csv`](d3d12x-child-slots.csv): queue, list, allocator, fence, heap, resource,
  descriptor heap and DXGI slots, with **this title's offsets and the 2020 offsets** side by side.
- [`agents/d3d12x-object-layouts.csv`](d3d12x-object-layouts.csv): every field the title reads inline.

## Measured (from the fork's committed run logs, `diagnostics/timing-run-2026-11-25.txt`)

- **0 `method not implemented` lines** in either log (startup to the world, and 295 s of play). The fork's
  wired slots, plus 0x1c8 and 0x218 (which fail on purpose), are the full set this title calls.
- Device made 0.82 s after the image is mapped; **first Present 1.23 s later** (frame 1, 0 draws, black by
  design); draws start at frame 2.
- Before the first Present: device 0x1c8 is called ≥ 8,192 times and 0x218 ≥ 32 times, in a 4 ms burst
  (0x1c8 passes 16,384 calls by 2.2 s). The frame interval is set to 33.333 ms twice.
- The title's shader-visible view heap holds 1,245,184 descriptors. The host refused that size, and the
  fork capped it at 1,000,000.

## Decided (recommendations; the lead decides)

- Every unwritten slot is a trap that logs offset, caller, **first-call time and count**. One run then
  gives the exact startup order, which the fork's INFO logs do not have.
- Command-list put pointer and all four limits at 0 (every command a call into `crates/xlist`). Our list
  state goes past +0x960.
- Device byte +0x18 = 1. Choose the child tables by the creation IID (`ID3D12Device11` means the newer
  layouts, +0x10 from queue 0x70 and list 0x1d8).
- 0x1c8 and 0x218: `E_NOTIMPL` with `*out = null` first. Decoding them later fits `sir`, because their
  descriptions carry GPU-code addresses and sizes.

## For the lead

1. `crates/runtime/src/exports.rs` `D3D12ConfigureVirtualMemoryX` returns 0 without filling the `lowest` /
   `highest` range (the struct is 0x20 bytes, outputs at +0x10 and +0x18, fork `device.rs:185-230`). The
   title may lay out its GPU addresses from that range. Fill it to match Build 1's windows.
2. When `D3D12XboxCreateDevice` lands, log the IID text and the first 16 words of `params` once. The fork
   decodes only the version (0x20004); ring and scratch sizes are unknown.
3. Pointer- or value-returning slots (list 0x4a0, `GetCPUDescriptorHandleForHeapStart`, DriverHintX,
   struct-return methods with a hidden first `*out`) must not share the HRESULT trap. The fork's 0x4a0
   `E_NOTIMPL` crashed the BVH builder.

## Next

Nothing in this lane. When Build 5 starts, the first-run trap log checks §4 and §6 of the doc.
