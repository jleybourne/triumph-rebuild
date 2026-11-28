# Lane `video` — the start screen's black stretches, Bink's video, CPU-written textures

Branch `agent/video`. Built on master 05818f1 (descriptor remap merged).

## Measured: what the black stretches were

On master before 05818f1 the start screen cut to full black for ~4-5 s, back for ~3 s, repeating. The title
submits the **same work every frame** in both phases (lists traced over frames 1000-1800: 5 lists, 124 calls, 21
draws, 1 dispatch, 1 clear in every frame). The difference is the UI's descriptor tables: the title takes them
from a ring that walks console view indices ~367,000 → ~1,230,000 by ~4,096 a frame and wraps every ~213 frames
(~7 s at 30 fps). Frames whose tables sat past index 750,000 were the black ones (frames 1000-1075, 1200-1275,
1400-1500 black on the old binary; the same frames have tables at 776,987-1,228,560 in the trace). Past 750k the
old runtime bound nothing ("in no bound heap"), so every UI draw read null views and wrote black.

**Fixed by the descriptor remap (lane `descremap`, 05818f1)**, not by this lane: on master 05818f1 with this
lane's changes switched off (`REBUILD_CPU_TEXTURES=0 REBUILD_BINK=host`), 9 captures over 40-72 s are all the
normal start screen (run report: 46,068 tables bound past index 750,000, all served).

Bink was **not** drawing on the start screen: the title opens its intro video there paused
(`BinkOpenWithOptions` from memory, flags 0x4104448, `BinkGetFrameBuffersInfo`, `BinkPause(1)`) and decodes nothing
until A is pressed (`REBUILD_TRACE_BINK=1`).

## Built: what was really broken — textures the CPU writes

After A the title plays its intro video, then the Rockstar logo (as on the console). Before this lane: a **solid
green screen** (Y = Cr = Cb = 0) for the whole video, and menu body text missing most letters
("T t di bl di i" for "Text and icons are balanced in size."), the menu gradient flat. One root cause: a texture
is a host texture of its own (`d3d12x`), and nothing carried what the CPU writes into its console memory into it.

- `crates/runtime/src/cputex.rs`: textures whose console layout is linear (rows a host copy takes as they lie)
  and that the GPU never writes (no target, depth or UAV flag) are registered when `d3d12x` makes them
  (`Memory::texture_made` / `texture_gone`, a new hook pair in `crates/d3d12x/src/memory.rs`). Marked when written:
  - watched memory: the upload ring's backer names the pages it sends (`cputex::written`, one call in
    `memory.rs`'s backer); the copy reads the card's copy of the pages (sub-window buffer), after the queue's
    ring wait, so never ahead of the upload;
  - other memory (Bink's frame buffers): Bink's frame calls (`crate::bink`) mark a frame; a 513-word sampled hash
    per texture picks the buffer set the frame went to (one of three).
  - `submit::execute` asks `cputex::refresh` for one host list ahead of the title's lists (one atomic load when
    nothing is marked). Each copy takes the texture from the layout it lives in (the title's initial one) to
    copy-dest and back with enhanced barriers.
- `crates/runtime/src/pagemap.rs`: the queue's page mapping batch (0xf8, `CopyPageMappingsBatchX`, was a trap
  returning E_NOTIMPL): the title maps pool pages (its physical pages' CPU view, e.g. `0x2546c510000`) at GPU
  addresses (`0x25225470000`); Bink's planes are textures at the GPU addresses, written through the CPU view.
  Kept as bookkeeping (GPU page → CPU address); `cputex` copies from the CPU view.
- `crates/runtime/src/bink.rs`: the title's 13 Bink imports that matter bound through counting wrappers (4 integer
  registers forwarded); `REBUILD_TRACE_BINK=1` prints calls and the `BINKFRAMEBUFFERS` handed over.
- WARP: copies from the reserved sub-window buffers fault in WARP's copy code (first-chance read of 0 on a WARP
  thread; the same data through an upload buffer runs clean), so on WARP every copy goes through an upload buffer
  (`REBUILD_CPU_TEXTURES=upload`, the default there; `ring` / `0` force the others).

## Measured (card, RTX 4070 Ti, master 05818f1 + this lane, A at 45 s)

| | before (`REBUILD_CPU_TEXTURES=0`) | after |
|:--|:--|:--|
| intro video (48-57 s) | solid green, 171 KB PNGs | the video (eagle, then the Rockstar logo), 9.6-11.6 MB PNGs |
| UI TEXT SIZE page (70 s) | body text "T t di bl di i", flat background | "Text and icons are balanced in size.", "Small (Default)", gradient |
| fps (75 s run) | 28.85 | 28.98 (paced at 30) |
| nvlddmkm events | 448 → 448 | 448 → 448 |
| copies | — | 1,503 texture copies, 5,391 MiB over the run (450 Bink frames x one 12.6 MB buffer set) |

WARP (70 s, upload mode): clean, 103 copies, 41 MiB; before the upload mode two WARP runs died in WARP's copy code
right after the first ring-mode copy. Tests: 723 passed, 0 failed (`cargo test --release --no-fail-fast`).

Run report: 82 textures kept current (32 outside watched memory), 606 refused as swizzled, 12 with rows the host
cannot copy (linear mip chains whose small mips sit off the 512-byte grid).

## Left / for the lead

- Bink frames go through the upload buffer: ~12.6 MB memcpy on the submit thread per video frame (~1-2 ms). The
  pool's CPU view is not opened in place (`titlemem` misses it); opening it would make it a GPU copy.
- Swizzled CPU-written textures (606 seen, e.g. a 256x128 R8 `SW_4KB_S` placed view with data at creation) are
  not refreshed: a detile pass (`xg` tables) is the next step. Streamed textures mapped through 0xf8 are the same
  case.
- 0xf8 now answers S_OK (was E_NOTIMPL); the GPU side of those addresses is not on tiles.
- Overlap with `titlemem-fast2` (cold slices, merged): none in code; this lane adds one call to `memory.rs`'s
  backer and two lookups (`memory::watched`). Cold slices keep the backer semantics (pages sent each pass).
