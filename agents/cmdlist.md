# Lane `cmdlist`: the D3D12.X command list and allocator objects (Build 5/6)

Branch `agent/cmdlist`. Code: `crates/xlist/src/console/` (new module `rebuild_xlist::console`), plus read-only
accessors appended to `crates/xlist/src/d3d.rs` (`Setup::pipeline_raw` … `Setup::submit`) for the card test. Nothing
else in `crates/xlist` changed. No game folder, no title runs.

## Built

**The objects in memory as the title reads them** (`agents/d3d12x-object-layouts.csv`):

```
list       +0x000 vtable (160 slots)  +0x008 bits 0  +0x00c refcount  +0x010 type
           +0x018 put, +0x020/28/30/38 limits: all ZERO (every command arrives as a call)
           +0x040 .. +0x960 the title's (root headers, the pipeline packet copy at +0x544, newer fields): zeroed, never touched
           +0x960 ours (ListState)
allocator  +0x000 vtable (24 slots)  +0x00c refcount  +0x010 type  +0x018 ours
```

Offsets are compile-time asserts and a test (`the_list_is_laid_out_as_the_title_reads_it`, which also scribbles over
+0x544..+0x960 as the title does and checks the list still works).

**Every slot exists.** Lists: **66 of 160 implemented, 94 traps**. Allocators: 7 of 24 implemented, 17 traps. A trap
counts its calls (one relaxed atomic add), records the first call's time, calls `ListHost::first_trap(object, offset)`
once, and fails with the slot's return kind: `E_NOTIMPL` only where the method returns an `HRESULT`
(list 0x028 SetPrivateDataInterface; allocator 0x018-0x028), **0 (null / false) everywhere else**, so a
pointer-returning slot never hands back an HRESULT (research §5.2). `console::traps()` lists every trapped slot
called, with count and first-call time: one title run writes the missing-slot table.

Implemented (this title's "newer" offsets):

| slots | what |
|:--|:--|
| 0x00-0x50 | QI (PC list ids 0-4, the console's List5/List6, child, object, pageable, IUnknown), AddRef/Release, private data, SetName, GetDevice (through the lead's device QI), GetType, Close, Reset |
| 0x60-0x88, 0x1e8, 0x280, 0x408 | draws, Dispatch, DispatchX, DispatchMesh, ExecuteIndirect (signature and buffers mapped), CopyBufferRegion, CopyResource (+0x2b0 CopyResourceX) |
| 0x1f0 | CopyMemoryToMemoryX: both addresses to (host buffer, offset), a CopyBufferRegion |
| 0xa0-0xc8, 0x230 | topology, viewports, scissors, blend factor, stencil ref, depth bounds; SetPipelineState through `PipelineSource` |
| 0xd0, 0x490 | ResourceBarrier (resources mapped; one on an unknown resource becomes a global barrier, so ordering is never lost); enhanced barrier groups (`ID3D12GraphicsCommandList7::Barrier` with resources mapped when the host has OPTIONS12 enhanced barriers; else one global barrier) |
| 0x100-0x180, 0x190-0x1b8 | descriptor heaps, root signatures, every root argument (tables, constants, CBV/SRV/UAV), index/vertex buffers, render targets, the four clears, discard |
| 0x238/0x240 | bottom-of-pipe writes → `ListHost::label` (default: `WriteBufferImmediate` MARKER_OUT into the mapped address, Build 2's labels; 64-bit written low word first) |
| 0x248/0x250 | memory waits → the list is **cut** there; the wait goes on the next `Segment` for the queue to make a GPU-side wait |
| 0x418/0x420 | pipe syncs → one global barrier, dropped when nothing ran since the last barrier |
| 0x458 | ResetX |
| 0x4a0 | real upload memory (aligned, mapped, GPU-readable), owned by the **allocator** until its Reset |
| 0x1c0-0x1d0, 0x258, 0x328, 0x470 | markers, depth-order hint, DriverHintX (→ 0), 0x470: counted no-ops |
| allocator 0x40/0x48 | Reset (host allocator + upload chunks reused), allocation callback (S_OK) |

Trapped list slots the CSV names: 0x080 CopyTextureRegion, 0x090 CopyTiles, 0x098 ResolveSubresource, 0x0e0-0x0f0
queries, 0x1d8/0x1e0 (unknown newer methods), 0x2a8 CopyTextureRegionX, 0x320 DispatchMeshX, 0x3f0/0x400 shading rate.
They need the lead's texture layouts (xg), query heaps, or are not seen called. 0x188 SOSetTargets is trapped (not
in the CSV).

## The interface the lead calls

```rust
use rebuild_xlist::console::*;

// device 0x48 CreateCommandAllocator
let a: *mut c_void = create_allocator(kind, &host_device, host.clone())?;    // host: Arc<dyn ListHost>
// device 0x60 CreateCommandList / 0x248 CreateCommandListX (open, `pipeline` bound if non-null)
let l: *mut c_void = create_list(kind, a, pipeline)?;
// queue 0x50 / 0x118 ExecuteCommandLists
if let Some(list) = host_list(l) { /* one host list, no waits: the common case */ }
for seg in segments(l).unwrap() { /* make each seg.waits a GPU wait, then execute seg.list.list */ }
stats(l)           // calls, draws, dispatches, elided, refused, splits since the last reset
traps()            // every trapped slot called, process-wide
upload_range(a, va) // a 0x4a0 address -> host range, outside a list
```

The lead implements `ListHost` (with `PipelineSource` as its supertrait):

```rust
fn pipeline(&self, console) -> Option<HostPipeline { pso, root_signature, compute }>   // PipelineSource
fn resource(&self, console) -> Option<Raw>              // ID3D12Resource
fn descriptor_heap(&self, console) -> Option<HeapBinding { heap, console_base/bytes/stride, host_base/stride }>
fn cpu_descriptor(&self, console_handle) -> Option<usize>   // RTV / DSV / UAV-clear CPU handles
fn memory(&self, va) -> Option<MemoryRange { console, bytes, host_va, resource, resource_offset }>
// defaults: device() null, root_signature() None, command_signature() None,
//           label() = WriteBufferImmediate MARKER_OUT, memory_wait() = Split, first_trap() / unknown_iid() no-ops
```

Every returned pointer is borrowed: it must outlive the GPU's use of the lists that recorded it (D3D12's rule).

