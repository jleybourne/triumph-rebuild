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

## Visual design (owner request, 2026-11-27)

The target is a top-tier commercial game launcher: dark, cinematic, confident; nothing that reads as a
default egui app. All colours, radii, spacing and type live as named tokens in `apps/launcher/src/ui/theme.rs`.

### Tokens

| token | value | use |
|:--|:--|:--|
| `BG_BASE` | `#07080C` | window base, under everything |
| `BG_SIDEBAR` | `#0A0C11` @ 92 % | sidebar |
| `GLASS` | `#0C0E14` @ 80 % | cards, over the animated background |
| `SURFACE_2` | `#161A23` | inputs, rows, raised controls |
| `SURFACE_3` | `#1D2230` | hover |
| `HAIRLINE` | white @ 7 % | 1 px borders |
| `HAIRLINE_HI` | white @ 12 % | top highlight on glass, focus |
| `TEXT` | `#F2F4F8` | primary text |
| `TEXT_2` | `#B6BCCA` | secondary text, help |
| `TEXT_3` | `#8C93A5` | captions, registry keys |
| `ACCENT` | `#7C6CFF` | the one accent (violet) |
| `ACCENT_2` | `#4F8BFF` | gradient partner on primary actions only |
| `SUCCESS` | `#3DDC97` | pass, live |
| `WARNING` | `#FFB547` | experimental, skipped |
| `DANGER` | `#FF5C6C` | fail, refusals, close hover |
| radii | 8 · 10 · 14 · pill | small controls · inputs/buttons · cards/panels · pills |
| spacing | 8 px grid: 8 · 16 · 24 · 32 · 48 | every margin and gap |
| shadow | 0 8 24 black @ 45 % | cards; primary button glow is `ACCENT` @ 40 % |
| motion | 120 ms hover · 180 ms press/toggle · 320 ms page · 40 ms stagger, cubic-out | |

Type: **Inter 3.019** (OFL-1.1, static Regular / Medium / SemiBold / Bold, in `apps/launcher/assets/fonts`),
mono **Hack** (egui's bundled face) for paths and logs, icons **Phosphor** (MIT, `egui-phosphor`) in their own
font family (Inter has glyphs at six Phosphor code points; Phosphor has blank Latin glyphs, so neither can
lead a shared family). Credits in `CREDITS.md`. No grain or texture anywhere.

| style | face | size | tracking |
|:--|:--|--:|--:|
| display (hero title) | Inter Bold | 40 | −1.0 |
| numerals (stats) | Inter Bold | 36 | −0.8 |
| h1 (page title) | Inter SemiBold | 26 | −0.4 |
| h2 (card title) | Inter SemiBold | 16 | −0.1 |
| body | Inter Regular | 14 | 0 |
| small (help) | Inter Regular | 12.5 | 0 |
| caption / pill | Inter SemiBold | 10.5 | +0.6, upper case |
| mono | Hack | 12.5 | 0 |

### Screens

| screen | contents |
|:--|:--|
| window | borderless; our own 40 px title bar (drag, minimise, maximise, close in theme); DWM rounded corners on Windows 11; edge resize; 600 ms fade-in splash |
| sidebar | logo mark and "Triumph Launcher"; the selected game's chip (opens the library); 9 items (icon + label) with a sliding accent indicator; collapses to icons (animated width); profile chip at the bottom |
| Library | game cards (art, name, supported pill, truncated path, Play / Select / Remove) in an equal-height grid, a dashed Add tile; add dialog (supported, or not supported yet with the reason; restore previous settings); remove dialog (keep or delete the launcher's data) |
| Play | full-bleed hero: run-time key art from the game folder (its `MicrosoftGame.config` splash image, read-only) with a slow drift, else an animated gradient glow; the title's name in display type; system-ready pill with glow; the large gradient **Launch** button; stat numerals (last fps, VRAM, driver); first-run setup card; the console panel with the frame-line tail |
| Display & upscaler, Title settings, Features, Diagnostics | page header; one glass card per registry group; a row per setting: label, help text, `LIVE` / `RESTART` pill, amber `EXPERIMENTAL` pill, the generated control (switch, slider, segmented control or dropdown, path field with Browse), a reset button when changed, refusal reasons in red |
| Diagnostics | also the system check as an animated checklist (skeleton rows while it runs, then pass/fail marks pop in, staggered, with a fix line each) |
| Mods | reorderable cards with drag handles, enable switches, conflict badges per mod, and a contested-files card naming the winner; friendly empty state |
| Profiles | profile cards (load), save-as, stable-only switch, import / export; empty state |
| About | wordmark, licences and credits, where every file lives |
| overlays | toasts (saved, applied, launched, errors), bottom-right, fade and slide |

Keyboard: `Ctrl+1…9` pages, `Ctrl+S` save, `Ctrl+Enter` launch, `F5` system check, `Tab` / `Shift+Tab`
through every control (all custom widgets take focus and draw a focus ring), `Space` / `Enter` activate.

## Screenshots (outside git)

`rebuild-launcher --shots <dir> --size WxH` renders 14 pages from a fixture (placeholder names "Your title",
"Second title", demo data, labelled "SCREENSHOT FIXTURE · DEMO DATA" in the title bar). With `REBUILD_GAME_DIR`
set, the first game points at that folder read-only, so its key art appears at run time.

| pass | folder | notes |
|:--|:--|:--|
| 1 | `D:\victory-data\launcher-shots\pass-1` | first theme; grain far too strong, content overflowed |
| 2 | `D:\victory-data\launcher-shots\pass-2\1280x800` | library, per-game; grain still on, harness panic at the end |
| 3 | `D:\victory-data\launcher-shots\pass-3` | grain gone; Phosphor-first fonts broke body text |
| 3b | `D:\victory-data\launcher-shots\pass-3b\{1280x800,1920x1080}` | text and icons fixed, hero scrim; the display is at 150 %, so 1280x800 is 1920x1200 px |
| 4 | `D:\victory-data\launcher-shots\pass-4\{1280x800,1920x1080}` | card hairline dropped, a top scrim behind the window controls on Play, caret up/down for mod order, equal card baselines on hover |

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

## Next

- C2 (lead): `rebuild.exe --config`; then the Play tab launches the real layer.
