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
