# Lane `setup` — first-run setup, real saves, and what comes after

Branch `agent/setup`. Built on master 7b95d2c (save editor #73 merged). Card: RTX 4070 Ti. Runs through
`runtitle --run --seconds 300`, windowed 1280x720, offline (no sandbox; the GDK reports no connectivity), each
taking `agents\MEASURING` and `agents\CARD` create-new and releasing only its own; nvlddmkm counted before and after.

## Built

### The live control channel: `REBUILD_INPUT_PIPE=<name>` (dev tool, off without the variable)

`crates/runtime/src/control.rs`. A named pipe `\\.\pipe\<name>` (local clients only, one at a time), opened when the
title creates its GameInput (~3 s into a run). One command a line, one answer a line:

| command | does | answer |
|:--|:--|:--|
| `ping`, `frame` | nothing | `ok <frames presented> <seconds since start>` |
| `press <what>` | a pad press in the input script's syntax (`A`, `Down+LT/500ms`, `RDown/12000ms`, `A/4f`), due at once | `ok <frame> <seconds>` |
| `cap` | capture the next shown frame | `ok <png path>` once written (15 s at most) |
| `exit` | write `summary.toml` (`pipe exit`) and end the run | `ok` |

Presses ride the GameInput poller (8 ms) exactly as `REBUILD_INPUT_SCRIPT` steps do (`script::parse_action`,
`Script::push` / `prune`; printed as `input     pipe ...`), added to what the real devices give. A capture comes back
in ~0.2-0.3 s on the card (3840x2160 PNG).

### `titlectl` (apps/rebuild): the client and a driver that looks at the screen

```text
titlectl <pipe> <command...>                 one command; prints the answer (waits up to 180 s for the pipe: --wait S)
titlectl <pipe> run <steps> [--out DIR]      the driver: comma-separated steps; 1280-wide copies of captures in DIR
titlectl small <png> <out.png> [width]       a smaller copy for reading by eye
titlectl diff <a.png> <b.png>                the driver's screen distance
```

Steps: `<what>` (press, wait out the hold + 400 ms), `<what>~[N]` (press, then wait up to N s, default 60, until the
screen differs from the capture taken just before the press and then holds still), `~[N]`, `settle[N]`, `cap:<label>`,
`sleep:<s>`, `exit`. Screen check: 160x90 grey thumbnails, distance = share of cells whose grey moved by more than
12/255; a page change is 1-99 %, a focus move about 1 %, a ticked box below the 0.3 % change threshold (tick with
plain presses). No reference crops are needed, so nothing derived from the title is kept anywhere.

### `REBUILD_TRACE_SAVES=1`

One stderr line per XGameSave call that names or changes a container (`gamesave  ...`): provider scid, container
info, enumerations, creates, reads, updates, blob writes, submits with their HRESULT, deletes.

### Fix: acceleration-structure views no longer remove the device

`crates/d3d12x/src/heap.rs` `plan`: a null SRV record whose dimension is not a texture's (2-10) is a null *buffer*
view. Before, the title's ray-tracing acceleration-structure SRV (dimension 11, no resource; created as it loads
past the intro) became a texture-shaped null view with format R8G8B8A8, which the host rejects with device removal:
debug layer `#28 CreateShaderResourceView: For D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE, the format
must be DXGI_FORMAT_UNKNOWN`, then `#232 RemoveDevice (DXGI_ERROR_INVALID_CALL)` and the title's GPU-hang assert at
exe+0x2474606 (runs `seed1`, `seed2`, ~45 s after leaving the start screen). Test:
`texture_views_samplers_targets_and_nulls_round_trip` (slot 6).

## The first-run flow (card, run `setup1`; times from process start)

| # | screen | input | notes |
|:--|:--|:--|:--|
| 1 | start / engagement: `ACCOUNT PICKER (Y) · SCREEN NARRATION (OFF) (X) · PLAYER (A)` | A | up at ~33 s |
| 2 | intro video, then the Rockstar logo | none (~20 s) | Bink; plays (video lane) |
| 3 | NAVIGATION EXPERIENCE (Minimap (Dynamic), 3 dots) | A | the setup carousel; ~58 s |
| 4 | UI TEXT SIZE (Small (Default), 4 dots) | A | |
| 5 | SUBTITLES (Off (Default)) | A | |
| 6 | BRIGHTNESS (level 15; "adjust until the symbol is barely visible") | A | **the symbol is not drawn** |
| 7 | FINISH SETUP? (Settings ≡ / Continue A / Back B) | A | ~66 s |
| 8 | LEGAL NOTICES: Copyright, Photosensitivity Health Warning, `Next` | Down, (≥1 s), Down, A | two Downs 0.6 s apart moved focus once: leave ~1 s between moves |
| 9 | ROCKSTAR GAMES LEGAL TERMS & POLICIES: Privacy Policy box, Terms of Service box, `Confirm` | see below | focus starts on the Privacy box |

