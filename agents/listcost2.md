# Lane `listcost2`: the list pool's and titlemem ranges' safety, then the card

Branch `agent/listcost2`, from master c51289a. Card: RTX 4070 Ti. Runs: `runtitle --run --seconds 70`, start screen only
(no input driver, no A), `REBUILD_XLATE_SLOTS=1`, ≤ 90 s wall time, one at a time, `agents\MEASURING` and `agents\CARD`
created new and released only if ours, after a 60 s gap with no title run, no lock and no `rustc`/`dxc` running.
nvlddmkm counted before, after, and 2 minutes after exit.

## Built

1. **Suspect (a), the pool** (`crates/xlist/src/console/pool.rs`, `list.rs`; `crates/runtime/src/submit.rs`). The
   title's Close now **seals** every host list of the console list (`HostList::sealed`). A retired list with no
   execution mark is reset at once only if it was never sealed (an open list cannot have been executed). A sealed list
   with no mark is **held**: kept alive, never reset, counted (`held` in the pool line). Submission marks every list it
   is given, including the ones it skips because their Close failed. It also waits for an earlier run on **any** queue
   (before: only its own), so the mark always covers the last execution. Tests:
   `a_closed_list_without_an_execution_mark_is_never_reset`, plus seal/unseal asserts in the cut test.
2. **Suspect (b), the audit** (`crates/runtime/src/titlemem.rs`, `exports.rs`, `physical.rs`). Each path that frees
   or remaps title memory, and how it reaches `titlemem::changed`:

   | path | reported |
   |:--|:--|
   | `VirtualFree` (the title's only import of it is overridden) → `xmem::host::virtual_free`: release, decommit, placeholder split | yes |
   | `XMemFree` / `XMemFreeDefault`: chunk frees (no mapping changes); big blocks via `virtual_free`; the title's own hooks via its `VirtualFree` import | yes |
   | `XMemMapPhysicalPages(Scatter)`: map (unmap, split, map), unmap; `XMemFreePhysicalPages` closing a section's view | yes |
   | `Physical`'s Drop unmapping section views | **no → now yes** |
   | `memory::arena_free` (readback release, CPU decommit; GPU kind has no CPU memory) | yes |
   | host heaps (`HeapFree`, `free`, `LocalFree`, `GlobalFree`, `CoTaskMemFree` from the title's DLLs), the APU heap's decommits (`crates/audio`), any memory the overrides never saw | **no, and cannot be** |

   So an opened allocation trusts range invalidation only when it lies in a reservation the title made through our
   overrides (`exports::title_reserved`). Any other allocation is confirmed again after every change, as with ranges
   off. Test: `only_allocations_whose_frees_are_reported_trust_range_invalidation_on_warp`.

Gates: `cargo test --release --no-fail-fast` exit 0, 835 then 836 passed.

## Measured (card; nvlddmkm 453 throughout, no reset)

| run | switches | nvlddmkm before → after → +2 min | fps avg (title cap 30) | Reset µs | cut (memory_wait) µs | GPU stamp µs | lookups that queried |
|:--|:--|:--|--:|--:|--:|--:|--:|
| `warp-all` | all three, WARP + debug layer | (WARP) | 19.90 | 16.8 | 21.1 | 43.3 | 9 |
| `card-sections` | SECTIONS | 453 → 453 → 453 | 9.33 ✗ | 227.9 ✗ | 210.6 ✗ | 198.4 ✗ | 392 |
| `card-off` | none | 453 → 453 → 453 | **28.70** | **57.8** | **63.9** | **36.6** | **1,040** |
| `card-pool` | POOL | 453 → 453 → 453 | 28.89 | **7.0** | **20.3** | 36.9 | 1,049 |
| `card-ranges` | RANGES | 453 → 453 → 453 | 28.90 | 60.5 | 70.1 | 33.6 | 947 |
| `card-sections2` | SECTIONS (quiet rerun) | 453 → 453 → 453 | 28.82 | 66.5 | 62.1 | 36.6 | **399** |

✗ `card-sections` ran while other lanes compiled (16 `rustc` seen just after it): its timings are not usable. The
quiet rerun replaces it.

- **Reset breakdown, everything off:** used allocator 5,320 × 45.9 µs, fresh allocator 1,927 × 81.1 µs, during a
  concurrent `ExecuteCommandLists` 12 × 379 µs; at cuts, used 1,935 × 47.6 µs. So it is allocator growth, not lock
  contention with submission (12 calls of 7,259).
- **Pool:** Reset 57.8 → 7.0 µs a call (the host Reset moves to the recycler: 9,214 recycled, 13.3 µs each there), a cut
  63.9 → 20.3 µs, xlate 0.398 → 0.168 ms a frame. 642 lists made while it warmed up. **8 held** (the same 8 on WARP):
  the title closes them and never submits them. Before this fix they were reset at once, which was harmless only
  because nothing executed them.
- **Ranges:** a re-confirmation costs 45.4 → 3.7 µs (340 → 247 of them). Still 698 new opens: those are first sightings,
  which ranges cannot save.
- **Sections:** lookups that queried 1,040 → 399; new opens 698 → 329; 38,116 lookups through 2 section views. **The
  GPU stamp stays 36.6 µs a call**, so its cost is not the lookup. The previous lane's "50 µs is the 2 % misses" does not
  hold on the card; the cost is elsewhere in `stamp: cpu_memory`. One section (0x185c00000 bytes, 6 GiB) cannot be
  opened on the card (E_INVALIDARG); its frames fall back to a heap per mapping, which is where the 329 come from.
- fps is at the title's 30 cap in every quiet run, so it cannot show these gains (28.70-28.90).

## For the lead

- The `pool1` hang (451 → 453) is not reproduced by POOL alone or RANGES alone on this branch. The combination
  (POOL + RANGES), which hung on v1, was not run here: it was not in this lane's order. The next card run, if wanted, is
  that pair.
- `stamp: cpu_memory` at 36 µs is still the largest per-call cost on the start screen after Reset; it needs its own
  breakdown (the section path is already a map lookup).
- The 6 GiB section the card refuses to open as a heap: split section views (e.g. 256 MiB windows) would let SECTIONS
  cover it.
