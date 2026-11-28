# Lane `listcost` — per-list fixed costs on the recording threads

Branch `agent/listcost`, from master 8086b14. Card: RTX 4070 Ti. Runs: `runtitle --run --seconds 75`, start screen only
(no A), `REBUILD_XLATE_SLOTS=1`, each taking `agents\MEASURING` and `agents\CARD` create-new and releasing only its own,
nvlddmkm counted before and after, 60 s gaps.

## Stopped at a GPU reset

| run | binary | what was on | nvlddmkm | result |
|:--|:--|:--|:--|:--|
| `base1` | master 8086b14 | nothing new | 451 → 451 | 26.68 fps avg (title cap 30), 1,870 frames |
| `warp1` | v1, WARP + debug layer | list pool, titlemem ranges | (WARP) | clean: no allocator or list errors |
| `pool1` | v1, card | list pool, titlemem ranges | **451 → 453** (153 at 00:10:40 and 00:11:40) | GPU hung ~5 s in; title fault exe+0x2215c35 (write of 0); 464 lists |
| `warp2` | v2, WARP + debug layer | pool, ranges, sections | (WARP) | clean |
| `warp3gbv` | v2, WARP + debug layer + GPU-based validation | pool, ranges, sections | (WARP) | clean: no message besides the known #622/#743/#964 |

`pool1` turned on two things at once (v1 = this branch's first build): the **list pool** (host lists and allocators
reset on a thread of their own after the queue's progress fence passes) and **titlemem range invalidation** (a free or
remap confirms again only the opened allocations it touched). Neither is isolated: card runs stopped at the reset, as the
rule says. On WARP the debug layer and GPU-based validation report no allocator reset before its executions completed
and no list reset while executing. **Both are off by default** (`REBUILD_LIST_POOL=1`, `REBUILD_TITLEMEM_RANGES=1`), so
master's behaviour is unchanged unless asked for. Suspects for the lead, in order:

1. The pool resets a host allocator when `executed_at` is 0 ("never executed"). A host list executed by any path that
   does not mark `executed_on/executed_at` (only `submit::execute` marks) would be reset while the GPU runs it.
2. Range invalidation trusts every free and remap to be reported (`xmem::host::virtual_free`, `physical` map/unmap/free,
   `memory::arena_free`). An unreported one leaves a stale heap the GPU writes a label into: a page fault. The global
   generation hid any such gap.
3. Unrelated: the `vram` lane hit two resets at 22:31 the same evening.

## Measured: the 2 % of title-memory lookups that missed

`REBUILD_TITLEMEM_TRACE=1` (new) prints the first 64 lookups that query, with the reason; the run report prints them by
reason (`titlemem` line 2). In `pool1` (card) and `warp1`:

- **Nearly all are new mappings, not invalidations**: 332 of 336 queries (card) were a first lookup in an allocation never
  seen, each opened as a heap of its own, **0.4-2.5 ms each on the card** (2.3 ms mean). Only 2-156 were re-confirmations
  after a change (3-6 µs each, cheap).
- The allocations are **64 KiB physical frames** (mapped views, `private: false`) at 0xb40000, 0xb60000, 0xb80000, ...:
  the title maps a fresh frame every few frames and writes its GPU-timestamp labels at +0xff20. 183,433 frees and remaps in
  75 s (~2,400 a second) on the start screen.
- So "stamp: cpu_memory" was 50.5 µs a call on the card (7,480 calls, 377 ms) while the ordinary label lookup is 1.6 µs.

**Fix (`REBUILD_TITLEMEM_SECTIONS=1`)**: a mapped frame is reached through its section's one whole view (`physical`
maps it once; the same bytes as every mapping), opened as a heap once per section. WARP: lookups that queried 504 → 12;
20,887 lookups through 2 section views; the two opens cost 125 ms each once (large sections), then each stamp lookup is a
map lookup. Not measured on the card (stopped).

## Measured: host Reset (base1, card)

| | calls | µs a call |
|:--|--:|--:|
| reset: host Reset | 6,975 | 66.2 |
| split: host Reset | 1,873 | 60.2 |
| memory_wait (a cut) | 1,873 | 79.4 |
| label: GPU stamp | 7,480 | 50.8 |

With the pool (WARP): the recording thread's Reset is a pop (5.9 µs, the notify included), a cut's 1.5 µs; the host
Resets moved to the recycler (124-152 µs each on WARP). `pool1` on the card: reset 13.9 µs, cut 27.1 µs (636 lists made
new while the pool warmed up), before the hang.

The breakdown by allocator state (`Reset: fresh allocator` / `used allocator` / `during submit`, new `xslot` rows, list
pool off) was built to tell driver lock contention from allocator growth, but no card run with the pool off was made
after the reset.

## Built

- `crates/xlist/src/console/pool.rs`: the pool (one host allocator per host list, recycled after its fence), a unit test
  on the fake backend; `REBUILD_LIST_POOL=1` (runtime: `submit::enable_list_pool`, called by `launch::run`).
- `crates/runtime/src/titlemem.rs`: range invalidation (`changed(lo, hi)`, `REBUILD_TITLEMEM_RANGES=1`), reasons and costs
  of queries (`why_report`), `REBUILD_TITLEMEM_TRACE=1`, the section-view path (`REBUILD_TITLEMEM_SECTIONS=1`).
- Every free and remap now reports its range, after the change (before: a bump before the free). The heap's big frees
  (`exports.rs` `Heap::free`) and the arena's frees were never reported before; they are now.
- `xslot` rows: allocator Reset count, host Reset by allocator state and by a concurrent `ExecuteCommandLists`
  (`console::SUBMITTING`), the pool line.

## Next (needs the lead's say for card runs)

1. A card run with only `REBUILD_TITLEMEM_SECTIONS=1` (no pool, no ranges): the stamp cost.
2. A card run with nothing on: the Reset breakdown (fresh / used / during submit).
3. Only then the pool alone, after closing suspect 1 (count host lists retired with `executed_at` 0 that were executed).
