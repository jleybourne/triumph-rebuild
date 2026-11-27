# agents/launcher.md — lane `launcher`: Triumph Launcher

Lane rules: [`secondary-agent.md`](../secondary-agent.md) section 5. Design: [`docs/design/launcher.md`](../docs/design/launcher.md).
The user-facing name is **Triumph Launcher** (window title, brand, title bar, About, the exe's version info).
Package and crate names stay as the plan has them: `apps/launcher`, package `rebuild-launcher`.
egui/eframe 0.31 (glow). No network crate, no telemetry, no browser engine.

## Built

| # | task | state | proof |
|:--:|:--|:--|:--|
| U1 | shell; every settings widget generated from `rebuild_config::REGISTRY`; profiles | ✅ | `model::tests::every_registry_entry_round_trips_through_the_ui_model`: all 18 entries edited through the UI model, saved, read back by `Config::parse` with **0 problems** |
| U2 | first-run setup and system check (8 checks, each pass/fail/skipped with a one-line fix) | ✅ | 4 unit tests with fakes (each failure caught alone, with a fix line); no device while the lock exists. On this machine: **8 / 8 pass** |
| U3 | `title.*` written into a copy of `settings.xml` in the launcher's overlay | ✅ | synthetic test and the real file (via `REBUILD_TITLE_SETTINGS`): **SHA-256 unchanged** before and after |
| U4 | mods: list, enable, reorder (drag or buttons), conflicts (later wins, case-insensitive) through lane `vfs`'s `ModSet::conflicts` | ✅ | 3 unit tests |
| U5 | Play: launch `rebuild.exe --config <path>`, tail the frame-line log; proof binaries run as they are | ✅ (fake child) | 4 integration tests with `fake-layer` standing in for `rebuild.exe` and for a proof binary; `gputest` never run (lead confirmed) |
| — | **library**: more than one game; detector trait; add / remove / switch | ✅ | `library::tests` (3): add, switch, persistence across a restart, per-game isolation (B byte-identical after A changes), a game update keeps settings, unsupported folders (wrong family, no exe, empty, missing) say why and add nothing, remove leaves the game folder's tree hash (contents + `fs::metadata` size and mtime) unchanged, re-add restores or sets aside |
| — | **per-game store**: never replaced or lost | ✅ | `store::tests` (3): explicit values survive a change of default, unknown keys and a newer schema survive a round trip, a cut-short file is recovered from `.bak` |
| — | **look**: own dark theme, Inter, Phosphor, borderless window | ✅ | `type_check` (a pangram in every text style: no blank or missing glyph), `icons` (every icon has a Phosphor glyph; inline icons not shadowed by Inter), `smoke` (the whole flow headless, no panic) |
| L6 | **redesign** (owner-approved Direction B): top bar, dark and light themes, every screen (Play, Running, Profiles, Settings, Runs & test matrix, Mods, Diagnostics, Library), **session diagnostics** with retention | ✅ | `session::tests` (5: UTC stamps, `session.toml` written at launch and on exit and parsed, retention deletes old / keeps recent / 0 keeps all / never the running one / only session folders, delete refuses anything else, crashes and captures listed), `frameline::tests` (4), `runs::tests` (2), `verify::tests` (1), `prefs::tests` (theme round trip), `model::tests` (every entry under exactly one pill; search; pending changes), `config` (retention and theme entries), `tests/launch.rs` (a launch in a session: `REBUILD_SESSION_DIR`, frame line inside it, `summary.toml` and a crash report from `fake-layer`), `smoke` (every section and the Running view in **both themes**, settings pills and search, an old session pruned on restart and a recent one kept, the theme saved launcher-wide and never an unsaved change), `type_check` (the approved palettes, contrast ≥ 4.5 for muted text and chips), `idle` (unchanged: 0 frames when idle) |

## Measured

```
cargo test --release                 148 green in the workspace; launcher 34 (27 unit, 4 launch, 3 UI: smoke, pangram, icons)
rebuild-launcher --check             8 checks, 0 failed
  GPU              NVIDIA GeForce RTX 4070 Ti
  Driver           610.88 (32.0.16.1088)
  VRAM             11.7 GiB dedicated
  Tiled resources  tier 4
  Shader model     6.8
  Resizable BAR    GPU upload heaps supported
  DXC              Windows SDK 10.0.26100.0 dxc.exe
title settings.xml SHA-256 before = after = 2f8d9deb…c0d1 (copy written to a temp overlay, then deleted)
```

## Per-game layout (owner requirement: settings are saved per game and never replaced or lost)

```text
%APPDATA%\triumph-rebuild\                 the launcher's own app-data (REBUILD_LAUNCHER_HOME overrides)
  library.toml                             games: id, folder, name, detector; selected
  games\<id>\                              one folder per game; id = FNV-1a 64 of the folder's absolute path
    rebuild.toml   (+ rebuild.toml.bak)    its settings: game.dir, values, [[mods]] order and enables, profile
    profiles\<name>.toml                   its profiles
    mods\<name>\                           its mods
    overlay\title\settings.xml             the title settings copy, regenerated from rebuild.toml at every launch
    frame-line.log                         its frame line
  games\<id>.previous-<unix seconds>\      old settings set aside when a game is re-added "fresh" (never deleted)
```

- **Never in the game folder**, never one shared file. The id comes from the folder path, so a game update
  (its files changing) keeps its settings.
- **Never replaced** (`store.rs`): a key the file held or the user set is written explicitly even when it
  equals today's default, so a new default never resets it; defaults only fill missing keys; "reset to
  default" is the one action that removes a key. Unknown settings keys and unknown top-level keys are written
  back as they were. `schema = 1` at the top; migration is forward-only and removes nothing; a newer schema
  number is never lowered.
- **Safe writes:** temp file, then rename; the previous good version is kept as `.bak` (a corrupt file never
  replaces a good `.bak`); a file that does not parse is recovered from `.bak` with a toast.
- **Remove a game:** removes the library entry; its data is kept by default and offered back ("restore
  previous settings") when the folder is added again. "Also delete the launcher's data" removes only
  `games\<id>\`, after a check that it is a direct child of `games\` and not the game folder.
- **Detectors:** `library::Detector` (`id`, `detect(dir)` → supported / unsupported with a reason / not mine).
  Today one: `GdkPackage` (MicrosoftGame.config naming an executable with `TargetDeviceFamily="Scarlett"`
  that exists). Nothing title-specific is in source; names come from the game's own files at run time.

## Visual design: L6, Direction B (owner-approved 2026-11-27)

Flat panels under a top bar, in **dark and light**; no moving background (the earlier glass, glow and drifting
gradient are gone, so the launcher draws nothing while idle). All colours, radii, spacing and type are tokens in
`apps/launcher/src/ui/theme.rs`; colours are functions of the current theme (`th::p()`), switched by `th::set_dark`.

### Tokens

| token | dark | light | use |
|:--|:--|:--|:--|
| `bg` | `#0E1014` | `#F5F6F8` | window |
| `header` | `#12151B` | `#FFFFFF` | top bar, save bar |
| `panel` | `#151A23` | `#FFFFFF` | cards |
| `control` / `control_hi` | `#1B212C` / `#232A37` | `#F2F4F7` / `#E8ECF1` | inputs, quiet buttons / hover |
| `line` / `line_hi` | `#262C37` / `#363E4C` | `#DCE1E8` / `#C3CAD5` | hairlines / hover, focus |
| `text` / `text_2` / `muted` | `#E8EAED` / `#B8BEC8` / `#8C95A3` | `#14171C` / `#363D48` / `#5B6472` | primary / secondary / captions, keys, "—" |
| `accent` | `#3D7BFF` | `#2F6BEB` | Launch, the active section's underline, selection |
| `ok` on `ok_bg` | `#9BE3B0` on `#17321F` | `#1D6B36` on `#E3F5E8` | Live, Ready, passes |
| `warn` on `warn_bg` | `#F5C36B` on `#2E2714` | `#7A4F00` on `#FFF2D6` | Experimental, VRAM "Build 5", warnings |
| `danger` | `#FF8A8A` | `#B42318` | failures, crash reports |
| radii | 6 · 8 · 10 · pill | | chips · controls · cards · status pill |

Type: **Inter 3.019** (Regular / Medium / SemiBold / Bold; headings Bold 22, card titles SemiBold 15, body 13.5,
small 12.5, captions SemiBold 10.5 upper case), **Hack** for paths, keys and logs, **Phosphor** icons in their own family.

### Screens

| screen | contents |
|:--|:--|
| top bar | logo and "Triumph Rebuild"; Play · Profiles · Settings · Runs · Mods · Diagnostics (active underlined in the accent; `Ctrl+1…6`); status pill "Ready" / "Running · `<profile>` · mm:ss" (with the matrix's n/total); theme switch (sun / moon); minimise, maximise, close. The bar is the drag area |
| Play | header band: "Ready to launch", the game's name ("Change game" opens the library), "Profile: … · output · upscaler · limits or no limits", **Launch**, the profile dropdown (built-ins and yours), Test matrix; cards Last run (the newest session's average fps, or "—"), Target (30 fps vs the 7.6 baseline), GPU (name, VRAM, driver from the check), System check (warnings, e.g. "DLSS DLL not set → FSR") |
| Running | tiles fps, frame time (budget 33.3 ms, amber when over), draws (+dropped), resolution (internal → output, upscaler); the frame-time graph (last 120 s, dashed line at 33.3 ms); where the frame goes (upload, list translation, GPU, upscaler, waiting: bars as shares of the frame, "—" until the frame line carries them); the frame line; Capture frame (`capture.request` in the session); Stop |
| Profiles | hardware tiers as cards (purpose, chips for output, upscaler and each limit; VRAM chips amber "Build 5"); stress tests in their own row; your profiles; "Limits in force" for the selected card (or what is in force now) with Use, Save a copy, Export .toml; save current as, Import .toml, Stable only |
| Settings | search over label, key and help; pills Display & upscaler · Hardware limits · Title settings · Memory & shaders · In-game menu · Diagnostics · Files & paths · Launcher (every file or folder setting is under Files & paths); a row per setting with help, a Live / Restart / Experimental badge, the generated control and reset; the save bar "N changes · M take effect on next launch", Discard, Save to profile (also writes the active user profile's file) |
| Runs | test matrix: profile checkboxes, run length 60 s / 3 min / 10 min, measure from title start / after loading, "Run N profiles" (each its own session, one after another; the launcher stays minimised); the last matrix side by side (avg fps, 1% low, frame ms, GPU ms, upload ms, vs 30 fps) with Export CSV; every run with Compare (two or more side by side), Log, open folder, delete |
| Mods | load order with switches, up / down, conflict and "wins" chips; conflicts with the winner; "your saves and settings" (the write overlay, open); game folder read-only check, **Verify now** (metadata only, on a thread) |
| Diagnostics | system check with Run again; crash reports (View, Copy for a bug report: launcher version, profile, limits, changed settings, the report); logs in tabs (frame line, `session.toml`, `summary.toml`) for any session; frame captures; the D3D12 debug layer switch and the retention setting; the session list; the proof-binary runner; About (where things live, credits) |
| Library | game cards (Play, Select, Remove) and an Add tile; add and remove dialogs as before |

Keyboard: `Ctrl+1…6` sections, `Ctrl+S` save, `Ctrl+Enter` launch, `F5` system check; every custom control takes
focus and draws a focus ring.

### Session diagnostics

A folder per launch, `games\<id>\sessions\<UTC stamp>-<profile>\`, passed to the child as `REBUILD_SESSION_DIR`,
with `REBUILD_FRAME_LOG` = `<session>\frame-line.log`. The launcher writes `session.toml` ([session] at launch with
every effective setting and the limits in force; [end] with the exit status). Retention `diagnostics.retention_days`
(default 14, 0 = forever): pruned at start for every game and after each run. The full contract (files, variables,
`summary.toml`, the frame-line fields) is in `docs/design/launcher.md`, "L6 built".

## Screenshots (outside git)

`rebuild-launcher --shots <dir> --size WxH` renders every screen in both themes (18 PNGs) from a fixture (placeholder names "Your title",
"Second title", demo data including demo sessions and a demo matrix, labelled "SCREENSHOT FIXTURE · DEMO DATA" in the top bar). With `REBUILD_GAME_DIR`
set, the first game points at that folder read-only, so its key art appears at run time.

| pass | folder | notes |
|:--|:--|:--|
| 1 | `D:\victory-data\launcher-shots\pass-1` | first theme; grain far too strong, content overflowed |
| 2 | `D:\victory-data\launcher-shots\pass-2\1280x800` | library, per-game; grain still on, harness panic at the end |
| 3 | `D:\victory-data\launcher-shots\pass-3` | grain gone; Phosphor-first fonts broke body text |
| 3b | `D:\victory-data\launcher-shots\pass-3b\{1280x800,1920x1080}` | text and icons fixed, hero scrim; the display is at 150 %, so 1280x800 is 1920x1200 px |
| 4 | `D:\victory-data\launcher-shots\pass-4\{1280x800,1920x1080}` | card hairline dropped, a top scrim behind the window controls on Play, caret up/down for mod order, equal card baselines on hover |
| 5 | `D:\victory-data\launcher-shots\pass-5-l6` | L6: Direction B top bar, `dark-*` and `light-*` for Play, Running, Profiles, Settings, Runs, Mods, Diagnostics, Library (no D3D12 device: run with `REBUILD_MEASURE_LOCK`) |

## For the lead

1. **U5 asks for `gputest --bare`; section 2 forbids helpers running `gputest`.** The proof path is built
   (`play::Running::start_proof`: any binary, its output tailed through the frame log) and tested with a fake
   child (`fake-layer`). `gputest --bare` itself was never run by this lane. Point the Play tab's
   "Run a proof binary" at `gputest.exe` with `--bare` to see it.
2. **Registry and config proposals** (`crates/config` is read-only for this lane):
   - `Config::parse` reports unknown settings keys as problems and drops them; the launcher keeps them
     itself (`store.rs`). If the layer should keep them too, `Config` could carry an `extra` table.
   - `Config::set` stores nothing for a value equal to the default, so a later default change silently moves
     such a user's value. The launcher works round this by writing explicit keys itself; a `Config` flag for
     "explicit" would let the layer do the same.
   - A `schema` integer at the top of `rebuild.toml` (the launcher writes `schema = 1`; `Config::parse`
     ignores it today).
   - A per-title support list belongs in config, not code: today the detector accepts any Xbox Series GDK
     package.
   - `title.settings_source` (Path): where the title's own `settings.xml` is. Today: `REBUILD_TITLE_SETTINGS`,
     else the first `settings.xml` under `game.dir`. The fork's file lives in the title's *storage*, not the
     game folder, so the search alone will not find it.
   - `diagnostics.frame_log_path` (Path), or a fixed convention: the launcher passes the path to the layer as
     `REBUILD_FRAME_LOG` (default `<home>/frame-line.log`) until the layer reads it from the registry.
   - A field on `Setting` naming the `settings.xml` element (`xml: Option<&str>`), replacing
     `title::XML_NAMES` (`title.fsr_quality` → `fsrQuality`). A test fails if a `title.*` entry has no row.
   - Help text for `display.height` and `display.window` (empty today; the launcher shows nothing there).
3. **`rebuild.exe` ignores its arguments** (`apps/rebuild/src/main.rs`): `--config` lands with C2.
4. **Overlay layout:** the edited copy goes to `<home>/overlay/title/settings.xml`. Lane `vfs` should map the
   title's storage path for `settings.xml` onto it; the lead decides one overlay root for both lanes
   (lane `vfs` suggests `%APPDATA%\triumph-rebuild\overlay`, which is the launcher's default `<home>/overlay`).
5. **VRAM floor** in the check is a provisional 8 GiB (`syscheck::MIN_VRAM_GIB`) until the owner sets the
   minimum spec in `plan.md`.
6. Minor, `crates/config/src/lib.rs` line 349: `assert!(c.problems[0].contains("black screen") || c.problems.iter().any(..))`
   is the same check twice; the first half adds nothing.
7. The launcher honours the measurement lock through `REBUILD_MEASURE_LOCK=<file>` (no D3D12 device while it
   exists); on the dev machine set it to `D:\Developer\triumph-rebuild\agents\MEASURING`.

8. **L6, for the layer and `runtitle` (the session contract, `docs/design/launcher.md` "L6 built"):** read
   `REBUILD_SESSION_DIR`; write `frame-line.log` (already `REBUILD_FRAME_LOG`), `summary.toml` at exit
   (`[summary]` avg_fps, low1_fps, frame_ms, gpu_ms, upload_ms, frames, seconds, measured_from), `crash-*.txt` per
   crash, and captures into `captures/`; watch for `capture.request` and delete it after capturing. For matrix runs
   honour `REBUILD_RUN_SECONDS` (write the summary and exit) and `REBUILD_MEASURE_FROM` (`title_start` /
   `after_loading`). Frame-line fields the Running view reads: `fps`, `ms`, `gpu`, `draws`, `dropped`, `upload`,
   `xlate`, `upscale`, `wait`, and `internal -> output upscaler`; any missing one shows "—".
9. **L6 registry entries added** in `crates/config` (owner-approved): `diagnostics.retention_days` (int 0-3650,
   default 14, Live) and `launcher.theme` (dark / light, default dark, Live; the launcher saves it launcher-wide in
   `<app home>\launcher.toml`, so it is the same for every game).
10. When `diagnostics.frame_log_path` is set, the launcher tails that path instead of `<session>\frame-line.log` and
    still passes `REBUILD_FRAME_LOG` = that path; a matrix run clears it so each run's frame line stays in its session.

## Next

- C2 (lead): `rebuild.exe --config`; then the Play tab launches the real layer.
- Layer side of the session contract (item 8): until it lands, sessions hold `session.toml` and whatever the frame line writes; summaries are averaged from the frame line.
