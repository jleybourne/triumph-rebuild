# Design — the launcher, settings, features, mods and the in-game menu

Status: **designed 2026-11-27 (owner request). `crates/config` (the settings registry) is built by the
lead. The desktop launcher is helper lane `launcher`. The in-game menu waits for Build 5's present
path.**

## One registry, three faces

Every option the layer has is declared **once**, in `crates/config`'s registry: its key, label, help text,
type and range, default, whether it applies **live** or needs a **restart**, and whether it is **stable** or
**experimental**. Everything else is generated from it:

```
crates/config (registry + rebuild.toml)
     ├── apps/launcher      desktop app: set everything before launch, profiles, system check, mods
     ├── the layer          reads rebuild.toml at start; validates against the registry; refuses bad values
     └── the in-game menu   F10 overlay: the live options, the frame line, the mod list
```

This replaces the old layer's ~40 `VICTORY_*` environment variables, which were untyped and undocumented
in the binary, and silently ignored when misspelt. Here an unknown key is reported, a bad value is refused
with its reason, and the default is always the measured best.

## `rebuild.toml`

Lives beside the launcher, or in `%APPDATA%\triumph-rebuild\`. Never in the game folder.

```toml
profile = "default"

[settings]                      # keys from the registry; anything absent takes its default
"display.width" = 3840
"upscaler.backend" = "auto"
"memory.upload_workers" = 4

[[mods]]                        # load order is the list order
name = "better-hud"
enabled = true
```

## The settings groups

| group | what it holds | examples |
|:--|:--|:--|
| `game` | where the title is | `game.dir` |
| `display` | output size, window mode (windowed, windowed fullscreen, fullscreen) and window size (720p by default), vsync, frame cap | `display.width`, `display.window`, `display.window_size`, `display.vsync` |
| `upscaler` | backend and mode (Build 7) | `upscaler.backend` = auto/dlss/fsr/native, `upscaler.mode` |
| `title` | edits to the title's own `settings.xml`, applied at launch to a copy in the write overlay (the original is never touched) | `title.fsr_quality` (render scale; 4 is refused: it was black in the fork) |
| `memory` | Build 1-2 switches | `memory.upload_workers`, `memory.labels` = gpu/relay |
| `shaders` | Build 4 switches | `shaders.wave64` = doubled/split |
| `diagnostics` | overlay, frame line log, captures, debug layer | `diagnostics.overlay`, `diagnostics.d3d12_debug` |
| `menu` | the in-game menu | `menu.key` |

**The rule for switches:** a switch is added only with its measured effect, and its default is the
measured best. Experimental switches are shown with a warning and can't be saved in a profile marked
"stable".

## The desktop launcher (`apps/launcher`, helper lane)

egui/eframe: Rust, same toolchain, MIT/Apache, no browser engine and no network crate.

```
tabs     Play · Display & upscaler · Title settings · Features · Mods · Diagnostics · Profiles · About
Play     profile picker, [Launch], status, and the live frame line once the layer runs
         (read from a file the layer appends, no sockets)
setup    first run: pick the game folder; system check (GPU, driver, tiled tier ≥ 2, SM 6.6, DXC, VRAM, ReBAR)
         with a one-line fix for each failure
Features every registry switch in its group, generated from the registry: type-correct widgets, help text,
         live/restart badge, experimental warning, "reset to default"
