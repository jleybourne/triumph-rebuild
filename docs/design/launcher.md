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
| `display` | output size, window mode, vsync, frame cap | `display.width`, `display.window`, `display.vsync` |
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
