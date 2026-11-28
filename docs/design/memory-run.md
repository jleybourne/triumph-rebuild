# Design: the memory model in the title run (plan decision 1, Build 5)

Status: **in progress (lane `memmodel`, branch `agent/memmodel`).** Builds on Build 1 (tiles,
`runs/build1-tiles.md`) and Build 2 (the ring, `runs/build2-ring.md`). Switch: `REBUILD_MEMORY=standin` keeps
the old path (one 1 GiB UPLOAD heap + `titlemem` opened in place, no window views).

## The contract (with the shader lane)

```text
sub-window   k = address >> 29                      512 MiB of console address space
SRV          view heap index WINDOW_HEAP_BASE + (k & 32767)       raw, 2^27 R32 elements, byte 0 = k << 29
UAV          view heap index WINDOW_UAV_BASE  + (k & 32767)       raw, the same bytes (writes)
offset       address & (512 MiB - 1)
WINDOW_HEAP_BASE = 967,232 (= 1,000,000 - 32,768)    WINDOW_UAV_BASE = 934,464 (= WINDOW_HEAP_BASE - 32,768)
```

A raw view tops out at 2^27 elements, so the unit is 512 MiB. The UAV's slot ("the alternate" of the window
SRV) is a fixed offset, not a shadow-table lookup: no extra load per access. The 32,768 slots come from the top of
the alternate range, which becomes `[CONSOLE_VIEWS + NULL_BANK, WINDOW_UAV_BASE)` (184,440 alternates). Addresses
at or past 16 TiB wrap onto lower sub-windows; nothing of ours is placed there (XMem graphics memory is below
1 TiB unless the title asks otherwise, and placement past 16 TiB is refused and counted).

## The GPU side: one reserved buffer per sub-window

Each sub-window the title touches is **one reserved (tiled) buffer of 512 MiB**, created on first use. 512 MiB,
not 4 GiB, because that is what a view covers anyway, and because WARP cannot reserve 4 GiB (it can 512 MiB),
so the debug-layer path is the same code. Creating it writes its SRV and UAV into every shader-visible view heap
(and into each heap created later); a sub-window never used keeps the null view (reads 0).

`uma::Uma` keeps the page bookkeeping (now sparse: page tables per 4 GiB window, allocated on first use),
`uma::tiles::Tiles` maps 64 KiB pages onto card pages in 64 MiB heaps. All tile mapping, zeroing and uploads run
on **one queue, the ring's**, so the title's queue waiting for the ring's fence also orders every mapping before
any use. A card page reused from the free list is zeroed on that queue before anything else touches it (CPU
memory is zero on commit; the GPU copy must match).

## Which memory goes where

| memory | CPU side | GPU side | kept current by |
|:--|:--|:--|:--|
| XMem graphics memory (write-watched reservations: `XMemAlloc*`, `XMemVirtualAlloc`, GPU-bit `VirtualAlloc`) | the title's, as now | tiles at the same address, placed **on the first CPU write the ring finds** (or under a resource placed there), retired on decommit (whole 64 KiB pages only) and release | the ring |
| d3d12x resources, DEFAULT / not CPU-visible (render targets, depth, UAV buffers, textures' memory) | address reserved, nothing committed | tiles | nothing: GPU-owned, never copied |
| d3d12x resources, UPLOAD / CPU-visible custom | write-watched, committed at the address | tiles | the ring |
| d3d12x resources, READBACK | committed at the address, opened as a D3D12 heap | that heap (GPU writes over the bus, CPU reads) | nothing: the GPU→CPU region |
| physical pages (placeholder views) | pagefile views, not watchable | `titlemem` (opened in place), as now | nothing (left: see below) |
| labels, GPU timestamps | the title's | written into the CPU copy (`titlemem`), never the tiles | nothing |

d3d12x resources get console addresses from an **arena** of our own reservations above the title's range (from
1 TiB, 512 MiB pieces): CPU-visible ones in write-watched pieces (one ring region each), GPU-only ones in plain
reserved pieces (no ring work, no hashes). `Memory::buffer` is a reserved buffer aliased onto the same card pages
(sharing by address, as Build 1 proved); `Memory::texture` stays a committed texture for now (GPU-owned VRAM
either way; sharing a texture's bytes by address is later work).

**Lazy placement.** The title has ~4.5 GiB of watched graphics memory committed by device creation (5.2 GiB at
30 s) and writes far less of it. Placing card pages at commit put 4.9 GiB of tiles on the card, and the title's
next large texture failed (`E_OUTOFMEMORY`, then its assert at `exe+0x131a2ad`). Now a page gets a card page when
the ring is about to send CPU bytes into it (a `Backer` hook, before the pass records) or when a GPU resource
covers it; a page nobody wrote reads zero through an unmapped tile, as the CPU's fresh memory does. Measured:
896 MiB of tiles instead of 4.9 GiB. The cost: a shader that writes by raw address into graphics memory that no
CPU write and no resource ever touched writes into an unmapped tile (dropped). `REBUILD_MEMORY=eager` places at
commit for bisecting that. Card budget: `REBUILD_TILE_MIB` (6 GiB on a card, 2 GiB on WARP, whose tiles are the
process's own memory under the run's 16 GiB job limit).

## CPU writes reach VRAM: the ring in the run

- `submit::execute` calls `memory::flush()` once per `ExecuteCommandLists` and queues `Wait(ring fence, V)` on
  the title's host queue: a channel send, no copy, no lock on the title's data (Build 2's 32 µs median).
- The coordinator **coalesces**: flushes that arrive while it works are served by one pass and one Signal of the
  highest value (a fence is monotonic). The ring stays 3 slots of 96 MiB, hash diff, 4 workers, one scatter per
  sub-window.
- Regions are **dynamic**: a watched reservation registers when it is made (its write-watch state is not reset:
  what the title wrote before the first flush goes up). Hashes are per 64 KiB block, allocated on first use, so a
  large reservation that is mostly uncommitted costs nothing.
- Release and decommit take the ring's region lock (write), so the coordinator never reads memory being freed;
  pages carried to the next flush that fall in the freed range are dropped.

## What replaces `titlemem`

`Memory::range(va)` (every buffer view, VBV/IBV, root view, copy) answers in this order: a watched XMem region or
an arena piece → its sub-window buffer (clipped to the sub-window; lists cache it); a READBACK allocation → its
opened heap; anything else → `titlemem` (physical pages, plain host memory). A new `Memory::visible(va)` /
`ListHost::cpu_memory(va)` answers labels and timestamp resolves from the CPU copy (`titlemem`), so the title's
polls see them.

## Cost

Submit thread: one channel send and one queue `Wait` per `ExecuteCommandLists`. Per draw: nothing new (views are
placed once per sub-window). Load time: one `UpdateTileMappings` per commit or resource. Workers: the Build 2
diff (~13-17 ms of 4 workers at the title's volume) off every title thread.

## Left after this lane

- Physical pages through window views: they cannot be write-watched and opened heaps cannot be tile-mapped
  (device removed). Needs a dirty source (DirectStorage completions are known; CPU writes need a guard-page
  watch) and then the ring. Root views and VBVs over them still work through `titlemem`.
- GPU writes into CPU-written memory are not copied back (the decision's rule); a title that reads back GPU
  results from plain graphics memory (not a label, not a READBACK heap) sees stale bytes.
- Textures at console addresses on tiles (`Memory::texture`).