Page 9: **the tick marks are never drawn** (both boxes stay empty), but A does toggle them: `Confirm` turns from
greyed to enabled after the ToS box is ticked and greys again when it is unticked. What worked: `RDown/15000ms`
(scrolls the privacy text to its end), A (privacy), Down, A (ToS: Confirm enabled). Unknown: whether scrolling is
required before a box takes A (the first A on the privacy box came before scrolling).

**The card hang (setup1, 19:53:09, nvlddmkm 153: 448 → 449).** After scrolling the ToS text (`RDown/12000ms`) and
pressing A (which unticked ToS, Confirm greyed), one `Down` moved focus to the greyed `Confirm`; the frames stopped
0.2 s later (frame 5491, ~196.3 s) and the title's GPU-hang assert fired (exe+0x2474606). Frame line just before:
30 fps, gpu 3.6-4.2 ms, 51-60 draws, 0-1 dispatch, nothing dropped. The lead's earlier script made the same move
(Down to Confirm, A) without a hang, so it is either intermittent or tied to the greyed state. Not repeated.
The ticks and the Brightness symbol are the swizzled CPU-written texture gap (lane detile).

**WARP cannot stand in here:** on WARP the menus draw as broken geometry (large grey/white wedges), so captures do
not show the pages; input still works (`setup2`: the same flow driven blind wrote no save).

## Saves

The owner's older Triumph build left real saves (read-only copy in the scratchpad `realsaves\triumph-old`). Decoded
(`docs/research/save-format.md` layout):

| container | file | header blob | data blob | ASAV kind | kind word | flags | display | outer +0x10 stamp |
|:--|:--|--:|--:|--:|--:|--:|:--|:--|
| `Player` | v7 | 296 (0x128, version 6) | 2,012 | **3** | 1 | 0 | `Player` | 0x6b077c94 |
| `SettingsProfileSettings` | v1 | 296 (version 6) | 7,344 | **2** | 1 | 0 | `Profile Settings` | 0x6b07786d |

Both are unsigned kinds (2-8): editable by the save editor. **The stamp is Unix seconds** (0x6b077c94 = 2026-11-27).
ASAV +0x128 text is the container name; +0x18 = 0, +0x1c = the data size, as in the outer header's +0x1c/+0x20.

Seeded into our layout (`overlay\GameSave\0009000000000001\00000000-0000-0000-0000-000000000000\%50layer.gsc`,
`%53ettings%50rofile%53ettings.gsc`; the fork's encoder replicated in a scratch script, `settings.xml` on S: was
already byte-identical), run `seed1`:

- the title reads both before the start screen (`container info "Player": 2 blobs`, `read blobs async "Player"
  [Header, Data]`, the same for `SettingsProfileSettings`), then after A enumerates both and reads their headers;
- **first-run setup is skipped**: A on the start screen goes to the opening video (`seed1` a6: the illustrated
  credits) and on, no UI TEXT SIZE / legal pages;
- **the engagement screen stays**, but without `SCREEN NARRATION (OFF) (X)`: only `ACCOUNT PICKER (Y) · PLAYER (A)`.
  So the profile settings are loaded before it; the screen itself is not a missing-save gate.

**Our scid is all zeros.** The title asks the provider for `00000000-0000-0000-0000-000000000000`; the fork's
saves sit under `…-00007952987a` (its title id). `Profile::title_id` is never set (0): `drives.rs` configures the
profile without it (gdk.md "For the lead" 1 asks for it from `MicrosoftGame.config`). Left to the storage lane,
which owns that file now.

## Why the engagement screen stays

- Every XUser call succeeds (saves lane trace); the GUID the GDK uses for controller-user association
  (`7d824997-…`, "user devices": `XUserFindForDevice`, device-association events) is **not in the exe's runtime
  GUID table** (only libHttpClient's), so the title never asks which user a pad belongs to. Its pad manager pairs a pad
  with the user itself, on an A press (saves lane).
- With a saved profile, the screen drops its narration item but stays. On a console the title still shows no such
  screen; what differs is not a save. Open: what the console's launch gives the title that skips it (for example
  the launching user and its controller arriving before the title's first frame; worth tracing which branch at
  the pad manager `0x142211xxx`-`0x142212xxx` chooses to show the screen).

## Our own setup run: real saves written by the runtime (card, `setup3`, no seeded saves)

