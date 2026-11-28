# Lane `fullscreen`: exclusive full screen

Branch `agent/fullscreen`. Files: `crates/runtime/src/fullscreen.rs` (state machine, mode choice, present flags;
no Windows calls), `crates/runtime/src/present.rs` (`Swap`: window + swap chain + views + the DXGI side of full
screen, `DxgiDisplay`), `crates/runtime/src/fullscreen_probe.rs` (the latency probe, by hand).

## Built

- **`REBUILD_WINDOW=fullscreen`** (the launcher's "fullscreen", which `crates/config` already passes through as
  `fullscreen`) is now its own mode, `WindowMode::Fullscreen`, and no longer runs as borderless. `exclusive` also
  selects it. `windowed` and `borderless`/`windowed fullscreen` parse, size and show exactly as before.
- **The window and swap chain are a `Swap`**, which can be made from any device and queue (the presenter's, or
  WARP's in tests). The presenter's path is the same for windowed and borderless: same window, same swap-chain
  size, flags, latency 2 and present flags (`Present(1,0)`; or `Present(0, ALLOW_TEARING)`).
- **Full screen:** the same borderless popup as windowed fullscreen, shown *with* activation (it needs the focus).
  Its buffers take the size of the chosen display mode. On the first present it calls
  `SetFullscreenState(TRUE, the containing output)` and then `ResizeTarget(mode)`, followed by `ResizeBuffers`.
- **Mode choice** (`pick_mode`): a mode of the output the window is on (`GetContainingOutput`, so with several
  monitors it uses the one the window is on), at the **title's image size**. Among modes of that size it takes the
  desktop's refresh rate (the closest within 0.5 Hz, so 60 rather than 59.94), else the highest rate. If no mode
  has that size it uses the desktop's mode. `display.width/height` do not reach the runtime as variables; the
  title renders at the console output mode, which is the host mode (`dxgi::host_mode`), so this is normally the
  native mode.
- **State machine** (`fullscreen::Controller` over the `Display` trait; DXGI's `DxgiDisplay` or a test mock).
  States: Pending, Exclusive, Suspended, Borderless, Off.
  - first present / focus gained / a check once a second → enter (only with the focus; "not now" is retried);
  - focus lost (Alt+Tab, Win) → leave, `ResizeBuffers`, minimize. Focus back → restore over the monitor, enter;
  - **Alt+Enter** toggles exclusive ⇄ borderless over the same monitor (DXGI's own Alt+Enter stays off:
    `DXGI_MWA_NO_ALT_ENTER | NO_WINDOW_CHANGES`, so every transition goes through the machine). The key is eaten
    so it doesn't beep;
  - full screen taken away (another exclusive app, the secure desktop): `Present` returns `DXGI_STATUS_OCCLUDED`,
    or the once-a-second check sees `GetFullscreenState` FALSE → resize, back to Pending, enter again on focus;
  - device removed/hung/reset (from `Present`'s HRESULT) or shutdown → leave first (DXGI forbids releasing a swap
    chain in full screen), then Off;
  - a failed enter (any other HRESULT) → stays borderless; Alt+Enter tries again.
- **Present flags in exclusive:** with vsync off it presents `Present(0, 0)`. DXGI rejects `ALLOW_TEARING` in
  exclusive full screen, and exclusive tears without it.
- **Threads:** the window thread only queues events (`WM_ACTIVATEAPP`, Alt+Enter). Transitions run on the
  presenting thread at the start of `PresentX`, under the presenter's lock, and wait for the GPU (5 s at most)
  before `ResizeBuffers`. The recorded present lists are dropped after a resize. The window's own calls
  (`ShowWindowAsync`, `SetWindowPos(SWP_ASYNCWINDOWPOS)`) never wait on the window thread. In full-screen mode only,
  `WM_CLOSE` shuts down from another thread, because `SetFullscreenState` sends the window thread messages while
  the presenter's lock is held.
- **Overlay (F10 menu) and resource monitor:** unchanged. They draw into the current swap-chain buffer through its
  RTV every frame, and after a resize they get the new buffer and RTV.
- **Dry run:** a hidden window (tests, `present::hide_window`) or `REBUILD_FULLSCREEN_DRYRUN=1` logs each display
  change ("would enter exclusive at 1920x1080 60.00 Hz") and never makes it. `ResizeBuffers` stays real.
- `present::fullscreen_state()` reports the current state. Every transition is logged as `fullscreen <event>: <a> -> <b>`.

## Decided: exclusive (`SetFullscreenState`) for "fullscreen", independent-flip borderless for "windowed fullscreen"

D3D12 only has flip-model swap chains. On Windows 10/11 a borderless flip-model window that covers the monitor
and has nothing over it is presented by **independent flip**, with the same latency as exclusive. That is
windowed fullscreen, and it stays as it is. Exclusive gives everything independent flip gives, plus:

1. **A display mode of the title's own size.** When the image size differs from the desktop, the display's
   scaler does the scaling, instead of the DWM (which composes, adding a frame), an MPO plane (only on some
   GPUs), or our blit.
2. **Ownership of the output.** A toast, the volume popup or an overlapping window can't knock the frame back
   to composition.

When the image is the desktop's size, exclusive is never worse. Its costs are DXGI's rules, all handled above
(no `ALLOW_TEARING`, leave before release, resize after each transition, a mode change on Alt+Tab only when the
mode differs from the desktop's).

### Measured (2026-11-28, RTX 4070 Ti, one 1920x1080 60 Hz output, Windows 11 26300)

`fullscreen_probe` runs the presenter's own `Swap`, one clear a frame, and measures from the `Present` call to the
vblank that showed it (DXGI frame statistics, `SyncQPCTime`):

| case | present → scan-out | Present CPU | notes |
|:--|:--|:--|:--|
| tearing support | `DXGI_FEATURE_PRESENT_ALLOW_TEARING` = **true** | — | 410 RGBA8 display modes on the output |
| windowed 640x360, vsync on (composed) | **32.9 ms median**, 49.8 ms p95 | 365 µs | 60 fps; ~2 refreshes: frame latency 2 + DWM composition |
| windowed, vsync off (composed) | n/a | 178 µs | 2,452 fps. Composed frame statistics name a vblank from *before* the present they count, so there is no usable latency |
| borderless (independent flip), exclusive | **the owner's run** (below) | | these cover the screen; no automated run takes the owner's display |

What the owner's run should show: borderless and exclusive both about one refresh shorter than windowed with
vsync on (no composition), and the two within noise of each other at 1920x1080. With vsync off, exclusive
presents `(0, 0)` and borderless presents `(0, ALLOW_TEARING)`, and both tear. On this monitor the title's image is
the desktop's size, so exclusive picks 1920x1080 at 60 Hz and changes no mode.

## Tests (all on WARP or pure)

- `fullscreen::tests` (8): enter → resize, and no tearing flag while exclusive; focus loss → leave, resize, minimize
  (a loss already over by the time it is handled is ignored); focus regained → restore, enter; no focus → waits,
  and "not now" is retried by the check; Alt+Enter both ways, and from Pending; a failed enter stays borderless;
  full screen taken away → resize, Pending, enter again; device loss and shutdown leave first and end it; mode
  choice (60 before 59.94, highest when the desktop's rate is missing, the desktop's size when the image's is
  missing, refresh-less modes skipped, an empty list).
- `present::swap_tests` (1, WARP, hidden window, so full screen is a dry run; a missing WARP fails it, never a silent pass): windowed keeps the image's size,
  borderless the display's, both present and ignore full screen's events. Full screen's buffers are its mode's
  size (the hidden window's nearest output, 1920x1080 at 60 here). Start, Alt+Enter ×2, focus lost and regained
  each do a real `ResizeBuffers` and presents continue after each. Device loss ends it.
- `present::window_mode_tests`: `fullscreen`/`exclusive` → Fullscreen; `windowed fullscreen`/`borderless` →
  Borderless; anything else → a window.

## The owner's manual check (the one real full-screen run)

Do it at the desk. It takes the screen.

1. **Probe (no title):** with `agents\MEASURING` and `agents\CARD` absent, in this branch's tree:
   ```powershell
   $env:REBUILD_FULLSCREEN_PROBE = "all"
   cargo test -p rebuild-runtime --release -- --ignored --nocapture --test-threads=1 fullscreen_probe
   Remove-Item Env:REBUILD_FULLSCREEN_PROBE
   ```
   The screen goes grey for about 8 s in borderless, then about 8 s in exclusive. Lines to read: `probe
   Borderless ...` and `probe Fullscreen ...`, with present → scan-out medians for vsync on/off and `full screen
   Some(Exclusive)` on the exclusive line. Put them in the table above.
2. **The title:** in the launcher, set Display → Window mode = **fullscreen**, then Play.
   - The screen shows the title full screen. The log has `fullscreen the mode: WxH at N Hz ...`,
     `fullscreen exclusive at WxH ...` and `fullscreen Start: Pending -> Exclusive`.
   - **F10**: the menu draws. The resource monitor (top corner) draws.
   - **Alt+Tab** to another window: the title minimizes, and the desktop is usable at its normal resolution
     (log `FocusLost: Exclusive -> Suspended`). Click the title on the taskbar: it comes back full screen (log
     `FocusGained: Suspended -> Exclusive`).
   - **Alt+Enter**: the title becomes a borderless window over the screen (`Exclusive -> Borderless`). Alt+Enter
     again: exclusive (`Borderless -> Exclusive`). No beep.
   - With two monitors: Alt+Enter to borderless, Win+Shift+→ to move it, then Alt+Enter. It goes exclusive on the
     other monitor, at that monitor's mode.
   - Close the title (Alt+F4, or the launcher's stop): the desktop comes back at its own resolution.
3. **Windowed and windowed fullscreen** behave as before: a framed window that doesn't take focus, and a borderless
   window over the screen. Alt+Enter does nothing in either.
4. If anything misbehaves, `REBUILD_FULLSCREEN_DRYRUN=1` runs the same path with every display change logged and
   none made.

## For the lead

- `crates/config/src/lib.rs:145`, `display.window`'s help text still says fullscreen "runs as windowed fullscreen
  until exclusive mode is built". Proposed: "fullscreen = exclusive full screen at a display mode of the title's
  image size (else the screen's), Alt+Enter toggles it with windowed fullscreen, Alt+Tab minimizes it". I didn't
  edit it (config is the lead's).
- `display.width/height` don't reach the runtime. If they should choose the full-screen mode, pass them as a
  variable and `Swap::new`'s `source` can take them.

## Next

- The owner's run (above) fills in the borderless and exclusive rows.
- HDR (`R10G10B10A2`/FP16 with an HDR colour space) isn't handled specially. The mode list falls back to RGBA8
  modes when the format has none.
