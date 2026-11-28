# Lane `vram3` — the world load under the card's budget

Branch `agent/vram3`, on master 620f8fe (vram #78, vram2 #81), then origin/master 5d15d84 merged. Card RTX 4070 Ti
12 GB, budget ~11.05 GiB. Starting point (vram2, run `v2-c`, `REBUILD_MAPPED_TEXTURES=1`): the world load ran with no
GPU hang and reached **10.8 of 11.05 GB**; frames stopped ~122 s, title fatal `exe+0x23cf683` (null write) ~139 s.

## Where v2-c's 10.8 GB went (vram2's census) and what each item can give

| item | MiB | what this lane does | expected |
|:--|--:|:--|--:|
| card pages under streamed textures: pool pages | ~2,640 | (the console's own cost) | 0 |
| ... host tiles with no console page ("private") | ~700 (11,190 tiles) | **derived tiles**: NULL while their mips' pages are unmapped, shared by textures of one layout over the same pool pages | measured: 367 |
| title-memory tiles | 1,394 | — | 0 |
| render targets in other memory | 3,005 (console 2,804, covered 2,804) | aliasing heaps (opt-in) | ~0: no two live ones overlap on the console |
| render targets in watched memory | 1,160 (console 1,119, covered 957) | aliasing heaps | up to ~160 |
| placed views, twins, rest | 782 + 322 + ~570 driver | — (census now says how much of the placed views lies over targets) | ? |

**The 3.0 GB of targets is not aliasing the host failed to honour**: vram2's census shows their console ranges cover
2,804 MiB, the sum of their sizes, so the console pays 2.8 GB for them too. Aliasing can only give back console
overlap that is live at once: ~160 MiB (watched memory) per the census. The census now prints the overlap across
origins per memory kind, and how many textures the GPU does not write lie over one it does (a view of a target's
memory with a host texture of its own: memory paid twice, and a view that does not see the target's writes).

## Built

- **Derived tiles** (`runtime::mapped`): a host tile with no console page of its own is keyed by the pool pages under
  the console pages of the mips it holds, the texture's start inside its page, its slot, and the texture's layout
  (console and host descriptions). NULL (no card memory) while none of those pages is mapped; textures of one
  layout over the same pool pages share it; it follows 0xf8 remaps of any of its pages. Replaces vram2's private
  tiles (a card page each for the texture's life).
- **Budget watcher, two levels** (`runtime::memory`): *near* (1 GiB left, off 1.5 GiB under): new sampled textures
  go to system memory (`rebuild_d3d12x::vram::NEAR`, also new small-texture heaps); *at* (384 MiB): as before,
  streamed textures take no new card pages. Woken by the adapter's budget-change event as well as 4x/s. GPU-written
  textures (and alias heaps) get `D3D12_RESIDENCY_PRIORITY_HIGH`, so past the budget the driver demotes sampled
  textures first. Counters on the vram line.
- **No failed texture reaches the title as a failure** (`d3d12x::resource`): when neither the card nor system memory
  takes a host texture, the title still gets its resource (null views; barriers and copies already skip a resource
  with no host one), logged and counted (`HOSTLESS`).
- **Aliasing heaps** (`d3d12x::alias`, **off unless `REBUILD_ALIAS_TARGETS=1`**): GPU-written textures at title
  addresses in heaps; one overlapping a live one on the console joins its heap at the console distance, when its
  host bytes fit and overlap there only textures it overlaps on the console.
- **Census across origins** per memory kind: overlap (what aliasing could give at most), and textures the GPU does
  not write that lie over one it does.

## Measured

WARP (`warp1`, debug layer, mapped on, aliasing on, A at 30 s: the world load ran on WARP): 1,191 targets placed,
104 joined a live one's heap: **3,912 MiB of heaps for 4,001 MiB of textures (89 MiB saved)**; debug layer #1422 x2
(a placed target drawn before any clear/discard/copy): placed targets start with undefined compression metadata.
So aliasing is opt-in. `warp2` (aliasing off): no #1422, no #1047, no device removal. nvlddmkm 453 -> 453 both.

Card (`v3-a`, mapped on, aliasing off, A at 40.7 s, 200 s, locks, nvlddmkm 453 -> 453 -> 453 two minutes after):

| | v2-c (vram2) | **v3-a** |
|:--|--:|--:|
| peak local / budget | 10,802 / 11,051 MiB (at the budget) | **10,421 / 11,051 MiB** (near, never at) |
| card pages under streamed textures | 3,340 MiB | 3,037 MiB |
| of them private / derived | 700 MiB (11,190 tiles) | **367 MiB** (5,875 keys for 11,126 derived tiles; 5,190 lookups shared, 5,030 NULL) |
| host textures | 5,430 MiB (placed views 782) | 5,151 MiB (placed views 521; 225 to system memory) |
| pool pages mapped | 2,816 MiB | 2,795 MiB |
| frames stop / title fatal | ~122 s / `exe+0x23cf683` ~139 s | ~102 s / `exe+0x23cf683` ~135 s |

**The fatal is not memory.** In v3-a it came with the card 630 MiB under its budget (pressure never on), no
texture refused, no mapping failure, and the title's memory at 7.1 of 27.2 GB: the same null write
(`exe+0x23cf683`, called from the render thread `exe+0x2215c35`) as in fix2, fix3, v2-c. The tile queue was idle
with every fence passed (issued = signalled = GPU past 19,237), so the frame stop is not the tile thread or the GPU
waiting on it. A lead for whoever takes it: the run report says host PSO creation at first use blocked the title's
threads **67,220 ms** over the run (507 made inline; v2-c 50,670 ms). When in the run is not recorded.

## Left / for the lead

1. The `exe+0x23cf683` fatal and the frame stop before it: not memory (above). PSO blocking is the first suspect.
2. Placed views over targets: the census finds 34 textures (211 MiB) in watched memory and 25 (50 MiB) in other
   memory that the GPU does not write lying over targets: separate host textures, so paid twice and blind to the
   target's writes. A view of a live target's memory could name the target's host texture (views lane).
3. Debug-layer #1047 (WARP, `warp1`, 5x): a reserved texture written on the title's queue while its tile mappings
   were in flight on the tile queue. The title queue waits on the tile fence on the GPU (`memory::flush_wait`), so
   the order holds; the layer does not see a cross-queue fence wait. Not seen in `warp2`.
4. Aliasing heaps: to turn on, a placed target needs a discard (an enhanced barrier from `UNDEFINED` with the
   discard flag) before the title's first use when it has a heap of its own; worth ~90 MiB per `warp1`.
