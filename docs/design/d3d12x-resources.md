# D3D12.X resources, heaps, descriptor heaps and views (Build 5)

Lane `resources`, 2026-11-27. Code: `crates/d3d12x` (package `rebuild-d3d12x`). Inputs:
[`docs/research/d3d12x-device.md`](../research/d3d12x-device.md), the three `agents/d3d12x-*.csv` tables, plan
decisions 1, 5 and 9, `agents/shader.md` S7 item 5. The fork (`victory-gpu` `descriptor.rs`, `resource.rs`,
`sampler.rs`) was read for facts and layouts only; nothing is copied.

## 1. What the crate is

The lead's device (`crates/runtime`) owns the device object and its 160-slot table. For 24 of those slots it calls
into this crate; the crate hands the title its own objects (resources, heaps, descriptor heaps) and writes views
into the title's descriptor memory. The list translator (`crates/xlist`, `ListHost`) asks it what a console
resource, heap, CPU descriptor or address is on the host.

```text
Resources                     one per device; create functions per slot, lookups for the translator
thunks::device_slots()        extern "system" functions for the 24 slots, over thunks::install(Resources)
Host<P>                       ListHost: resources and descriptors from Resources, pipelines from the lead's P
Memory (trait, the lead's)    console address -> host memory (plan decision 1); HostMemory: an upload-heap stand-in
HostDevice (trait)            where host views, heaps, the shadow table and host textures are made;
                              DeviceHost is the real one, the unit tests use a recording stand-in
```

## 2. Objects as the title reads them

```text
every object  +0x00 vtable  +0x08 u32 bits (0)  +0x0C u32 refcount  +0x10 ours (context, private data, fields)
resource      0x40 Map  0x48 Unmap  0x50 GetDesc  0x58 GetGPUVirtualAddress  0x70 GetHeapProperties
heap          0x40 GetDesc
descriptor h. 0x40 GetDesc  0x48 / 0x50 Get{CPU,GPU}DescriptorHandleForHeapStart (an address, both)
all           0x00-0x20, 0x30 IUnknown and ID3D12Object (private data kept), 0x38 GetDevice (the lead's device QI)
```

Tables are 32 slots; every slot we do not implement is a trap (offset, first-call time, count; `Events::first_trap`
once), failing with its return kind: `E_NOTIMPL` only for `HRESULT` methods (0x28 SetPrivateDataInterface, resource
0x60/0x68/0x78, heap 0x48), **0 for value and pointer slots** (research §5.2). Implemented: resource 12 of 32, heap
8 of 32, descriptor heap 10 of 32.

**Struct returns, both register orders.** MSVC C++ passes `(this, out)` and returns `out`; the fork declared the
device-level ones `(out, this)` and its objects' `(this, out)`, and both ran. So: our objects find `this` by its
table pointer; the device thunks (0xc8, 0xd0, 0x360) by the device pointer the lead gives (`set_device`). The 8-byte
heap-start getters take a second argument as the caller's return slot only if it lies within 1 MiB of our stack
frame, else return the value in RAX (a C-style caller). The first title run tells which is real.

## 3. Memory (plan decision 1)

Every resource has console memory, so `GetGPUVirtualAddress` is a console address and the title can place, alias
and view it by address:

| slot | memory | host resource |
|:--|:--|:--|
| 0xd8 / 0x3f8 committed | `Memory::allocate(footprint)` (freed with it) | buffer: `Memory::buffer(address)`; texture: `Memory::texture(address)` or a committed host texture |
| 0xe0 heap | `Memory::allocate(size)` | none |
| 0xe8 placed | heap address + offset (holds the heap) | as committed |
| 0x428 at an address | the title's address | as committed |

Footprints are the console's (lane `xg`'s layouts: the title keeps them in its texture tables); copyable footprints
(0x130) are the host's, because the copies they describe run on the host. Host textures are typeless (their family),
so every view format the console allows works; views default to the resource's typed format (depth formats read as
their colour twin: D32 as R32_FLOAT). B5G5R5A1 render targets get RGBA8 (fork lesson 14); the console's 16/8 depth is
the host's 24/8.

