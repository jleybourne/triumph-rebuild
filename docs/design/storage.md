# Design: where the launcher and the layer keep their files

Status: **built 2026-11-28 (owner question, lane `storage`).** Code: `crates/storage` (package
`rebuild-storage`). It is the one owner of every per-user path. The launcher (`apps/launcher`), the layer
(`apps/rebuild`), the runtime's drives (`crates/runtime/src/drives.rs`), DirectStorage's BCPack cache and tables
(`crates/dstorage`) and the pipeline caches (`crates/pipelines`) all ask it. **No other code reads `APPDATA` or
`LOCALAPPDATA`.**

The owner asked: *"should there be a games folder that holds saved data / any caches / game files that need to
compile on disk inside of the launcher's actual running folder?"* Yes. By default it is a `data` folder beside the
launcher, with one folder per game. The game's own files are never part of it. They are only read, in place.

## The layout

```text
<data>\
  launcher.toml                     launcher-wide prefs: launcher.theme, storage.cache_dir
  library.toml                      the game library (id, folder, name, detector; the selected game)
  bcpack-tables.bin                 (local root) the BCPack format tables, if you supplied them (never in git)
  games\<game-id>\
    settings.toml                   that game's settings (was rebuild.toml), passed to the layer with --config
    profiles\<name>.toml            its saved profiles
    mods\<name>\                    its mods
    saves\<user>\<scid>\*.gsc       XGameSave containers, one folder per user (xuid, 16 hex digits): small, back these up
    storage\                        the title's S: drive (persistent local storage, kept between runs; the vfs
                                    overlay's own layout, files\…)
    overlay\                        the write overlay: G\ (writes over the package), T\ (emptied every run),
                                    title\settings.xml (the launcher's copy of the title's settings)
    backups\before-import-*.zip     taken automatically before an import replaces anything
    cache\<build>\shaders\          the shader library the launcher builds on first Play (shader-library.md)
    cache\<build>\pipelines\        driver pipeline cache, session record, on-demand translations, rect GS
    cache\<build>\images\           prepared copies of the title's images (relocated, patched)
    cache\bcpack\                   decoded BCPack textures (content-keyed, so kept across builds)
  logs\<game-id>\<stamp>-<profile>\ (local root) one folder per session: frame line, session.toml, summary.toml, crash reports, captures
  shared\<kind>\                    (cache root) caches of tools run without a game (benches)
```

Code reaches these through `rebuild_storage::Storage` (the roots) and `GameStore` (one game):
`settings_toml()`, `saves_dir(user)`, `storage_dir()`, `overlay_dir()`, `shaders_dir()`, `pipelines_dir()`,
`images_dir()`, `bcpack_cache_dir()`, `sessions_dir()`, `backups_dir()`. The save editor's entry point is
**`rebuild_storage::saves_dir(game_id, user) -> PathBuf`**. It returns `<data>\games\<id>\saves\<user as 16 hex>`.
Under it are `<scid>\<container>.gsc` in `crates/gdk`'s container format.

### The roots: portable by default, per-user when it can't be

| | how it is chosen | user data | logs | caches |
|:--|:--|:--|:--|:--|
| **portable** | the launcher's folder is writable: `<exe dir>\data` is made and a file can be written in it | `<exe dir>\data` | same | same |
| **per-user** | it isn't (Program Files, a read-only share), or the exe is in a cargo `target` folder | `%APPDATA%\triumph-rebuild` | `%LOCALAPPDATA%\triumph-rebuild` | `%LOCALAPPDATA%\triumph-rebuild` |
| **explicit** | `REBUILD_DATA_DIR` (and optionally `REBUILD_LOCAL_DIR`) | that folder | `REBUILD_LOCAL_DIR` or the same | same |

