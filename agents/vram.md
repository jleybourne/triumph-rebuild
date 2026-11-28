# Lane `vram` — card memory while the world loads

Branch `agent/vram`. Built on master 136a9bc (detile #77 and setup #76 merged). Card: RTX 4070 Ti, 12 GB, budget
~11.0-11.2 GiB (`QueryVideoMemoryInfo`). Runs: `runtitle --run --seconds 160-240` with saves present, A at 45 s
(opening video, then the world load), `REBUILD_VRAM_LOG=1|5`, each run taking `agents\MEASURING` and `agents\CARD`
create-new after a 60 s quiet gap and releasing only its own; nvlddmkm counted before and after. Tests: 800 passed,
0 failed (`cargo test --release --no-fail-fast`, exit 0).

**Stopped at a GPU reset**: run `fix4` took nvlddmkm 449 → 451 (two events 153, 22:31:26 and 22:32:26). The path that
run tested (reserved textures over page-mapped memory) is **off by default** in this branch; see "Left".

## Measured: where the card memory goes (the cause)

New: `REBUILD_VRAM_LOG=<s>` prints a `vram` line (and every tenth line a census of every live host texture); the
run report prints it once at the end. Host textures are counted by origin at creation and release
(`rebuild_d3d12x::vram`, bytes from `GetResourceAllocationInfo`); card pages by region (`memory::pages_by_region`).

Baseline (`base1`, master + counters, end of the run at ~179 s, world loading):

| what | live | host MiB | console MiB |
|:--|--:|--:|--:|
| card (`QueryVideoMemoryInfo` local / budget) | | **24,742 / 10,877** (non-local 2,164) | |
| tiles (the memory model's card pages) | | 1,408 of heaps: title memory 1,303, arena GPU-only 63 | |
| committed textures (own console memory) | 2 | 63 | 63 |
| textures at an address (0x428) | 2,990 | 7,982 | 7,762 |
| placed views (0x230) | 44,338 | **14,795** | 13,044 |
| depth twins | 318 | 322 | |

Census (`census2`, same point; `covered` = the union of console ranges, `pool` = distinct pool pages under them now):

| origin, written by, memory | count | host MiB | console MiB | covered MiB | addresses | pool MiB |
|:--|--:|--:|--:|--:|--:|--:|
| placed view, CPU/copy, page-mapped (0xf8) | 46,182 | **15,773** | 14,071 | 4,577 | 29,989 | **1,102** |
| at address, CPU/copy, page-mapped | 1,914 | **3,936** | 3,961 | 3,961 | 1,914 | **426** |
| at address, GPU (targets), other memory | 514 | 3,005 | 2,804 | 2,804 | 514 | |
| at address, GPU, watched | 665 | 1,160 | 1,119 | 957 | 457 | |
| placed view, CPU/copy, other | 4,834 | 632 | 369 | 102 | 4,364 | |
| placed view / at address, CPU/copy, watched | 130 | 273 | | | | |

- **The suspected double payment is small**: console memory behind GPU-only committed textures is 63 MiB, and all
  tiles together 1.4 GB. Fixed anyway (textures and heaps no longer place card pages at allocation, `allocate_lazy`).
- **The cause is the title's texture streaming.** It reserves GPU addresses, makes textures over them (0x428 and
  0x230), and maps 64 KiB pages of one pool at those addresses as data streams in (0xf8). On the console a texture
  costs the pool pages mapped under it. Here each was a committed host texture of its full size, mapped or not, and a
  placed view's texture lived for the run (`placed` cache keyed by address and words): **19.7 GB of host textures
  over 1.5 GB of pool pages** (46,182 placed-view textures over 29,989 addresses).
- nvidia-smi: 6.7 GB on the menus, 11.3 → 11.9 GB of 12.3 GB once the load starts; local usage then runs past the
  budget (24.7 GB "in use": the driver demotes), and the run ends with `CreateCommittedResource3: E_OUTOFMEMORY`
  → `host texture refused 0x8007000e` → the title's assert at `exe+0x193e00f` (`census1`), or the host process's own
  allocation failing (`memory allocation of 136664 bytes failed`, `base1`: system memory that demoted resources
  take counts against the process).

## Built

- `rebuild_d3d12x::vram`: host textures by origin (committed, placed, at address, placed views, twins): live, bytes,
  console bytes, peak; a census table (`REBUILD_VRAM_LOG` only). `HostDevice::bytes`.
- `runtime::memory`: `vram_line` (card usage/budget, tiles by region, card pages used/allowed, host textures, mapped
  textures, census), `REBUILD_VRAM_LOG=<s>`; `pages_by_region`; `video_memory`.
- **Lazy card pages for textures and heaps** (`Memory::allocate_lazy`): a committed texture's console memory and a
  heap's get addresses only; buffers still get card pages under them (`memory::buffer`). `REBUILD_VRAM_LAZY=0` for
  the old behaviour.
- **Safety net** (owner's request): a default-heap resource that fails with `E_OUTOFMEMORY` is made again in system
  memory (`CUSTOM`, `NOT_AVAILABLE`, `L0`) and logged (`the card is full: ... goes to system memory`); never a null
  resource handed back. A **budget watcher** (`memory::start_budget_watch`, `QueryVideoMemoryInfo` 4×/s, on unless
  `REBUILD_VRAM_BUDGET=0`): within 384 MiB of the budget (off again 768 MiB under), new textures nothing renders to go
  to system memory directly. Not yet seen to trigger on the card (the runs that reached the budget predate it).
- **`runtime::mapped`, off by default (`REBUILD_MAPPED_TEXTURES=1`)**: textures over page-mapped GPU memory as
  **reserved** host textures; each host tile stands for one 64 KiB console page (mip by mip, slice by slice, from
  `rebuild_xg`'s console layout) and is mapped to the card page of the **pool page** that console page is mapped to
  now (unmapped: reads zero); 0xf8 remaps move the tiles (`mapped::remapped`, before `cputex::remapped`); a host tile
  with no console page of its own gets a private card page; textures starting inside a page are keyed by (pool page,
  offset) so neighbours never share; small ones (≤ 64 KiB at 4 KiB alignment) are placed in 64 MiB heaps of their
  own instead of a tile each. `uma::space::Space::map_tiles` (one addition to `crates/uma`) maps texture tiles.

## Measured with the mapped path on (card)

| run | what changed | card (local / budget) | host textures | card pages under mapped textures | how far |
|:--|:--|:--|:--|:--|:--|
| `base1` | baseline | 24,742 / 10,877 at 179 s | 23,163 MiB | — | `memory allocation failed` ~185 s |
| `census1` | + lazy | 26,534 / 11,052 | 25,021 MiB | — | E_OUTOFMEMORY, assert `exe+0x193e00f` |
| `fix1` | reserved, 64 KiB-aligned only | 10,514 / 11,052 at 208 s | 8,409 MiB | 888 MiB (11,587 textures) | ran to 220 s; **loading art draws from reserved textures**; 36,090 starts inside a page still committed |
| `fix2` | + inside-page keys | 11,235 / 11,052 | 5,145 MiB | 4,216 MiB (58,441) | frames stop ~107 s; title fatal `exe+0x23cf683` (null write) ~165 s |
| `fix3` | + small packing | 10,932 / 11,050 | 5,150 MiB | 3,509 MiB (34,059) + small 336 MiB (27,288) | same stall ~107 s, same fatal |
| `fix4` | + budget watcher | **9,907 / 11,047** | 5,116 MiB | 2,725 MiB (27,491) + small 285 MiB | stall ~106 s → **device hung (0x887A0006), nvlddmkm 449 → 451**: stopped |

- `fix4` hung **below** the budget (9.8 GB at the stall), so the stall is not memory: the suspect is the volume of tile
  mapping updates (≈ 34k creations + 87k remaps ≈ 120k `UpdateTileMappings` calls in ~20 s, on the ring's queue,
  which every title submission waits on), or something in reserved textures under heavy streaming. Not isolated:
  the rule is to stop at a reset.
- The VRAM side works: with the path on, host textures drop from 23 GB to 5.1 GB and the streamed textures cost
  2.7-3.5 GB of card pages (pool: 2.7-2.9 GB mapped), the run no longer hits E_OUTOFMEMORY, and the load gets
  further (59k-63k placed views vs 44k).
- What is left on the card at that point: render targets in the title's other memory 3.0 GB (514 textures, no
  aliasing), targets in watched memory 1.16 GB (aliasing: 957 MiB covered), title-memory tiles 1.4 GB, twins 0.3 GB.

## Debug-layer items asked about (not reproduced here: no debug run after the reset)

- `#726` vertex-buffer views 0xf0-0x100 bytes past a 64 KiB resource: `xlist` `vertex_buffers` takes the host
  address from `ListHost::memory` (`s.va`), which for the title's physical memory is a per-64 KiB-frame view
  (`titlemem` / `physical.rs` open one view per frame), so a view crossing a frame boundary overruns. Not in this
  lane's files: the clamp belongs in `xlist` (lists lane) or a contiguous run opened as one resource.
- `#1417` barrier with `SyncAfter NONE` then an access in the same submit: the title's own split barriers translated
  by `xlist` (`console/twin.rs` keeps `SYNC_NONE` as given). Lists lane.
- "invalid call" device removal while the world loads (coordinator): not seen in this lane's baseline runs; what
  they show is E_OUTOFMEMORY → assert, with card usage far past the budget first. The safety net above means a
  failed creation never returns a null resource now.

## Left / for the lead

1. **Isolate the `fix4` hang before turning `REBUILD_MAPPED_TEXTURES` on.** Candidates, cheapest first: batch tile
   mapping updates (one `UpdateTileMappings` per texture is already the case; batch per 0xf8 call), move them to a
   queue of their own that only the copies into those textures wait on, or create textures with all tiles mapped
   once and remap only on real page changes (most remaps re-map the same pool page). Run under the debug layer once.
2. Placed-view host textures still live for the run (by design: a descriptor may name them); with the mapped path
   their cost is the pool pages under them.
3. Overlap: `crates/uma/src/space.rs` (`map_tiles`, `TexTile`) and `crates/uma/src/lib.rs` (`card_pages`) are the
   lead's files: one method each, added. `runtime/pagemap.rs` (detile lane's `touched` reused). No change to
   `xlist`.
