# Lane `present`: Build 5's last stretch (submission, PresentX, capture, the frame line)

## Built

| piece | where | what |
|:--|:--|:--|
| submission | `crates/runtime/src/submit.rs` | queue 0x050 `ExecuteCommandLists(count, *lists)` and 0x118 `ExecuteCommandListsX(count, *lists, token, flags)`. Each console list's `segments()` in order; runs without waits go out in one host `ExecuteCommandLists` (stack batch of 64). A memory wait becomes a queue-side wait: a fence's value cell (`GetGpuAddress`) compared `>=`/`>`/`==` → host `Wait(host fence, value)`; memory that already holds → nothing; other CPU-readable memory → the **bridge**: the host queue waits on the queue's own fence for the next value, and a waiter thread signals it from the CPU once the memory holds (polls every 250 µs, only while something is pending); unreadable memory → dropped, counted |
| queue / fence lookup | `queue.rs`, `fence.rs` | `find()` no longer takes the global `Mutex<HashMap>`: the object keeps its `Queue`/`Fence` at +0x38, trusted after the method table matches. `fence::by_cell()` finds a fence by its value cell (append-only table, no lock) |
| PresentX | `crates/runtime/src/present.rs` | window (borderless over the primary display, own message thread), flip-model swap chain on the presenting queue's host queue at the host display mode, 3 buffers, frame-latency waitable at 2, format by the back buffer's family. Copy path (`CopyTextureRegion` + `SetSourceSize`, the compositor scales) or blit path (full-screen triangle, linear sampler; other format families or larger than the screen). Never blocks: display full or slot busy → frame counted `late` and not shown |
| GPU time | `present.rs` + `submit.rs` | per frame slot a timestamp before the frame's first submission (a list recorded once, put in front by submission) and one after the present's copy |
| capture | `present.rs` | `capture.request` in the session folder (checked by the frame-line thread) or `REBUILD_CAPTURE_FRAMES=2,30` (presented frame numbers): the shown image → readback → PNG `captures/frame-<n>.png` on a thread of ours; `capture.request` deleted after |
| frame line, summary | `crates/runtime/src/framelog.rs` | one line a second to `REBUILD_FRAME_LOG`; `summary.toml` at `REBUILD_RUN_SECONDS` (then exit) or on window close; `runtitle`'s own summary now carries the frame keys (`framelog::summary_keys`) |
| wait time | `frame.rs`, `fence.rs` | time in `WaitFrameEventX` and blocking fence waits feeds `wait` |

### PresentX, as settled

```text
HRESULT PresentX(queue, u32 planes, const Plane* planes, const void* parameters)
Plane = { u64 frame token, u32 resource count, ID3D12Resource* const* resources, ... }
```

Plane 0's first resource is the image. Source: the research doc's queue row (`agents/d3d12x-child-slots.csv`, 0x100),
the fork's `queue.rs:1117-1123` (`Plane`) and `:1288-1395` (which showed frames with exactly this reading, run 28:
one plane). **Confidence: high for the first three fields and plane 0** (two independent readings agree and the
fork displayed frames with it); **unknown**: the plane stride, the rest of a plane, and `parameters` (unread; with
one plane none is needed).

### The frame line

```text
t 12s | fps 30.8 | ms 32.5 | gpu 21.0 ms | draws 4,512 | dispatches 120 | dropped 0 | wait 6.0 ms | late 1 | held 2 | 2560x1440 -> 3840x2160
```

Per frame: `ms`, `gpu`, `draws`, `dispatches`, `dropped` (draws/dispatches the lists dropped for a pipeline with no
host object, `ListStats::skipped`), `wait`. Per second: `fps`, `late` (presents not shown), `held` (bridged memory
waits). `upload`, `xlate`, `upscale`: not measured yet, absent. The sample is `crates/runtime/src/frameline.sample`;
runtime formats it, the launcher's own test parses it (`frameline::tests::the_layers_frame_line_parses`).

## Measured

See "Numbers" below (from `present_card_lists_run_in_order_and_the_frame_reaches_a_png`, `--nocapture`).

## Tests

- `present_card::present_card_lists_run_in_order_and_the_frame_reaches_a_png` (GPU; skipped while `agents\MEASURING`
  exists or with no device): through the device's slots, d3d12x resources and views, xlist lists: clear a render
  target, copy it into a back buffer, execute (0x50), present (0x100), capture, decode the PNG: every pixel the clear
  colour. Then a list on queue 1 cut by a memory wait whose label a list on queue 2 writes 100 ms later: the
  submit returns at once (bridged), the copy after the wait shows queue 2's green. Then the costs.
- `submit::tests` (fence-wait values, refused/met/bridged placement), `present::tests` (format families, pixel
  conversion), `framelog::tests` (the line, read back; per-frame deltas; grouping).

## For the lead

1. **Back buffer state at PresentX.** The copy reads the title's back buffer by implicit promotion from `COMMON`
   (= `PRESENT`): right if the title's last barrier leaves it in `PRESENT`, as console code does before presenting.
   In a real run, check the debug layer for state errors on the first presents.
2. **Wait placement in a real run:** watch `held` on the frame line and `submit::WAITS_{MET,FENCE,BRIDGED,REFUSED}`.
   Bridged waits cost one GPU round trip each (the queue stalls until the waiter sees the label, ≤ 0.25 ms after it
   lands). If the title's waits are mostly labels its own queue wrote earlier, a later step can track label writes
   per segment and drop such waits (the queue's order already keeps them).
3. **Plane 0's resource must have a host texture**; if not, the first such present says so once on stderr.
4. `device.rs`: three small edits: `list_host()`, `framelog::start()` at creation, and a creation lock (two callers
   at once, e.g. parallel tests, could make two host devices and mix their objects).
5. `runtitle.rs`: `write_summary` includes `framelog::summary_keys(false)` in place of the fixed
   `measured_from = "title_start"`.
6. `pipeline.rs` has two `non_snake_case` warnings (`CreateComputePipelineState`, `...X`) on master already; not
   touched here (lane `streampso` is in that file).
7. The window takes the primary display (borderless, not exclusive fullscreen). Closing it writes `summary.toml`
   and ends the process.

## Next

- Pacing from presentation (Build 8): the waitable, `GetFrameStatisticsX` from DXGI's frame statistics.
- `upload` and `xlate` on the frame line once the ring and the lists report times.