Mods     folders under mods/, enable/disable, drag to reorder, conflicts (two mods replacing one file) shown
Profiles named sets of settings; export/import as .toml
```

### Redesign (owner, 2026-11-27): top bar, dark and light, session diagnostics

The owner approved **Direction B: a top bar** (mockups: https://claude.ai/artifact/C4JarmJgfw9jAdFdGnJZLY, page
"Direction B"), in **dark and light** themes (a theme switch; default dark). Every part of the launcher lives in it:
Play, Profiles, Settings, Runs & test matrix, Mods, Diagnostics, and the Running view while the title plays. The
screens' content follows the Direction A mockups (same canvas) under the top bar.

**Session diagnostics (owner):** every play session gets its own folder (frame line log, run summary with the
profile and limits in force, crash reports, captures), kept for `diagnostics.retention_days` days (default 14; 0 =
keep forever). The launcher creates the folder, passes it to the layer as `REBUILD_SESSION_DIR`, lists sessions on
Runs and Diagnostics, and deletes folders past the retention when it starts. The layer and `runtitle` write into it.

#### L6 built (lane `launcher`): the screens, and the session folder contract

Screens (code: `apps/launcher/src/screens/`, tokens `apps/launcher/src/ui/theme.rs`): a top bar (logo, Play ·
Profiles · Settings · Runs · Mods · Diagnostics with the active one underlined in the accent, a status pill "Ready" /
"Running · `<profile>` · mm:ss", the theme switch, the window buttons); Play (header band, Launch, profile picker,
Test matrix; cards Last run · Target · GPU · System check) and the Running view (fps, frame time against 33.3 ms,
draws and dropped, internal → output resolution; the frame-time graph with the budget line dashed; where the frame
goes; the frame line; Capture frame; Stop); Profiles; Settings (search, group pills, Live / Restart / Experimental,
the save bar); Runs & test matrix; Mods; Diagnostics. The game library is reached from Play ("Change game"). Both
themes are the approved palettes; `launcher.theme` is saved launcher-wide in `<app home>/launcher.toml`, not per
game. A value nothing has measured shows "—". The launcher still requests no frame while idle.

**The session folder** (`apps/launcher/src/session.rs`). One per launch, under the game's own launcher folder:

```text
<app home>/games/<id>/sessions/<UTC stamp>-<profile>/      e.g. 20261127T153012Z-series-x  (-2, -3 if taken)
  session.toml      LAUNCHER: [session] at launch, [end] when the child exits (see below)
  rebuild.toml      LAUNCHER, matrix runs only: the configuration that run used (--config points here)
  frame-line.log    LAYER: the frame line, one line a second (REBUILD_FRAME_LOG points here)
  summary.toml      LAYER: the run's summary, written at exit (or when the run length is reached)
  crash-*.txt       LAYER / runtitle: one file per crash (any name starting crash- and ending .txt)
  captures/         LAYER: frame captures
  capture.request   LAUNCHER: "Capture frame"; the layer captures into captures/ and deletes this file
