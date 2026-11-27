# Build 1 — tiled VRAM at console addresses

2026-11-27 · RTX 4070 Ti (tiled resources tier 4), PCIe 3.0. Code: `crates/uma` (`lib.rs` page bookkeeping
with card-page reuse, `tiles.rs` the GPU side, `arena.rs`, `tileproof.rs`). 21 unit tests green.

## `gputest --tiled`

```
backend   2 reserved 4 GiB windows, 64 KiB tiles on 64 MiB heaps, tiled resources tier 4
PROVEN    a shader wrote tile N at console addresses in 2 windows; pages [3, 4, 5, 100, 65543] read back N
PROVEN    an unmapped page reads zero, and a write into it is dropped
PROVEN    CPU pattern at console address 3·64K, shader doubled it into 100·64K, CPU read the doubled values
PROVEN    a second placement at the same address maps no new card pages
PROVEN    two buffers at one address: what one writes, the other reads
PROVEN    a render target (4 tiles) at the same address as window pages: its clear is read through the window
PROVEN    retiring the last placement unmaps its pages; they read zero again
```

## `gputest --arena` — what cannot be tiled on this card

```
ARENA     D32_FLOAT_S8X24 depth-stencil
ARENA     D24_UNORM_S8 depth-stencil
tiles     D32_FLOAT depth
tiles     RGBA8 render target, 4x MSAA
tiles     RGBA8 render target
PROVEN    arena: 2 untileable kinds, each placed twice = 2 committed resources, all released after their last user
```

Only depth-stencil formats with stencil need the arena here. MSAA and plain depth tile. The arena
refuses a partial overlap rather than alias part of a resource (unit tests).

## `gputest --scale 10` — a title's commit pattern at scale

```
committed 10 GiB = 5120 placements of 2 MiB at random addresses in 4 windows: mapped in 1637 ms, 10240 MiB of tile heaps
PROVEN    5120 placements written by a shader and read back (847 ms): 0 wrong, no device removal
```

10 GiB of tile heaps on a 12 GiB card, with no device removal. Mapping costs ~0.32 ms per 2 MiB
placement (1.6 s for 10 GiB). That is a load-time cost, never a frame cost.

## `uploadbench --tiled` — Build 0 (a) against tiles

Same benchmark, with the destination as a tiled window instead of a committed buffer (raw lines in
[`build1-upload-tiled.txt`](build1-upload-tiled.txt)). The hash + scatter + 4-worker path, 50 % of chunks
changed, three runs each:

```
GPU ms      committed   10.00   18.79    9.90
            tiled        9.48    7.24   18.79
```

**No tile penalty is measurable.** Both destinations show the same two modes, ~7-10 ms and ~19 ms, which
is the card's clock state, not the tiles. Build 0 (a)'s verdict stands for tiles. Later GPU timings should
pin clocks (`nvidia-smi --lock-gpu-clocks`, administrator) or report both modes.
