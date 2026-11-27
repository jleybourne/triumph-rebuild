# Lane `input` — controller and keyboard

Crate `crates/input` (package `rebuild-input`). No GPU, no global state, no title code.
State: **I1, I2, I3 closed.** 16 tests green (`cargo test --release -p rebuild-input`), plus one hand-run
measurement (`host_poll_cost`, ignored by default).

## Built

- `pad.rs`: `PadState` and `KeyState`, laid out like the console's `GameInputGamepadState` (28 bytes) and
  `GameInputKeyState` (12 bytes), checked field by field in a test. GameInput button bits. XInput →
  `PadState`: XInput's documented dead zones (left 7849, right 8689, triggers 30), radial, rescaled so the
  edge of the dead zone reads 0 and full travel 1.
- `keyboard.rs`: the stand-in pad, the fork's documented mapping (README "Running it"): W A S D / I J K L
  sticks, arrows D-pad, Enter / Backspace / X / Y faces, Q / E shoulders, Z / C triggers, Esc / Tab Menu /
  View. F / H click the sticks (in the fork's table, not in its README). 24 keys, none twice, F10 free.
- `source.rs`: `trait Source` (pad per slot, keys held, rumble), `Frame` (four slots, the stand-in pad,
  the keys) and `Fake` for tests. The stand-in pad is connected only while no real pad is; the keyboard is
  always a device of its own.
- `menu.rs`: `Input<S>` (I2, the fork's `docs/design/debug-menu.md` A1). One poll per present gives
  `Polled { title, menu, change }`:
  - **Closed**: the title gets live input; F10 is removed from its keys.
  - **Open** (F10, or `open()`): the title gets every connected pad as `NEUTRAL` (connected, released,
    never "disconnected") and no keys; the menu gets the real frame. Rumble `(0, 0)` is sent to all four
    slots on open, and `title_rumble` is dropped until the title sees live input again.
  - **Draining** (F10 again, or `close()`): the title stays neutral until every key and pad button is up
    and both triggers are below 0.1. Sticks are positions, not presses, and are not waited on. F10 during
    draining reopens; holding F10 toggles once.
- `gameinput.rs` (I3): what each GameInput call the title makes reads from a `Frame` (table below).
- `host.rs` (Windows): XInput slots 0-3, `GetAsyncKeyState` over 0x08-0xFE (244 keys; generic
  Shift/Ctrl/Alt skipped, their left/right keys are reported), keys only while the foreground window
  belongs to this process.

## Measured

```
tests                 16 green: pad 2, keyboard 2, source 2, menu 7, gameinput 2, host 1
I2 cases              open · hold while open · close while held · all-up · F10 swallowed · reopen while draining · nothing held
host poll, 0 pads     4 empty XInput slots: 64-82 µs median, ~150 µs p99 (3 runs, 1,000 polls each)
keyboard scan         244 × GetAsyncKeyState: 103 µs median, ~300 µs p99
Frame::read focused   ~170 µs a poll (pads + keys); unfocused ~64-81 µs (keys skipped)
```

Machine: the dev machine, idle, `agents/MEASURING` absent, no pad plugged in. Command:
`cargo test --release -p rebuild-input host_poll_cost -- --ignored --nocapture`.

## I3 — GameInput as the title reaches it

Source: lane `loader`'s `agents/loader-imports.csv`, **read from the unmerged branch `agent/loader`
(PR #1, commit `46d04f8`)**. Of the census's 697 GDK functions in 86 DLLs, the title imports exactly **one**
from `gameinput.dll`: **ordinal #1**, which is `GameInputCreate` (the DLL's only export). Every other call
goes through the objects it returns, so the import table cannot list them. The methods below are the
public GameInput (v0) interfaces in declaration order. "Fork" says whether the fork's working
implementation answers the call (`victory-hle/src/modules/gameinput.rs`, read for design only), which is
the evidence that the title uses it.

| interface · method | fork | maps onto |
|:--|:--:|:--|
| `GameInputCreate` (ordinal #1) | yes | one `Input<Host>` behind the returned object |
| `IGameInput::GetCurrentTimestamp` | yes | host clock, µs |
| `IGameInput::GetCurrentReading(kind, device)` | yes | `gameinput::first_of(kind, &polled.title)`, or the given `Device`; its reading wraps that `Frame` |
| `IGameInput::GetNextReading` / `GetPreviousReading` / `GetTemporalReading` | yes | a per-device history of `Frame`s (a new reading per change), kept by the runtime |
| `IGameInput::RegisterReadingCallback` | yes | called on each changed `Frame` |
| `IGameInput::RegisterDeviceCallback` | yes | `Device::status` changing between polls (pad plugged or pulled; stand-in appears when the last pad goes) |
| `IGameInput::RegisterSystemButtonCallback` / `RegisterKeyboardLayoutCallback` | token only | nothing; never fires |
| `IGameInput::StopCallback` / `UnregisterCallback` | yes | runtime bookkeeping |
| `IGameInput::CreateDispatcher` / `CreateAggregateDevice` | E_NOTIMPL | — |
| `IGameInput::FindDeviceFromId` | yes | `gameinput::DEVICES` by id |
| `IGameInput::FindDeviceFromObject` / `FromPlatformHandle` / `FromPlatformString` | returns "not found" | — |
| `IGameInput::EnableOemDeviceSupport` / `SetFocusPolicy` | no-op | — (focus is `host::Host::focused`) |
| `IGameInputDevice::GetDeviceInfo` | yes | `Device::kind` (gamepad `0x40000`, keyboard `0x10`) |
| `IGameInputDevice::GetDeviceStatus` | yes | `Device::status`: `0x3` connected, else 0 |
| `IGameInputDevice::SetRumbleState` | yes | `Input::title_rumble(slot, low, high)`: dropped while the menu has input |
| `IGameInputDevice` battery, force feedback, haptics, sync, power-off, raw device access | not implemented | — |
| `IGameInputReading::GetInputKind` | yes | `Device::kind` |
| `IGameInputReading::GetSequenceNumber` / `GetTimestamp` / `GetDevice` | yes | the runtime's reading history |
| `IGameInputReading::GetKeyCount` / `GetKeyState` | yes | `Device::keys(&frame)` → `[KeyState]` (12-byte console layout) |
| `IGameInputReading::GetGamepadState` | yes | `Device::gamepad(&frame)` → `PadState` (28-byte console layout); false for the keyboard |
| `IGameInputReading` raw report, controller axes/buttons/switches, mouse, touch, motion, arcade stick, flight stick, racing wheel, UI navigation | return nothing | — |

What the isolation means for these calls: everything above reads `polled.title`, never the raw frame, so
while the menu is open or draining every reading the title gets is neutral, and a device never appears
disconnected because the menu opened.

## Next

- The GameInput object itself (COM-style vtables the title calls, reading history, callbacks) is Build 3's
  runtime, which needs the lead's memory model; this crate gives it everything it reads.
- Throttle empty XInput slots (re-check an empty slot about once a second): an empty slot is the larger
  share of a poll.
- Mouse: GameInput has a mouse reading; the fork returns nothing and so does this crate.

## For the lead

- **Poll cost is ~170 µs with focus, ~70 µs without, and 4 empty XInput slots are most of the unfocused
  number.** The debug-menu design asks for one poll per present and gives the whole overlay 0.3 ms. Poll on
  a thread of its own (the fork polls every 8 ms) and have the present path read the last `Polled`, or
  throttle empty slots, before L5 puts this on the present thread.
- `secondary-agent.md` section 5 (I3) says "once merged": the CSV was read from the unmerged `agent/loader`
  branch (PR #1). If the census changes before merge, only the gameinput row matters here (ordinal #1).
- The stand-in pad's F / H stick clicks are in the fork's table but not in the mapping section 5 quotes;
  kept, since they collide with nothing.