```

Environment the launcher gives the child (on top of `--config <path>` and the `REBUILD_LIMIT_*` variables):

| variable | when | meaning |
|:--|:--|:--|
| `REBUILD_SESSION_DIR` | every launch (and proof runs) | the session folder, absolute; it exists and holds only `session.toml` (and `rebuild.toml` for a matrix run) |
| `REBUILD_FRAME_LOG` | every launch | `<session>/frame-line.log`, unless `diagnostics.frame_log_path` is set (then that path, which the launcher tails instead) |
| `REBUILD_RUN_SECONDS` | test matrix runs | the run length; the layer should write `summary.toml` and exit after it. The launcher stops the process 20 s later if it hasn't |
| `REBUILD_MEASURE_FROM` | test matrix runs | `title_start` or `after_loading`: where the summary's measuring window starts |
| `REBUILD_MATRIX_ID` | test matrix runs | the matrix's id (a stamp), also in `session.toml` |

`session.toml` (the launcher writes it; the layer only reads it, if at all):

```toml
[session]
profile = "series-x"                     # the profile name in force
profile_label = "Console class (Series X budget) (built-in)"
started = "2026-11-27T15:30:12Z"
started_unix = 1795793412
launcher_version = "0.1.0"
exe = 'C:\...\rebuild.exe'
config = 'C:\...\rebuild.toml'
limits = ["RAM 13.5 GiB", "VRAM 10 GiB (from Build 5)", "14 logical processors"]
matrix = "20261127T150000Z"              # matrix runs only, with run_seconds and measure_from
[settings]                               # every registry key's effective value at launch
"display.width" = 3840
# ...
[end]                                    # added when the child exits
ended = "2026-11-27T15:33:17Z"
ended_unix = 1795793597
seconds = 185
exit_code = 0                            # absent when the process gave no code
exit = "exit code: 0"
stopped_by_launcher = false
```

`summary.toml` (the layer writes it; every key optional, and a missing one shows as "—"):

```toml
[summary]
avg_fps = 30.1          # mean over the measuring window
low1_fps = 27.0         # 1 % low
frame_ms = 33.2         # mean frame time
gpu_ms = 20.1
upload_ms = 1.9
frames = 1800
seconds = 60
measured_from = "after_loading"
```

Without `summary.toml` the launcher averages fps, frame, GPU and upload ms from `frame-line.log` and leaves the 1 %
low empty. **The frame line** is `|`-separated `key value [unit]` segments in any order (`apps/launcher/src/frameline.rs`):
`fps`, `ms` (whole frame), `gpu`, `draws`, `dropped`, `upload`, `xlate` (list translation), `upscale`, `wait`, and
a resolution segment `internal -> output upscaler words` (or just the output), e.g.
`t 12s | fps 30.8 | ms 32.5 | gpu 21.0 ms | draws 4,512 | dropped 0 | upload 1.9 ms | xlate 2.4 ms | upscale 1.1 ms | wait 6.0 ms | 1707x960 -> 2560x1440 dlss quality`.

**Retention:** `diagnostics.retention_days` (int, 0-3650, default 14, 0 = keep forever, Live). At start the launcher
deletes every game's session folders whose start (from the folder name) is older than that game's setting, and the
current game's again after each run; it never deletes the running session, and never anything under `sessions/`
whose name isn't a session stamp. Tests: `session::tests` (stamps, `session.toml` written and parsed, retention,
delete only session folders, crash and capture listing), `tests/launch.rs` (a launch in a session with `fake-layer`
writing `summary.toml` and a crash report through `REBUILD_SESSION_DIR`), the headless smoke test (every section in
both themes; an old session pruned on restart, a recent one kept).

### Recommended for this PC (owner, 2026-11-28)

The owner asked for the launcher "to recommend a profile to run the game at based on the specs of the machine
automatically". Code: `apps/launcher/src/recommend.rs` (detection, the pure mapping, first run), the strip on Play,
"This PC" on Diagnostics, `rebuild-launcher --recommend` (prints it, no window).

**Decided: a recommendation is not a test profile.** The built-ins (`docs/design/test-profiles.md`) impose
`limits.*` on purpose, to make this machine behave like a weaker one; applied on a real player's machine they would
only slow the game. A recommendation sets the same eight keys a built-in does (`PROFILE_KEYS`, so nothing of a test
profile is left behind when it is applied after one) with **every limit 0**: output (`display.width/height`) and
upscaler (`upscaler.backend/mode`). Nothing else is touched (window mode, window size, vsync, frame rate and every
other setting are kept). `profile` becomes `recommended`; the launcher shows it as "Recommended for this PC", and
"(edited)" once a value differs.

**Detection** (Windows, the `windows` crate already in use; DXGI only, **no D3D12 device**, so it is safe while
`agents\MEASURING` exists). Read once per start, on a thread:

| fact | how |
|:--|:--|
| GPU name, vendor, dedicated VRAM | every DXGI adapter (`EnumAdapters1`), software adapters and the Basic Render Driver (WARP) skipped; the one with the most dedicated VRAM |
| RAM | installed (`GetPhysicallyInstalledSystemMemory`), else usable total (`GlobalMemoryStatusEx`) |
| cores | physical = processor-core records, logical = their group masks (`GetLogicalProcessorInformationEx`) |
| display | the primary display's current mode in physical pixels (`EnumDisplaySettingsW`, unaffected by DPI scaling) |

**The mapping** (`recommend::recommend`, pure, unit-tested with a table of machines):

1. **The GPU's class.** Dedicated VRAM gives a class (with half a GiB of slack: DXGI reports a 12 GB card as
   12,282 MiB). A known model **caps** it, so a small chip with a lot of VRAM (RTX 3060 12 GB, Arc A770 16 GB) isn't
   over-rated; a laptop part is one class lower. Under 2 GiB dedicated (an integrated GPU's carve-out), Intel UHD /
   Iris, or no hardware GPU at all is the lowest class. The models are placed against the minimum spec (RTX 4070 Ti):
   roughly 0.9x and up is 4K, 0.5x is 1440p, 0.3x is 1080p (the list is in `recommend::MODELS`).

   | class | VRAM | known models (examples) | output | mode | internal |
   |:--|:--|:--|:--|:--|:--|
   | 4K (the minimum spec) | ≥ 11.5 GiB | RTX 4070 Ti and up, 3090, 5070 Ti and up; RX 7900 XT/XTX, 9070 XT | 3840 x 2160 | Quality | 2560 x 1440 (the console's own) |
   | 1440p | ≥ 7.5 GiB | RTX 4070, 5070, 3070/3080, 4060 Ti, 2080 Ti; RX 6800-6950, 7700-7900 GRE, 9070, 6700 XT | 2560 x 1440 | Quality | 1707 x 960 |
   | 1080p | ≥ 5.5 GiB | RTX 2060-2080, 3060, 4060, 5060; GTX 1070/1080; RX 5600-6700, 7600; Arc A750/A770, B570/B580 | 1920 x 1080 | Quality | 1280 x 720 |
   | lowest | less, or integrated | iGPUs, RTX 3050, GTX 16/10 series below 1070, RX 6500 and older, Arc A380 | 1280 x 720 | Performance | 640 x 360 |

2. **The output is never above the display.** When the display is smaller than the class's output, the output is
   the largest 16:9 size that fits (3840 x 2160, 2560 x 1440, 1920 x 1080, 1600 x 900, 1280 x 720; an ultrawide
   3440 x 1440 gets 2560 x 1440; below 720p, the largest 16:9 size inside the display).
3. **The mode keeps the class's internal pixels.** Each class carries a number of internal pixels (its output through
   its mode); the mode is the one with the most internal pixels within that (DLAA / native AA 1.0, Quality 1.5,
   Balanced 1.7, Performance 2.0, Ultra Performance 3.0). So a 4070 Ti on a 1440p display gets **1440p DLAA**
   (2560 x 1440 internal, the same rendering cost as 4K Quality) instead of idling at 1707 x 960.
4. **The backend:** DLSS on an NVIDIA **RTX** card (DLSS needs tensor cores), FSR on everything else (AMD, Intel, a
   GTX). Explicit, not `auto`, so a GTX never asks for DLSS. Without a DLSS DLL the layer falls back to FSR, and Play
   warns, as before.
5. **Notes, not changes:** RAM and the CPU don't change the output (the resolution doesn't fix them); under 15 GiB
   of RAM, under 31 GiB, and under 12 logical processors each add a one-line note, shown on hover and on Diagnostics.

The machine table (`recommend::tests::the_machine_table`):

| machine | display | recommended |
|:--|:--|:--|
| RTX 4070 Ti 12 GB, 64 GB (the reference) | 3840 x 2160 | 4K · DLSS Quality |
| the same | 2560 x 1440 / 1920 x 1080 / 3440 x 1440 | 1440p · DLSS DLAA / 1080p · DLSS DLAA / 1440p · DLSS DLAA |
| RTX 4090 24 GB | 4K | 4K · DLSS Quality |
| RTX 3070 8 GB, 16 GB | 4K / 1080p / 1920 x 1200 / 1680 x 1050 | 1440p · DLSS Quality / 1080p · DLSS Quality / 1080p · DLSS Quality / 900p · DLSS DLAA |
| unknown 8 GB card | 4K | 1440p · FSR Quality |
| RX 5600 XT 6 GB (and an unknown 6 GB AMD) | 1440p | 1080p · FSR Quality |
| RX 7900 XTX 24 GB | 4K | 4K · FSR Quality |
| RTX 3060 12 GB / Arc A770 16 GB (capped by model) | 4K | 1080p · DLSS Quality / 1080p · FSR Quality |
| RTX 4090 Laptop 16 GB | 4K | 1440p · DLSS Quality |
| GTX 1660 Super 6 GB | 1080p | 720p · FSR Performance |
| Intel UHD 770 / Iris Xe / AMD APU / no hardware GPU | any | 720p · FSR Performance |
| Intel UHD on a 1024 x 600 display | 1024 x 600 | 1024 x 576 · FSR Balanced |

**In the launcher:**

- **First run, automatically.** When a game's settings have never chosen an output or upscaler (none of the eight
  keys is explicit, the profile is the default one) and the launcher hasn't done it for that game before, it applies
  the recommendation and saves it, with a toast "Set up for this PC: …". It writes `first_run_recommendation =
  "<name>"` at the top of that game's `rebuild.toml`, so it happens **once per game**: not again on a restart, not
  after the user resets the keys. It never runs with unsaved edits, and **never overwrites a value the user set**
  (any of the eight keys explicit, or a profile chosen, means it is not a first run).
- **Play** always shows "Recommended for this PC: `<name>`" with the detected specs as its one-line reason (the why
  and any notes on hover) and **Apply**, or "In use" when every value is in force. Apply works like applying a
  built-in: the eight keys change, everything else is kept, and the save bar appears. The profile picker lists it
  first, under "For this PC".
- **Diagnostics** has a "This PC" card: GPU and vendor, VRAM, RAM, cores and logical processors, the display, the
  recommendation with its internal resolution, the why and the notes.
- Nothing asks for frames: detection is one thread at start; while it is pending the launcher repaints at the same
  10 Hz as the system check, then idles again.

Tests: `recommend::tests` (the machine table, the reference's lines, model matching, VRAM slack, apply clears limits
and keeps the rest, first run only once and never over a choice, detection on this machine), and the smoke test
`a_new_game_gets_the_recommendation_once_and_the_users_choice_stays` (a new game set up and saved; the user's 1080p
kept through new specs and a restart; a built-in chosen first is kept; Apply restores it unsaved; Play and
Diagnostics draw).

## Mods

A mod is a folder under `mods/<name>/` whose files replace title files by path, through Build 3's overlay
file system (title paths → game folder, with a write overlay on top). Enabled mods are layered in list
order, so later mods win, and the game folder is never modified. The launcher shows which mod wins each
contested file. Script or code mods are out of scope.

## The in-game menu (after Build 5)

- Drawn by our present path onto the back buffer, never into the title's textures.
- `menu.key` (default F10) toggles it.
- **Input isolation**, the fork's rule (`docs/design/debug-menu.md` there): while open, the title sees
  neutral input. On close, input stays neutral until every key and button is released, so the closing key
  never reaches the title.
- Shows the frame line, every **live** registry option, and the mod list (read-only in game; changes apply
  on the next launch).

## Order

| # | step | closes when |
|:--:|:--|:--|
| C1 | `crates/config`: registry, `rebuild.toml` load/save, validation, defaults (lead) | unit tests: defaults valid, round trip, bad values refused with a reason, unknown keys reported |
| C2 | the layer reads `rebuild.toml` at start; every existing switch moves into the registry (lead) | `gputest`/benches take their options from it |
| L1 | launcher shell: tabs, Features generated from the registry, save/load profiles (lane `launcher`) | launcher writes a `rebuild.toml` that `config` validates |
| L2 | setup and system check (lane `launcher`) | each check has a pass/fail and a fix line |
| L3 | title settings: apply `title.*` to a copy of `settings.xml` in the write overlay (lane `launcher`) | the original file's hash is unchanged |
| L4 | mods tab (lane `launcher`); the overlay file system itself is Build 3 (lead) | conflicts listed; order respected |
| L5 | in-game menu (lead, after Build 5) | toggles live options; input isolation test |
| L6 | redesign: top bar, dark/light themes, all sections, Runs & test matrix, session diagnostics with retention (lane `launcher`) | every mockup screen exists in both themes; sessions past the retention are deleted at start; idle cost still 0 |
