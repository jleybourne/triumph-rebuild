# Lane `detile` — CPU-written textures in the console's tiled layouts

Branch `agent/detile`. Built on master c0fecdd (video lane merged: linear CPU-written textures kept current), rebased
onto 42b4f46. Tests: 783 passed, 0 failed (`cargo test --release --no-fail-fast`, exit 0).

## Measured: what the 606 were

Master's run report on the start screen → video → menus said `refused: 606 not linear, 12 rows the host cannot
copy`. This lane adds a census to `cputex` (always in the run report; `REBUILD_TRACE_CPU_TEXTURES=1` lists every
class): each texture that is not plain linear rows, by where its memory is, its swizzle mode, format, dimension, mips
and arrays, with whether it held data when made, whether the ring marked it again, and whether its CPU bytes ended
different from its last copy (**stale**). Card run, A at 45 s, 110 s (`card3` below):

| memory (what writes it, what marks it) | mode | textures | MiB | data when made | written again | stale at the end |
|:--|:--|--:|--:|--:|--:|--:|
| page-mapped pool (0xf8: DirectStorage fills the pool's pages, the title maps them at GPU addresses) | `SW_4KB_S` (5) | 447 | 116.3 | 438 | 0 (12 changed by remaps) | 1 |
| page-mapped pool | `SW_64KB_D` (10) | 8 | 43.0 | | | |
| watched (the title's graphics memory; the ring marks it) | `SW_4KB_S` (5) | 42 | 134.8 | 24 | 36 (1x1 constants, 33 rewrites) | 0 |
| watched | `SW_256B_D` (2) | 26 | 119.6 | 0 | 1 | 0 |
| watched | linear mip chains off the 512-byte grid (the 12) | 12 | 0.6 | 0 | 2 | 0 |
| other CPU memory (physical-page views) | `SW_256B_D` (2) / `SW_4KB_S` (5) | 19 / 6 | 43.7 / 5.2 | 0 | 0 | 0 |
| unreadable when made (pages never mapped during the run) | `SW_4KB_S` / `SW_64KB_D` | 33 / 25 | 9.4 / 98.4 | 0 | 0 | 0 |

- **Formats**: BC1 (71, 72), BC3 (77), BC5 (83), BC7 (98, 99), RGBA8 (28, 29), RGBA16F (10, as 17³-33³ 3D colour
  grading LUTs: 112 of them), R9G9B9E5 (67: 2560x1440 lighting-sized textures, no data, likely written by the GPU
  through an aliasing view), R32F (41), and a few more. Dimensions 1x1 to 5808x5808; mips 1 to 11; arrays to 6;
  3D textures.
- **How they are written**: streamed textures are written by DirectStorage (zlib/BCPack output, already in the
  console's tiled layout) into pool pages *before* the title makes the texture (a placed view or resource over
  them): 438 of 455 held their data when made and never changed after. The watched ones are the title's CPU
  writes (1x1 constant textures rewritten 33 times; the rest once or never).
- **No XOR modes** (`_X`, `_T`) and no MSAA among CPU-written textures: every one is addressable by `rebuild_xg`.
- **Not touched**: render targets, depth and UAV textures (GPU-written) as before; textures in the arena's GPU-only
  memory are now skipped explicitly (0 in these runs).

## Built

- `crates/runtime/src/detile.rs`: the **detile pass on the card**. Per subresource, `rebuild_xg::Layout::tables`
  gives a column table and a row table (`offset = rows[z*h + y] + columns[x]`, true for every non-XOR mode and for
  linear); one compute shader (cs_5_0 through `D3DCompile`, root constants + two root SRVs + a root UAV, no
  descriptor heap) reads the texture's console bytes where the card already has them and writes linear rows into
  a staging buffer; the copies then take the rows by footprint, as for a linear texture. Elements of 4/8/16 bytes
  are one word load per word; 1- and 2-byte elements gather four or two. `detile::reference` is the same on the
  CPU (the bit-exact reference, and `REBUILD_DETILE=cpu`).
- `cputex`: textures that are not plain linear rows (swizzled, or linear with rows the host cannot copy) are
  registered with a `Copy::Detile` (layout; tables made at the first copy) instead of refused. The refresh puts
  every detile of a submission in one list: pass (staging UAV) → one barrier → texture barriers → footprint copies.
  Sources: watched memory from the ring's card copy (no CPU work), else the title's memory opened in place, else
  its CPU bytes through the upload buffer (the page-mapped pool, once per texture). A batch's staging is capped at
  64 MiB (the rest waits a submission; one texture always goes). `REBUILD_DETILE=0` keeps the old refusal.
- `pagemap` → `cputex::remapped`: the queue's page mappings (0xf8) mark the textures over the GPU pages they map
  or unmap (a streamed texture made before its pages were mapped, or a slot streamed into again).
- Census and staleness in the run report (above), and refresh CPU time by phase.

## Decided (with measurements)

- **GPU detile, not CPU.** The bytes of watched textures are already on the card (the ring's sub-window
  buffers), so the pass costs the submit thread nothing but recording; a CPU detile would read them back from title
  memory and write them to an upload buffer (xg's detiler: 3.6-11.5 GB/s single-threaded). For the page-mapped
  pool the CPU still copies the raw bytes once (memcpy, 11-12 GB/s) and the card detiles. Load-time cost on the card
  run: 539 MiB detiled in 653 detiles, all inside one 110 s run's normal frame pacing (29.2 fps, no dip in low 1 %).
- **Placing textures at console addresses with a matching host layout: rejected.** The title's CPU-written
  textures are almost all `SW_4KB_S` and `SW_256B_D`; D3D12's only standard swizzle is 64 KB
  (`D3D12_TEXTURE_LAYOUT_64KB_STANDARD_SWIZZLE`, optional), which matches neither, and has its own mip-tail rules.
- **Tables, not swizzle equations in the shader**: the same tables `rebuild_xg`'s CPU detiler is checked with (lane
  `xg`, against the fork); 0.1-5 % of a texture's bytes, made once per texture.
- **Bink (task 3)**: opening the pool's CPU view in place is not cheap here: `physical.rs` maps one view per 64 KiB
  frame, so a 12.6 MB buffer set is ~190 views → ~190 heaps per set, read over PCIe. Splitting the upload memcpy
  over four threads was measured (12.2 vs 11.1 GB/s on one thread: 0.1 ms a video frame) and dropped. The
  per-frame cost is ~1.2 ms of memcpy (5,683 MiB over 450 video frames in 539 ms).

## Measured

| | master (video lane) | this lane |
|:--|:--|:--|
| refused | 606 not linear + 12 rows the host cannot copy | **0 + 0** |
| detiled | — | 618 textures, 653 detiles, 539 MiB |
| stale at the end (bytes differ from the last copy) | (not measured; 606 never copied) | 1-2 of 618 |
| fps (110-117 s, A at 45 s, menus; paced at 30) | 28.98 (video lane, 75 s); 29.34 this build with `REBUILD_DETILE=0` | 29.08 / 29.18 / 29.31 / 29.33 (four runs), low 1 % 27.6-29.0 |
| nvlddmkm events | 448 → 448 | unchanged in all nine runs (448, then 449 after another lane's run) |

Visible (card captures, outside git):

| page | `REBUILD_DETILE=0` (master's behaviour) | detile on |
|:--|:--|:--|
| BRIGHTNESS ("adjust until the symbol is barely visible") | no symbol | the R★ symbol, dark as intended |
| LEGAL NOTICES, FINISH SETUP?, menu pages | — | inline VI / R★ logo glyphs and controller icons (L-stick, menu, A, B) drawn |
| the game's loading screens | — | full-screen artwork (streamed BC textures) whole and clean |

The ROCKSTAR LEGAL TERMS checkbox tick (the setup lane's other target) was **not** checked: getting there means
ticking the terms checkbox, which accepts the terms; that is the owner's to do. A run that does it can confirm the tick.

Past setup (fresh save, A presses into the story's loading): 1,054 textures through the pass, 961 detiles,
571 MiB; with `REBUILD_DETILE=0` the same path refuses **1,027 + 12**. Both runs then end with the same device removal
(DXGI "invalid call") and the title's assert at `exe+0x2474606` while the world loads, **with the detile off too**:
not this lane (for the lead, below).

Refresh CPU by phase (menus run, 117 s): finding 353 ms (Bink's sampling), sources and plans 246 ms, buffers 70 ms,
staging and recording 659 ms, of which the CPU copies are 536 ms (5,683 MiB, 11.1 GB/s: Bink). At load the detile
work alone (no Bink frames yet) is ~260 ms over the whole load: plans 137 ms, recording 95 ms, buffers 26 ms,
copies 34 ms.

WARP: the pass equals the CPU reference word for word on 66 cases (every addressable mode the title uses plus the
256 B ones; element sizes 1-16; BC1/BC7; full chains with mip tails; arrays; 3D; odd sizes): 1,271,135 words
(`detile::tests::the_pass_matches_the_reference_on_warp`); the reference equals `rebuild_xg::Layout::detile` row for
row. A 90 s WARP title run is clean (570 textures, 580 detiles, 434 MiB).

## Left / for the lead

- **Device removal while the world loads (not this lane)**: after FINISH SETUP → A, the story's loading screen
  runs ~10 s, then DXGI removes the device ("the application made a call that is invalid") and the title asserts at
  `exe+0x2474606`; identical with `REBUILD_DETILE=0` (runs `card6` / `card7`), nvlddmkm unchanged. A debug-layer run
  on that path would name the call.
- **Bink's sampling scales with linear textures**: past setup ~16,000 linear textures sit outside watched memory, and
  every Bink frame samples each one (513 reads): 543-724 ms of the refresh in those runs (with the detile off too).
  Sampling only textures in Bink's registered buffer sets would make it constant.
- Every detile is checked against its host texture before its first copy (subresource count, mip sizes); 0
  mismatches seen, logged and left alone if one comes.

- **Stale streamed textures (1-2 per run)**: page-mapped textures whose pool pages DirectStorage rewrites while the
  texture and its mapping stay (12 changed in a run, 10-11 of them caught by remaps or later copies). Marking them
  needs DirectStorage's page writes → frames (`physical::mapped_at`) → pool CPU pages → GPU pages (a reverse
  of `pagemap`) → `cputex`. Gameplay will stream far more than the menus; measure there first.
- **Textures over pages never mapped** (58, 108 MiB in the menus, `Unreadable`): copied when 0xf8 maps their pages.
- **R9G9B9E5 2560x1440 textures in watched memory** (26 + 19, no data, no RT/UAV flag): probably written by the GPU
  through an aliasing view of another format. The detile copies only their (zero) console bytes at creation; if the
  title writes them by the GPU through an alias, that alias's host texture is a different one (not this lane).
- **Bink** stays a ~1.2 ms memcpy per video frame on the submit thread (see Decided).
- `placed_texture` (d3d12x heap.rs) caches placed views by (address, descriptor words) forever: a streaming slot
  reused with the same words keeps its first host texture. The remap marks cover it while the mapping changes.