The caches move on their own with `REBUILD_CACHE_DIR`, or the launcher setting **`storage.cache_dir`** (Settings →
Files & paths, saved launcher-wide in `launcher.toml`, applied when the launcher next starts), for example onto a
bigger drive. Saves, settings and logs never move with them. A cache folder inside `games\` (among the saves) is
refused with a note, and the default is used.

Why:

- **Portable first.** The owner's question was about the launcher's own folder. One folder means one thing to move,
  back up or delete, and nothing is left elsewhere on the machine. Most people unzip a launcher like this into a
  folder they own.
- **Per-user when the folder is read-only.** An installer in Program Files can't write beside itself without
  elevation. Writing there is exactly what Windows forbids. `%APPDATA%` holds what a user would miss (settings,
  saves). `%LOCALAPPDATA%` holds what is big or machine-specific (caches, which are GPU-driver and build
  specific, and logs with captures), so a roaming profile never syncs gigabytes.
- **Never inside a cargo `target` folder.** The dev build runs from `target\release`, and `cargo clean` deletes
  that folder. Saves must not sit in a build's output. Cargo marks the folder with `CACHEDIR.TAG` (the standard
  "this is a cache" tag), and resolution checks for it: an exe with the tag in one of its five nearest parent
  folders is never portable.
- **The child gets the parent's roots.** The launcher passes `REBUILD_DATA_DIR`, `REBUILD_LOCAL_DIR`,
  `REBUILD_CACHE_DIR`, `REBUILD_GAME_ID` and `REBUILD_GAME_BUILD` to the layer (`Storage::child_env`). The layer
  then resolves exactly the folders the launcher shows, even if it was started from somewhere else. Started by
  hand (`runtitle`, `rebuild --config`), the layer resolves the roots itself, by the same rules.
- `REBUILD_LAUNCHER_HOME`, the launcher's home variable before this layout, is read as `REBUILD_DATA_DIR`.

### The game id and the build

`rebuild_storage::identify(game_dir)` reads the package's own `MicrosoftGame.config`, read-only:

- **id**: the package identity's `Name` (`<Identity Name="…">`), lower-cased and reduced to `a-z 0-9 . - _`
  (64 at most). It is the same for every copy and every update of a title. **A game moved to another drive keeps
  its saves and settings**, which the old id (a hash of the folder's path) did not. Without an identity:
  `title-<TitleId>`. Without either: the path hash (the old id).
- **build**: the identity's `Version`. Without one: `m` and a hash of the manifest. Without a manifest:
  `unknown`. Only caches are per build. When a patch changes the build, the shader library and pipeline caches are
  rebuilt, and nothing else changes.

Since the id is the title, the same title can be in the library only once. Adding a second folder of it says where
the first one is. Nothing about any title is written in the code. The id comes from the user's own files at run
time, and folder names on disk are not in git.

### Saves vs caches

Clearing caches can never touch a save:

- Saves, storage, the overlay, settings and backups are under `games\<id>\`. Caches are under
  `<cache root>\games\<id>\cache\`. Even when both roots are the same (portable), `cache` is its own folder,
  beside `saves`.
- `GameStore::clear_cache` deletes only the children of a folder named `cache` whose parent is named for the game.
  It refuses when that folder would hold any of the game's user-data folders (a forged or misconfigured path).
  "Clear old builds" keeps the current build and the content-keyed BCPack cache.
- Tests: `clearing_the_cache_never_touches_saves` (portable and split roots: every user-data file is
  byte-identical after clearing, and logs too), `clearing_refuses_anything_but_a_games_own_cache_folder`.

## Migration (once, at start)

The layout before this one spread files over seven places. `rebuild_storage::migrate` moves them in. **A save is
never lost.** User data is copied, each file is checked by BLAKE3, and the old files stay where they were with a
`MIGRATED.txt` note beside them. A file the new place already has with other bytes is kept in both places and
reported. Only regenerable caches are renamed, and one that can't be renamed (another drive) is left to be
rebuilt.

| before | after | how | who |
|:--|:--|:--|:--|
| the old launcher home (`REBUILD_LAUNCHER_HOME`, the exe folder with a `rebuild.toml`, or `%APPDATA%\triumph-rebuild`): `library.toml`, `launcher.toml`, `games\` | `<data>\…` | copied and verified, when the data root changed and has no library yet; the old home gets a note; sessions are not copied | launcher, at start |
| `games\<path hash>\` | `games\<identity>\` (and its cache and logs) | merged: moved where the new id has nothing; a conflicting file stays in the old folder | launcher (`Library::migrate`) |
| `games\<id>\rebuild.toml` (and `.bak`) | `games\<id>\settings.toml` (and `.bak`) | renamed | launcher |
| `games\<id>\sessions\<stamp>\` | `logs\<id>\<stamp>\` | moved | launcher |
| `%APPDATA%\triumph-rebuild\overlay\GameSave\` | `games\<id>\saves\` | copied and verified; note left | layer, at launch |
| `…\overlay\S\` | `games\<id>\storage\` | copied and verified; note left | layer |
| `…\overlay\G\` | `games\<id>\overlay\G\` | copied and verified; note left | layer |
| `…\overlay\T\` | (nothing) | T: is emptied every run | |
| `%LOCALAPPDATA%\triumph-rebuild\bcpack-cache\` | `games\<id>\cache\bcpack\` | renamed | layer |
| `…\pipelines\` | `games\<id>\cache\<build>\pipelines\` | renamed | layer |
| `…\images\` | `games\<id>\cache\<build>\images\` | renamed | layer |
| `…\bcpack-tables.bin` | `<local>\bcpack-tables.bin` | copied when the local root changed (per-user: the same file) | layer |

The old overlay held one title's files without naming the game. They go to the first game launched after the
update, which is the only title that runs today. A copy that fails anywhere writes no note, so it is tried again
at the next launch. Tests: `the_old_overlay_is_copied_verified_and_never_deleted` (a newer save already in the
new place is never replaced, and every old file is still byte-identical), `a_failed_copy_leaves_no_note_so_it_is_tried_again`,
`caches_are_renamed_and_the_tables_copied`, `settings_are_renamed_and_sessions_become_logs`,
`a_game_renamed_to_its_identity_merges_without_losing_files`, `the_launcher_home_is_copied_when_the_root_changes`.

## Export and import (owner: "add a import and export function too")

`rebuild_storage::export(game, what, path)` and `rebuild_storage::import(game, path, options)` (with
`inspect(game, path, options)` for the preview). One `.zip`:

```text
manifest.toml                         format = "triumph-rebuild-export", version = 1, game_id, build, exported (UTC),
                                      users, and every file: path, size, blake3
