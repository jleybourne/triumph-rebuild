# Lane `overlay` — the in-game menu (task L5)

Crate `crates/overlay` (package `rebuild-overlay`), wired in `crates/runtime/src/overlay.rs` and `present.rs`.

## Design

```text
overlay-input thread (crates/overlay::service)        present thread (runtime::present)
  Input<Host>::poll every 8 ms (~170 µs a poll)         overlay::visible()  one relaxed load (all it costs closed)
  title's Frame -> Shared::title (neutral while open)   overlay::set_target(shown w, h)  one store
  Navigator: presses + repeat -> Nav                    overlay::record(): Arc<Scene> clone, copy quads,
  Menu: Settings / Stats / Developer                       one DrawInstanced(4, n) in the slot's own list
  layout -> Scene (quads), only when something changed  executed with the present list, before Present
```

- **Render**: after the title's image is copied/blitted into the swap-chain buffer, a second list per frame
  slot (only while something is shown): root constants + two root SRVs (quads in the slot's upload buffer, the
  atlas as a raw byte buffer), no descriptor heaps, no vertex buffers, colour writes only (alpha untouched),
  rounded rects by SDF, glyphs 1:1 from a GDI-baked atlas (Segoe UI semibold, sized from the output height,
  rebaked when the height changes). sRGB colours; converted to linear for an RGBA16F (scRGB) swap chain.
  Shaders and pipeline are built on the input thread, never the present thread.
- **Captures** copy the buffer before the overlay; `REBUILD_CAPTURE_OVERLAY=1` copies after it.
- **Input**: menu key `menu.key` / `REBUILD_MENU_KEY` (F10), or View + Menu together on a pad (new
  `Input::with_pad_chord`, `PAD_CHORD`). Navigation: arrows / D-pad / left stick, Enter / A, Backspace / B,
  Esc / Start closes, Q / E / LB / RB change page; directions repeat (350 ms, then 70 ms). Anything held at open
  is ignored until released. The title sees neutral pads and no keys while open, and until all is released
  after close (crates/input's draining).
- **Settings** (the `Live` registry entries the runtime applies): Vsync, Frame rate, Frame cap,
  Frame line overlay (`diagnostics.overlay` = the stats readout top-right while closed). Applied at once through
  `frame::set_vsync` / `set_frame_limit`; saved on close into `REBUILD_CONFIG` (file re-read, only changed keys
  set, `.bak` kept, written whole then renamed).
- **Stats**: fps, frame ms, GPU ms, wait, draws, dispatches, dropped, late, resolution, the overlay's own CPU
  and GPU cost, pacing words. From `framelog::latest()` (new: the last second's line, kept with or without a log).
- **Developer**: `Registry` of `Action`s (name, group, typed `Param`s: int / float / choice / bool,
  `offline_only`, `bound`, `run(args) -> Result<line>`). Offline-only actions are disabled while
  `online()` (stub: false). Today: "Capture frame" (real) and four `Placeholder`s (Teleport, Spawn vehicle,
  Set time of day, Set weather) that refuse until bound.
- **Theme**: every colour and size in `crates/overlay/src/theme.rs`, sizes at 1080 lines scaled by height.

`REBUILD_OVERLAY=0` turns all of it off (no thread, no renderer).

## Measured

```text
4070 Ti, 3840x2160, 600 frames   record CPU (present thread) mean 17-18 µs, p99 28-30 µs
(card_overlay_cost_at_4k, 3 runs) GPU pass 9.3-9.6 µs; first frame (atlas copy) 141-150 µs
                                  layout (input thread) 8-39 µs a page; GDI bake at 45 px 6.2 ms (input thread)
menu closed                       one relaxed load a present; no list, no submission
WARP title run (runtitle, 30 s)   readout drawn at 4K over the title's frames, captured with
                                  REBUILD_CAPTURE_OVERLAY=1; overlay ready 39 ms after the swap chain
```

**A GPU fault found and fixed before it reached a card:** fxc flattened the pixel shader's glyph branch, so solid
quads loaded the atlas at wrapped coordinates through a root SRV (no bounds check). WARP removed the device; the
debug layer's GPU-based validation named it (`Root descriptor access out of bounds`). Fixed with `[branch]` and
every atlas read clamped to its last byte; `tests/gbv.rs` (its own process, debug layer + GBV on WARP) fails if
it comes back (checked by reverting the fix). Lesson for every root-SRV shader in the layer: a branch is not a
bounds check.

## Tests

`cargo test --release -p rebuild-overlay`: 27 + 1 (GBV); input +1 (pad chord); runtime +2 (frame API, registry).
Menu state machine and navigation, key repeat, open-held keys ignored, isolation through the service (keyboard
and pad), pad-only open/navigate/close, save on close keeping the rest of the file, action registry and online
gating, layout inside its panel at 720p/1080p/4K, WARP: pixels changed only inside the panel (alpha untouched), a
glyph drawn exactly from its atlas pixels. By hand: `warp_snapshots` (PNGs for the design pass,
`REBUILD_OVERLAY_SNAPSHOTS=<folder>`), `card_overlay_cost_at_4k` (takes the CARD lock).

## What the Developer page needs from the RE work (`docs/research/devmenu.md`)

For each action: a name and group; its parameters with ranges or choice lists (teleport destinations, vehicle
names, weather states as the title spells them); how to invoke it (a title function address + calling
convention, or the memory to write), which thread may call it (input thread vs. the title's main thread: if
the latter, we need a hook point to queue onto); whether it is safe online; and how to read the title's real
online state for `online()`.
