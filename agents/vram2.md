# Lane `vram2` — reserved-texture tile mappings without the hang

Branch `agent/vram2`, on master 4fe1812 (vram #78 merged). Card RTX 4070 Ti 12 GB, budget ~11.05 GiB. Tests: 802
passed, 0 failed (`cargo test --release --no-fail-fast`, exit 0).

**Stopped at a reset.** After card run `v2-d`, nvlddmkm went 451 → 453 (two events 153 at 00:10:40 and 00:11:40,
30 s and 90 s after that run's process had ended and its locks were released; no lock was held until another lane's
at 00:12:58). `v2-d` never reached the world load (tile queue idle, every fence passed), so the cause is not shown;
by the rule, no card runs after it. `REBUILD_MAPPED_TEXTURES` stays **off by default**.

## The 0xf8 stream (measured, run `v2-c`, new counters)

| | |
|:--|--:|
| batches | 336 |
| pages mapped / onto the pool page they had | 49,414 / **0** |
| pages unmapped / not mapped before | 4,361 / 10 |
| ranges | 49,414 kind 1 (one page each), 1,365 unmapped, **0 multi-page runs** |
| tiles over remapped pages / same key (skipped) / still unmapped / remapped | 84,010 / 0 / 16 / 83,209 |

So the title never re-maps a page onto its own pool page and never sends contiguous runs: skipping same-page remaps
saves nothing on the real stream. What does save: no per-key calls, no NULL call at creation, one call per texture
per batch, and (on our side) 1.8 tiles a range, 3.6 tiles a call.

## Built

- `runtime::tilequeue`: tile mappings planned per resource (`plan`: runs of consecutive tiles onto consecutive card
  pages as one range, one call per heap, NULL ranges folded into it, a later mapping of a tile wins) and issued by a
  thread of their own on a queue of their own (`REBUILD_TILE_QUEUE=copy|compute|direct`, default copy) with a fence;
  title submissions wait for the latest value on the GPU (`memory::flush_wait`); the tile queue never waits, so no
  cycle; jobs for one resource queued behind a slow pass are merged; resources held until the fence passes.
- `runtime::mapped`: keys and card pages per creation or 0xf8 batch in one call (`memory::cards_for`,
  `release_keys`); key pages are not mapped in their sub-window (only reused card pages, once a batch, to be zeroed);
  a new texture maps only tiles with a card page; remaps onto the same key and unmapped→unmapped skipped. Nothing
  runs under the memory model's lock in a driver call any more (in `fix4` the vram logger blocked ~60 s behind one).
- `uma::space::heap(h)` replaces `map_tiles`/`TexTile` (one small method; overlap with the lead's crate).
- Counters: `pagemap` (same page, unmapped twice, ranges by kind) and the tile queue's live state (issued,
  signalled, GPU past, thread in a pass for how long) on the `vram` line, with the present count.
- `tilebench` (WARP): the stream replayed old path vs new, calibrated to `fix4`. `titlectl` step `A*N`: press until
  the screen changes (the engagement screen is ready anywhere from 35 to 70 s, by menu frame rate).

## Measured: WARP stress test (`tilebench --seconds 20`, `agents/vram2-tilebench.txt`)

| path | UpdateTileMappings | driver thread in mapping calls | mapping latency p50 / p99 / worst | frame latency p99 / worst | device |
|:--|--:|--:|--:|--:|:--|
| old (vram lane) | 172,669 (8,633/s) | 90.7 ms/s | 0.04 / 39.6 / 119.8 ms | 42.4 / 113.7 ms | ok |
| new | 114,157 (5,708/s) | 5.8 ms/s | 0.10 / 3.8 / 8.3 ms | 3.9 / 6.4 ms | ok |

WARP neither stalls nor errors on either path: the hang does not reproduce there.

## Measured: card (guarded: locks, ≤ 200 s, nvlddmkm before/after, `REBUILD_VRAM_LOG=1`, mapped on)

| run | input | how far | VRAM local / budget | resets |
|:--|:--|:--|:--|:--|
| `v2-a` | A at 36 s | A too early: engagement screen to the end; 194 s of frames | 6.3 / 11.2 GB | 451 → 451 |
| `v2-b` | A at 45 and 61 s | load started; title crash ~65 s (write to a read-only page of its own, `exe+0x376a61b`; 0 remaps by then) | 6.4 GB | 451 → 451 |
| `v2-c` | A at 45 s | **world load ran**: 33,271 reserved textures, 83,209 remaps in **49,965 calls** (fix4's old path: ~120,000 est.), 0 mapping failures; loading-screen art drawn at 81 and 101 s; frames slow 102-122 s, stop ~122 s; title fatal `exe+0x23cf683` (null write, as in fix2/fix3) ~139 s | **10.8 / 11.05 GB** (at the budget; 3.1 GB non-local) | 451 → 451 |
| `v2-d` | A at 47.6 s | A too early (menus ~10 fps); 175 s of frames | 6.3 GB | 451 at exit, **453 30-90 s later** |

- `v2-c` got past `fix4`'s hang point (~106 s) with no device hang and no reset; the vram logger kept its 1 s pace
  throughout (no lock held through a driver call). The tile thread's calls cost 102 µs of CPU each, worst
  submit-to-signal 371 ms.
- But it reached the budget: tile heaps 4,800 MiB (3,340 under mapped textures, of which ~700 MiB are 11,190
  private tiles; title memory 1,394) + host textures 5,430 MiB (render targets in other memory 3,005, targets in
  watched memory 1,160, placed views 782, twins 322). Why frames stopped is not shown (no tile-queue state in that
  run; `v2-d` added it but never loaded).

## Left / for the lead

1. The reset after `v2-d`: unattributed (see top). Before the next card run, check who used the card at
   00:10:40-00:11:40 on 2026-11-29.
2. Memory to reach the budget with room: private tiles (~700 MiB) could map NULL or one shared page (small mips
   only); render targets in the title's other memory (3.0 GB, no aliasing) are the largest single item.
3. One card run with the tile-queue state on the vram line (`v2-d`'s build) through the load, to see whether the
   ~122 s stop is the tile thread (a pass that does not end), the GPU (fence not passed) or the title.
