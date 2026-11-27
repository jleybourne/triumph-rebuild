# Lane `resources`: D3D12.X resources, heaps, descriptor heaps, views and the shadow table (Build 5)

Branch `agent/resources`. Code: `crates/d3d12x` (package `rebuild-d3d12x`, new). Design and numbers:
[`docs/design/d3d12x-resources.md`](../docs/design/d3d12x-resources.md). Nothing else changed but one line in the
root `Cargo.toml`. No game folder, no title runs; GPU only for the unit-level tests, with `agents\MEASURING` absent.

## Built

- **Objects** (`resource.rs`, `heap.rs`, `object.rs`, `slots.rs`): resource, heap and descriptor heap, console header
  at +0x00..+0x10 (vtable, bits 0, refcount), 32-slot tables. Implemented: resource 12 (IUnknown, private data,
  SetName, GetDevice, Map, Unmap, GetDesc, GetGPUVirtualAddress, GetHeapProperties), heap 8, descriptor heap 10.
  Every other slot traps: count, first-call time, `Events::first_trap` once, `E_NOTIMPL` only for `HRESULT`
  methods, 0 for value and pointer slots. `traps()` lists what was called.
- **Device slots served (27)**, as `Resources` methods and as `thunks::device_slots()`: 0x70, 0x78, 0x80, 0x88, 0x90,
  0x98, 0xa0, 0xa8, 0xb0, 0xb8, 0xc0, 0xc8, 0xd0, 0xd8, 0xe0, 0xe8, 0x130, 0x138, 0x148, 0x1e8, 0x230, 0x238, 0x250, 0x258, 0x360,
  0x3f8 (the back buffers), 0x428. Not served (trap in the lead's table): 0xf0, 0x260.
- **Memory** by the lead's `Memory` trait (console address = GPU address; committed memory from it; buffer views
  from its ranges); `HostMemory`, an upload-heap stand-in; footprints from lane `xg`.
- **Descriptors**: 32-byte records in the title's memory (buffers named whole; textures, samplers, RTV, DSV by a
  generation-checked table entry), mirrored into host heaps at the same index on create, copy and `sync`; raw SRV at
  every buffer descriptor's index, its own kind at an alternate; the shadow table `(stride, records, alternate,
  lead)`; a per-thread cache of buffer views (copied, not made).
- **`Host<P>`**: `ListHost` over `Resources` (resource, descriptor_heap, cpu_descriptor, memory) wrapping the lead's
  pipelines.

## Measured (RTX 4070 Ti)

| | |
|:--|--:|
| host limits | 1,000,000 views, 2,048 samplers (with static samplers) |
| CBV / raw SRV / typed SRV / raw UAV, created and mirrored | 249 / 210 / 277 / 235 ns (4.0 / 4.75 / 3.6 / 4.3 a µs) |
| byte-copied records synced; unchanged slots synced | 3.95 a µs; 97 a µs |
| the title's 1,245,184-view heap made | 36.7 ms |

Workspace: 392 passed, 0 failed, 11 ignored, 0 warnings. Crate tests: 25 (no GPU) + 3 on the card (`--ignored`): layouts, tables and traps; each view kind through its record and
back; the shadow table; a raw mirror for every buffer view; copies and syncs; the 1,245,184-view heap; the card
reads a buffer through views made by console calls (raw at the index with a lead, typed and CBV at alternates, a
byte-copied CBV) and every value is exact.

## Decided

- **Raw mirrors**: a raw SRV at every buffer descriptor's index (any buffer slot reads as a `ByteAddressBuffer`,
  S7-5), the descriptor's own kind at an alternate named by shadow word 2. Not a parallel plane (does not fit
  1,000,000 with the title's 1,245,184), not raw only for SRVs (a raw read of a CBV slot would be undefined).
- **Heap size**: indices kept; console indices `[0, 750,000)`, null slot 750,000, alternates after; writes past it
  counted and noted; shaders clamp to `view_index_limit()`.
- Struct returns accept both register orders (MSVC C++ `(this, out)`, the fork's `(out, this)`).

## For the lead

1. Confirm shadow words 2 (alternate) and 3 (lead), and the translator contract (design §6.1).
2. Confirm the heap strategy (design §4.2).
3. UAV clears of buffer descriptors need the alternate's GPU handle: `Resources::own_kind_gpu` (xlist maps the
   GPU handle to the primary, a raw SRV).
4. `D3D12_RESOURCE_DESC1` is 64 bytes, not the fork's 72 (0x360 with count > 1).
5. `Memory::buffer` needs a host buffer aliasing a 64 KiB-aligned console address (a reserved resource on the
   tiles); range resources must live for the run.

## Next

- Wire into the device (the lead's), then one run: the trap list, the struct-return order notes, `HeapStats` (highest
  index, beyond, misaligned, pool empty).