saves/<user>/<scid>/<container>.gsc   all users, or one (What { saves: Some(Users::All | Users::One(xuid)), .. })
settings/settings.toml                What { settings: true, .. }
settings/profiles/<active>.toml       the active profile, when it is a saved one
```

The archive is our own small ZIP writer and reader (`archive.rs`: deflate through `flate2`, CRC-32 checked on
every read, no ZIP64, entry names that could escape their folder refused). Any zip tool opens it.

**Import checks everything before it changes anything.** It checks the manifest's format and version, the game id
(another game's export is refused), every file's size and hash, and that the archive holds no file the manifest
doesn't list. It is refused while the title runs: it checks the layer's run mutex `Local\triumph-rebuild-title-run`,
and in the launcher also its own child. Then:

1. **Backup:** the current saves (every user) and settings go to `games\<id>\backups\before-import-<stamp>.zip`.
   The backup is read back and checked against what was written. If it fails, nothing changes.
2. **Stage:** every imported file is written into a staging folder under the game's folder, and each one is read
   back.
3. **Swap:** each imported user's saves folder is replaced whole, by rename, so a user's saves are exactly the
   export's and never a mix. If a rename fails, the old folder goes back. Settings and the profile are written
   atomically. **Imported settings keep this machine's `game.dir`**, because the export names the other machine's
   folder.

**Users:** an export of one user who is not this machine's lands as this machine's offline user
(`DEFAULT_USER`, the runtime's xuid: `ImportOptions::for_this_machine`). An export of several users keeps them as
they are, unless one is chosen (`from_user`) to import as this user.

Tests (`transfer::tests`): `export_then_import_into_a_fresh_root_is_byte_identical`;
`damaged_or_mismatched_exports_are_refused_with_no_change` (a flipped byte, a file swapped behind the manifest's
back, an extra file, another game, not an export, the title running: each refused, and the data root
byte-identical after); `a_backup_is_taken_before_anything_is_replaced` (the backup holds exactly the old saves and
settings, and restoring it brings them back); `one_user_exported_and_imported_as_this_machines_user`.

The **save editor** lane keeps single-container import and export in its own Saves screen, through
`saves_dir(game, user)` and the container format.

## In the launcher

**Diagnostics → Storage** (`apps/launcher/src/screens/storage.rs`):

- a **Portable / Per-user / REBUILD_DATA_DIR** chip, with the reason on hover;
- the data, cache and log folders, each with Open folder;
- the selected game: its id and build, then saves (size and users), title storage, settings, overlay, mods,
  backups, the cache by build, and logs, each with Open folder;
- **Clear old builds** and **Clear cache** (caches only, never saves or settings);
- **Export saves… / Export settings… / Export both…** (a save dialog, `<game-id>-<what>-<stamp>.zip`) and
  **Import…**. Import shows what will be replaced, removed and added, and any user mapping. Then "Back up and
  import" does it.

Removing a game with "delete its data" also clears its cache and logs, whose roots may be elsewhere.

## What is not here yet

- `T:` is still under `overlay\T` (emptied every run). It could move to the cache root if it ever grows large.