`Memory::buffer` must return a host buffer whose byte 0 is the console address and which shares those bytes
(xlist passes the title's offsets with it). With Build 1's tiles that is a reserved buffer whose tiles map onto the
same card pages (`Tiles::alias`), so it needs a 64 KiB-aligned address; committed and placed buffers are 64 KiB
aligned. Textures do not alias title memory byte for byte (console tiling); their contents come through the lead's
upload and detile path.

## 4. Descriptors

**The title's heap memory stays the title's.** A placed heap (desc followed by `{address, size}` with
`size == count × increment`, the fork's test) keeps its descriptors where the title put them; other heaps get ours.
Increments: 32 / 16 / 128 / 128. Every create call writes a **record** at the handle:

```text
32 bytes  w0 'XD' | kind   w7 a check over w0-w6 (stray bytes never pass)
  buffers (CBV, buffer SRV/UAV, views of placed memory): named whole: format, GPU address of the first byte,
          elements, stride, flags. No table: the title makes ~14,000 CBVs a frame at new addresses.
  textures, RTV, DSV: a table entry and its generation (one entry per distinct view of a resource; freed, with a
          new generation, when the resource is released, so a stale record decodes to nothing)
  nulls: the D3D12 dimension
16 bytes  samplers: a table entry (one per distinct description)
```

The title copies records as bytes; a record means the same wherever it lands.

**Mirroring.** A slot is mirrored when a create call writes it, when `CopyDescriptors(Simple)` copies into it, and
when the lead calls `Resources::sync(start, len)` for bytes the title changed itself (write-watch over the heap's
pages: plan decision 2's dirty list). Each slot keeps a fingerprint of the bytes it was mirrored from, so a sync of
an unchanged slot is a read and a compare. Every host slot starts null, and zero bytes mirror to null, so a new heap
costs nothing per slot. Slots are independent: no lock is taken to mirror one (except the alternate pool's free
list on the oversized heap).

### 4.1 The shader-visible view heap

```text
host index    [0, mirrored)      console index i at host index i (SM 6.6 heap indexing keeps the title's order)
                                   buffers: a RAW SRV of exactly the descriptor's bytes (from a 16-byte aligned start)
                                   textures: the title's view
              null_index (P)     a null view; translated shaders clamp view indices to P
              (P, views)         ALTERNATES: a buffer descriptor's own kind (CBV, typed SRV, raw or typed UAV)
shadow table  console index i -> 16 bytes (stride, records, alternate index, lead), t0 space101
```

**Raw mirrors (S7-5): decided, every buffer descriptor's view at its index is a raw SRV.** Translated programs read
any buffer slot as a `ByteAddressBuffer` at the descriptor's index (`crates/sir/src/lift/res.rs`), including slots
whose index they compute at run time (6,915 of the oracle's programs), where the translator cannot know whether the
title put a CBV, a typed view or a UAV there. A raw SRV at the index makes every such read exact, at no shader
cost. The descriptor's own kind goes to an alternate index the shadow table names, so a typed load, a constant
buffer load or a store pays one more 16-byte load (already in cache when the program reads the shadow table for an
indexed load; uniform otherwise). Raw SRVs for SRVs only was rejected: a raw read of a CBV slot would be undefined
(S7-5's concern). A raw plane at a fixed offset was rejected: two planes of 1,245,184 do not fit the host's
1,000,000.

Shadow words (plan decision 9 had `(stride, records, 0, 0)`; words 2 and 3 are new, for the lead to confirm):

| word | meaning |
|:--|:--|
| 0 stride | structured: its stride; typed: the element's bytes; raw and CBV: 1 (the index is the byte) |
| 1 records | structured and typed: elements; raw and CBV: bytes |
| 2 alternate | host index of the descriptor's own kind; `P` (the null slot) when it has none (raw SRV, textures) |
| 3 lead | bytes from the raw view's 16-byte-aligned start to the descriptor's first byte (D3D12 requires raw views to start 16-byte aligned; console descriptors need not). Raw loads add it; 0 for every aligned descriptor |

A constant buffer not 256-byte aligned on the host keeps its exact raw view and gets a null CBV alternate (counted:
`HeapStats::misaligned`); a typed view not element-aligned likewise. The translator can fall back to raw loads.

**Alternate placement.** If the heap's count fits the alternate range, the alternate of index i is `P + 1 + i`
(no allocation). Otherwise (the title's heap) alternates come from a free list over `(P, views)`, owned per slot:
the title overwriting a slot means nothing in flight reads its old views (the console's own rule for descriptor
memory), so its alternate is reused at once.

### 4.2 Heap size: 1,245,184 views against the host's 1,000,000

The fork asked for 1,245,184, was refused, and capped silently at 1,000,000 (timing-run L322); a title index past
the cap read a null view, and on AMD (uncapped, 1,507,328) a garbage index of 16,777,088 removed the device.

Decided:

1. **The host's limit is asked, not assumed** (`Limits::probe`: `D3D12_OPTIONS19.MaxViewDescriptorHeapSize`, else a
   creation probe at 2^21, else the tier-2 1,000,000). This card (RTX 4070 Ti, driver 610.88): **1,000,000 views,
   2,048 samplers** (the limit with static samplers; 4,080 without).
2. **Indices are kept** (no remap: a remap costs every descriptor fetch an indirection). Three quarters of the heap
   are console indices: **P = 750,000**, above the 720,000 the fork saw the title use; the rest are alternates.
3. **Nothing past P is silent**: a write there is recorded in the title's memory, counted (`HeapStats::beyond`), the
   highest index kept (`HeapStats::highest`), and noted once through `Events::note`. `HeapBinding` covers only
   `[0, P)`, so the list translator refuses (counts) a table past it.
4. **Translated shaders clamp view indices to P** (`Resources::view_index_limit()`, fixed at device creation,
   before the first pipeline): a garbage index reads the null slot instead of removing the device.

If a run shows the title past 750,000, the split moves (P is one expression in `Limits::null_index`), or the
alternates shrink to the kinds that need them (they are only CBVs, typed views and UAVs of buffers).

### 4.3 Samplers, RTVs, DSVs, CPU-only heaps

Sampler heaps mirror index for index (`min(count, Limits::samplers)`); `CreateSamplerX` (27 console words) is
decoded by the fork's word positions. RTV and DSV heaps are CPU-only host heaps of the same count; `cpu_descriptor`
brings a slot up to date from its bytes before returning the host handle, so byte-copied render target records work.
CPU-only view heaps hold each descriptor's own kind (they are copy sources and UAV-clear CPU handles). A UAV clear's
CPU handle for a shader-visible slot is written into a small CPU-only ring.

## 5. Measured (RTX 4070 Ti, 2026-11-27, `cargo test -p rebuild-d3d12x --release -- --ignored --nocapture`)

| what | number |
|:--|--:|
| host limits | 1,000,000 views, 2,048 samplers (with static samplers; 4,080 without); P = 750,000 |
| the title's 1,245,184-view heap made (host heap null-filled, shadow table) | 36.7 ms, once |
| CBV created and mirrored (record, raw SRV, CBV alternate, shadow) | **249 ns = 4.0 a µs** |
| raw SRV created and mirrored | **210 ns = 4.75 a µs** |
| typed SRV created and mirrored (raw + typed alternate) | 277 ns = 3.6 a µs |
| raw UAV created and mirrored (raw + raw UAV alternate) | 235 ns = 4.3 a µs |
| records copied as bytes, then synced | 253 ns = 3.95 a µs |
| sync of unchanged slots | 10 ns = 97 a µs |
| host alone: `CreateShaderResourceView` (raw) / `CreateConstantBufferView` / `CopyDescriptorsSimple` / a shadow entry | 121 / 11 / 5-11 / 5.5 ns |

Buffer views come from a per-thread cache of recently made ones (CPU-only heap, 65,536 slots, direct-mapped, no
lock) and are copied in (5-11 ns) instead of made (121 ns): the title makes its views at ring addresses that recur.
That took the CBV from 334 ns to 249. At the fork's ~14,000 CBVs a frame: **~3.5 ms of CPU a frame, spread over the
title's recording threads** (against 33 ms). The shadow table lives in `D3D12_HEAP_TYPE_GPU_UPLOAD` memory where the
device has it (this card does): VRAM, written through the BAR.

The GPU test makes a buffer and its views through the console calls only (create, Map, write, `CreateSRV/CBV`, a
byte copy and a sync), binds what `Resources::binding` and `shadow_va` say, and reads back: a raw SRV from byte 36
(lead 4), a typed float4 view at its alternate, the shadow's (stride, records), and a byte-copied CBV both as a
constant buffer and raw. All exact.

## 6. For the lead

1. **Confirm the shadow words** (2: alternate index, 3: lead) and the translator contract: raw loads at index i add
   `sh.w`; typed loads, constant buffer loads and stores go through `sh.z`; view indices clamp to
   `view_index_limit()`. A program that both stores and loads one descriptor in a dispatch should load through the
   alternate UAV as well (an SRV read may not see the same dispatch's UAV writes on the host).
2. **Confirm the heap strategy** (§4.2): keep indices, P = 750,000, count and note past it, clamp in shaders.
3. **UAV clears**: `ClearUnorderedAccessView*`'s GPU handle must name a UAV; for a buffer descriptor the UAV is the
   alternate. `crates/xlist` maps the GPU handle through `HeapBinding` (the primary, a raw SRV). Use
   `Resources::own_kind_gpu(console handle)` for the GPU half (textures: the slot itself).
4. **`D3D12_RESOURCE_DESC1` is 64 bytes** (the sampler-feedback region sits in the plain description's tail
   padding at +52). The fork's 72-byte struct misreads the second of an array (0x360 with count > 1).
5. `Memory` is yours: `range` (largest ranges first: the translator caches the last), `allocate/free` for committed
   resources and heaps, `buffer` (a host buffer at a 64 KiB-aligned console address, sharing bytes: a reserved
   resource aliased with `Tiles::alias`), `texture` (optional). Resources a range names must live for the run (the
   view cache keys on them). `HostMemory` is a stand-in for wiring.
6. A released resource's host views stay in the host heap until the title rewrites those slots (as on the console:
   the title must not use them); its records decode to null from then on.
7. Not done: UAV counters (noted once if a title passes one), contents of placed textures (Build 5 detile),
   `CreateReservedResource` (0xf0, never seen) and `CreateCommittedResourceX` (0x260, Series S only).
