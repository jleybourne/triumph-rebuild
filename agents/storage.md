# Lane `storage`: where every file lives

Design: [docs/design/storage.md](../docs/design/storage.md). Code: `crates/storage` (package `rebuild-storage`).

## Built

- One owner of every per-user path. The seven sites that read `APPDATA` / `LOCALAPPDATA` now ask
  `rebuild_storage`: launcher home, vfs overlay default (removed), BCPack tables and cache, pipeline session
  dir, images cache, devmenu test.
- Roots: portable `<exe dir>\data` when writable (never inside a cargo `target`, which `CACHEDIR.TAG` marks),
  else per-user (`%APPDATA%` for user data, `%LOCALAPPDATA%` for caches and logs), else `REBUILD_DATA_DIR`. Caches
  move with `REBUILD_CACHE_DIR` or `storage.cache_dir` (new registry entry, launcher-wide in `launcher.toml`).
- Layout per game, keyed by the package identity (`<Identity Name>`), with caches per build (`<Identity Version>`).
- Migration, once: saves, storage and G: writes are copied and verified, the old files kept with a note. Caches
  are renamed. Launcher ids move from the path hash to the identity. `rebuild.toml` becomes `settings.toml`.
  Sessions move to `logs\`.
- Export / Import (owner request): a zip with a manifest, checked before anything changes. Import is refused while
  the title runs or for another game. It takes an automatic backup first and swaps each user's saves whole.
  Single-user exports map onto this machine's user.
- Launcher: Diagnostics → Storage (portable / per-user chip, folders, sizes, Open folder, Clear old builds, Clear
  cache, Export saves / settings / both, Import with a preview).

## For the save editor lane

`rebuild_storage::saves_dir(game_id, user: u64) -> PathBuf` returns `<data>\games\<id>\saves\<user as 16 hex>`.
Under it are `<scid>\<container>.gsc`. `GameStore::save_users()` lists the users. `export` / `import` /
`inspect` cover whole-game transfer.

## For the lead

- `crates/config` got one registry line (`storage.cache_dir`) and its `NOT_RUNTIME` entry. The lead assigned that.
- The `D:\victory-data` defaults in `crates/pipelines` / `crates/runtime` are untouched. `GameStore::shaders_dir()`
  is where the first-run shader library goes (the portable-install work).