## Measured

| what | number | how |
|:--|:--|:--|
| ours alone (stand-in host list that only counts) | **0.056 µs a draw, 0.012 µs a call** (0.25 ms a frame) | `our_cost_per_draw_on_the_stand_in`, Build 0 (b)'s frame: 4,500 draws, 20,629 calls through the console table |
| ours + D3D12 + driver, one thread | **0.465-0.474 µs a draw**, 0.100 µs a call (2.09-2.13 ms a frame), vs `translate()` on the same frames in the same run **0.425-0.431** (ratio 1.09-1.10) | `card_cost_per_draw`: 48 console lists, 20,846 calls a frame, 40 interleaved rounds, medians; a noisy third run read 0.617 vs 0.509 (ratio 1.21) |
| on the card | clear → barrier → 64-bit label → copy out of 0x4a0 memory → memory wait (cut: 2 segments) → pipe sync → enhanced global barrier group (0x490) → Close; segments run in order; pixel `[255,0,0,255]`, label `0x123456789`, 64 copied bytes exact, **debug layer: 0 errors** | `card_records_a_clear_a_barrier_a_label_and_a_cut` |

So the object layer costs **~0.04 µs a draw over the bare translator** (one more indirect call per command, the
shadow writes, the host-side lookups) and stays in the 0.43 µs class, well under decision 3's "near 0.5 µs" rule.

Run the card tests by hand, only while `agents\MEASURING` is absent (they check and skip):

```
cargo test -p rebuild-xlist --release -- --ignored --test-threads=1 card_records_a_clear_a_barrier
cargo test -p rebuild-xlist --release -- --ignored --nocapture card_cost_per_draw
```

## Decided

- **Translate as called, not at Close.** Same thread as decision 3, no recording buffer: arguments that point into
  the title's stack (barrier arrays, viewports, clear colours) are consumed during the call, so nothing is copied,
  and the cost is the bare translator's plus one call. Close only closes.
- **Memory waits cut the list** into segments (the fork's run-30 fix, made the default): a wait before any work
  attaches to the current segment with no cut; after a cut everything bound is set again on the new host list
  (heaps, root signatures, pipeline, every root argument incl. constants, fixed-function state, targets, buffers).
- **0x4a0 memory belongs to the allocator**, not the list: a list may be reset as soon as it is submitted, an
  allocator only once the GPU is done, so freeing at list reset (the fork) could free memory the GPU still reads.
  Chunks of 1 MiB upload heap, kept and reused at every reset: a frame allocates nothing.
- **Unknown objects are refused, never guessed** (counted in `stats().refused`), except barriers, which degrade to
  a global barrier so ordering survives.

## For the lead

1. **Submission:** use `segments(l)`; each segment's `waits` must become a GPU wait (queue `Wait` on the fence your
   label machinery signals for that address) before its list runs. `host_list(l)` is `Some` only for the
   one-segment, no-wait case.
2. **Resource states:** legacy barriers pass the title's states through unchanged. Host resources must start in the
   state the title believes (back buffers, render targets), or the debug layer will complain. Mixing the title's
   legacy (0xd0) and enhanced (0x490) barriers on the same resource is passed through as the title does it.
3. **Timestamps:** 0x240 flag 1 (GPU timestamp) is written by the list itself (`console/stamps.rs`): a query at
   the label's point, resolved into the title's memory at the host list's end, in host ticks (queue 0x90 answers
   the host's frequency). Flag 2 (system timestamp) still goes to `label` and is refused by default. Every direct
   and compute host list also carries a timing pair (`HostList::stamp`) for the lead's GPU time
   (`runtime/src/gputime.rs`).
4. **Comparison codes** of memory waits are taken as D3D12's comparison functions (the fork's reading; the title's
   first wait is 7 = `>=`, reference 1). `MemoryWait::holds_for` implements it; unverified beyond that wait.
5. **Single-range render targets** step by `CONSOLE_RTV_INCREMENT` = 128 (the RTV increment device 0x78 answers).
6. **Trap callers:** `first_trap` gives object and offset; the caller's module+RVA (research §4's recommendation)
   needs the return address, which stable Rust cannot read. The lead's exception/stack tooling can add it.
7. `secondary-agent.md` §2 lists `crates/xlist` as the lead's; this lane was assigned there. The change is additive:
   a new `console` module, one `pub mod` line in `lib.rs`, accessors appended to `d3d.rs`.

## Next

- CopyTextureRegion / CopyTextureRegionX with the console's tiled footprints (needs lane `xg`'s layouts).
- Queries (0x0e0-0x0f0) with the lead's query heaps (device 0x138).
- Measure on the real stream once the title records (Build 5's frame line): calls per draw and the refused count.