The full flow driven by `titlectl` (steps in the table above) up to the terms page, then: `RDown/12000ms`, A
(privacy), 2.5 s, Down, 2.5 s, `RDown/12000ms`, A (ToS: Confirm enabled), 2.5 s, **Down onto the enabled Confirm**,
2.5 s, A. No hang (nvlddmkm 449 → 449). So the setup1 hang came with the move onto a *greyed* Confirm 0.6 s after a
tick; with Confirm enabled and 2.5 s between moves it did not happen (one run each: not proof either way).
Note the terms page arrives ~1 s after `Next`'s A (the driver's `A~20` saw only the focus change, distance 0.009):
wait for the page before scrolling.

Save trace after Confirm (all `S_OK`): `Player` (header 296 + data 2,012), then `SettingsProfileSettings` (296 +
7,344), then `Player` again, then `container info "PlayerCrossSlot": none`. Files (`overlay\GameSave\
0009000000000001\00000000-0000-0000-0000-000000000000\`):

| file | bytes | container | display | header | data | ASAV kind | kind word | flags | ASAV name |
|:--|--:|:--|:--|:--|--:|--:|--:|--:|:--|
| `%50layer.gsc` | 2,402 | `Player` | `Player` | 0x128, version 6 | 2,012 | 3 | 1 | 0 | `Player` |
| `%53ettings%50rofile%53ettings.gsc` | 7,768 | `SettingsProfileSettings` | `SettingsProfileSettings` | 0x128, version 6 | 7,344 | 2 | 1 | 0 | `Profile Settings` |

Same sizes and kinds as the older build's. The outer stamp (+0x10) is 0x6b0aa8d6 = the container's modification time
in Unix seconds: **the stamp's unit is Unix seconds**. Copies (not in git): scratchpad `realsaves\rebuild-setup3`.

**Second run on them (`persist1`):** the title reads both at start, shows the engagement screen *without* the
narration item, and A goes straight to the opening video and on (no setup pages). Ended through the pipe's `exit`
(exit 0, 70 s).

## Past setup: how far it goes

| run | what | how it ended |
|:--|:--|:--|
| seed1 | engagement (A at 56 s) → opening video (illustrated credits) → letterboxed loading panel, art not drawn (11 draws/frame) | device removed ~101 s (acceleration-structure view, fixed above) |
| seed2 | same, debug layer | the same, named by the debug layer |
| seed3 | with the fix, debug layer | loading continues; frames stall (0-5 fps, 5 s gaps); `CreateCommittedResource3: E_OUTOFMEMORY` for a 2048x2048 10-mip texture, `host texture refused 0x8007000e`, title assert exe+0x193e00f (it checks that HRESULT) at ~125 s. Debug layer at the end: 58,147 live resources, 53,393 heaps, 2,018 PSOs |
| seed4 | the same without the debug layer, VRAM logged (nvidia-smi every 3 s) | **VRAM 6.6-7.2 GB on the menus, 8.3 GB → 11.5 → 11.9 GB of 12 GB** over ~40 s of loading, then the same refusal and assert (~110 s after start) |
| setup3 | after Confirm: dark screen, loading | `memory allocation of 65536 bytes failed` (host side; fail-fast 0xc0000409) at ~232 s |

**The first thing that breaks past setup: memory while the world loads.** Every texture is its own committed host
resource (53k heaps); VRAM fills to the card's 12 GB, then a texture creation fails and the title asserts (or, in
setup3, the host process's own allocation fails). Next: textures placed in heaps with a VRAM budget (residency:
evict / demote to system memory under `IDXGIAdapter3::QueryVideoMemoryInfo` pressure), and the start-screen baseline
(6.6 GB before any world) measured by kind. Also seen at load (debug layer, not fatal): `#726 IASetVertexBuffers`
views running 0xf0-0x100 bytes past their 64 KiB resource; `#1417` a barrier with `SyncAfter NONE` followed by an
access in the same `ExecuteCommandLists`; `#743` command signature argument type (known).

## For the lead

1. **Saves now exist** in `overlay\GameSave\…\00000000-…-000000000000` (written by `setup3`): every title run from
   now on skips first-run setup. To see setup again, move the two `.gsc` files aside (the storage lane's migration
   will copy them to `games\<id>\saves\`).
2. `Profile::title_id` is 0, so the title's scid is all zeros (the fork's: `…00007952987a`). Set it from
   `MicrosoftGame.config` with the storage lane's package identity.
3. The ticks / Brightness symbol: swizzled CPU-written textures (lane detile).
4. Card resets in this lane: **one** (setup1, 448 → 449). Every later run 449 → 449.
