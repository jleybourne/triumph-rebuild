//! Where every file the launcher and the layer keep lives (`docs/design/storage.md`). This crate is the
//! one owner of every per-user path: no other code reads `APPDATA` or `LOCALAPPDATA`.
//!
//! ```text
//! <data>\                               portable: <launcher exe dir>\data; per-user: see below
//!   launcher.toml                       launcher-wide prefs (theme, storage.cache_dir)
//!   library.toml                        the game library
//!   games\<game-id>\
//!     settings.toml                     that game's settings (was rebuild.toml)
//!     profiles\  mods\                  its profiles and mods
//!     saves\<user>\<scid>\*.gsc         XGameSave containers (small: back these up)
//!     storage\                          the title's S: persistent local storage
//!     overlay\                          the write overlay (G: writes, T: temp, the settings.xml copy)
//!     backups\                          automatic backups taken before an import
//!   games\<game-id>\cache\              (under the cache root) regenerable, never holds a save
//!     <build>\shaders\  <build>\pipelines\  <build>\images\
//!     bcpack\                           decoded BCPack, content-keyed, kept across builds
//!   logs\<game-id>\<stamp>-<profile>\   (under the local root) one folder per session
//! ```
//!
//! **Roots.** `REBUILD_DATA_DIR` (explicit; `REBUILD_LOCAL_DIR` and `REBUILD_CACHE_DIR` may split it), else
//! **portable** when the launcher's folder is writable (`<exe dir>\data`; never inside a cargo `target`
//! folder, which `CACHEDIR.TAG` marks as a cache), else **per-user**: user data in `%APPDATA%\triumph-rebuild`,
//! logs and caches in `%LOCALAPPDATA%\triumph-rebuild`. The caches move as a whole with `REBUILD_CACHE_DIR` or
//! the launcher setting `storage.cache_dir` (another drive).

pub mod archive;
mod identity;
pub mod migrate;
pub mod transfer;

pub use identity::{identify, identify_manifest, is_game_id, path_id, Identity};
pub use transfer::{export, import, import_with, inspect, ImportOptions, Imported, Manifest, Plan, Users, What};

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// The folder name under `%APPDATA%` / `%LOCALAPPDATA%`.
pub const APP: &str = "triumph-rebuild";
/// The data root, explicit (the launcher passes its own to the layer).
pub const ENV_DATA_DIR: &str = "REBUILD_DATA_DIR";
/// The local root (logs) when it differs from the data root.
pub const ENV_LOCAL_DIR: &str = "REBUILD_LOCAL_DIR";
/// The cache root: the large regenerable files, e.g. on another drive.
pub const ENV_CACHE_DIR: &str = "REBUILD_CACHE_DIR";
/// The game id and build the launcher resolved (the layer's own resolution otherwise).
pub const ENV_GAME_ID: &str = "REBUILD_GAME_ID";
pub const ENV_GAME_BUILD: &str = "REBUILD_GAME_BUILD";
/// The launcher's home before this crate: read as `REBUILD_DATA_DIR` when that is unset.
pub const LEGACY_ENV_HOME: &str = "REBUILD_LAUNCHER_HOME";
/// The runtime's one offline user (`rebuild_gdk::profile::Profile::default().xuid`): an import of one other
/// user's saves lands as this user.
pub const DEFAULT_USER: u64 = 0x0009_0000_0000_0001;
/// The launcher-wide setting (in `launcher.toml`) that moves the caches.
pub const CACHE_DIR_KEY: &str = "storage.cache_dir";
/// Held by a title run for its whole life (`apps/rebuild/src/launch.rs`): import refuses while it exists.
pub const RUN_MUTEX: &str = "Local\\triumph-rebuild-title-run";

/// How the data root was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `<exe dir>\data`: everything beside the program, moved or deleted as one folder
    Portable,
    /// `%APPDATA%` / `%LOCALAPPDATA%`: the program's folder is not writable (e.g. Program Files)
    PerUser,
    /// `REBUILD_DATA_DIR`
    Explicit,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Portable => "portable",
            Mode::PerUser => "per-user",
            Mode::Explicit => "explicit (REBUILD_DATA_DIR)",
        }
    }
}

/// Where the cache root came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheFrom {
    /// the local root
    Default,
    /// `REBUILD_CACHE_DIR`
    Env,
    /// `storage.cache_dir` in `launcher.toml`
    Setting,
}

/// What resolution reads from the process, gathered so tests can give their own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Env {
    pub data_dir: Option<PathBuf>,
    pub local_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub legacy_home: Option<PathBuf>,
    /// the folder holding the running exe
    pub exe_dir: Option<PathBuf>,
    pub appdata: Option<PathBuf>,
    pub local_appdata: Option<PathBuf>,
    pub temp: PathBuf,
}

fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

impl Env {
    pub fn from_process() -> Env {
        Env {
            data_dir: var(ENV_DATA_DIR),
            local_dir: var(ENV_LOCAL_DIR),
            cache_dir: var(ENV_CACHE_DIR),
            legacy_home: var(LEGACY_ENV_HOME),
            exe_dir: std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)),
            appdata: var("APPDATA"),
            local_appdata: var("LOCALAPPDATA"),
            temp: std::env::temp_dir(),
        }
    }
}

/// The roots, resolved once.
#[derive(Debug, Clone, PartialEq)]
pub struct Storage {
    pub mode: Mode,
    /// user data: launcher.toml, library.toml, games\<id>\ (settings, saves, storage, overlay)
    pub user: PathBuf,
    /// local, never roaming: logs (sessions, crash reports), the BCPack tables file
    pub local: PathBuf,
    /// regenerable caches: games\<id>\cache\
    pub cache: PathBuf,
    pub cache_from: CacheFrom,
    /// anything resolution set aside (a refused cache folder), one line each
    pub notes: Vec<String>,
}

/// Whether a folder lies inside a cargo `target` folder (cargo marks it with `CACHEDIR.TAG`): a build's
/// output is not where saves belong (`cargo clean` deletes it), so the dev build is never portable.
pub fn in_cargo_target(dir: &Path) -> bool {
    dir.ancestors().take(5).any(|a| a.join("CACHEDIR.TAG").is_file())
}

/// `<dir>\data`, made if missing, when a file can be created and removed in it.
fn writable_data(dir: &Path) -> Option<PathBuf> {
    let d = dir.join("data");
    std::fs::create_dir_all(&d).ok()?;
    let probe = d.join(format!(".write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"probe").ok()?;
    std::fs::remove_file(&probe).ok()?;
    Some(d)
}

fn absolute(base: &Path, p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

/// `storage.cache_dir` from `<user>\launcher.toml` (`[settings]`), when set.
pub fn cache_dir_setting(user: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(user.join("launcher.toml")).ok()?;
    let doc: toml::Table = text.parse().ok()?;
    let v = doc.get("settings")?.get(CACHE_DIR_KEY)?.as_str()?.trim();
    (!v.is_empty()).then(|| PathBuf::from(v))
}

/// Whether `inner` is `outer` or inside it (case-insensitive on Windows, `.` components ignored).
pub fn is_within(inner: &Path, outer: &Path) -> bool {
    let key = |p: &Path| -> Vec<String> {
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        abs.components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .map(|c| {
                let s = c.as_os_str().to_string_lossy().replace('/', "\\");
                if cfg!(windows) {
                    s.to_lowercase()
                } else {
                    s
                }
            })
            .collect()
    };
    key(inner).starts_with(&key(outer))
}

impl Storage {
    /// The process's roots (environment, the exe's folder, `launcher.toml`).
    pub fn resolve() -> Storage {
        Storage::resolve_with(&Env::from_process())
    }

    pub fn resolve_with(env: &Env) -> Storage {
        let mut notes = Vec::new();
        let (mode, user, local) = if let Some(d) = env.data_dir.clone().or_else(|| env.legacy_home.clone()) {
            let local = env.local_dir.clone().unwrap_or_else(|| d.clone());
            (Mode::Explicit, d, local)
        } else if let Some(d) = env.exe_dir.as_deref().filter(|d| !in_cargo_target(d)).and_then(writable_data) {
            (Mode::Portable, d.clone(), d)
        } else {
            let user = env.appdata.as_ref().or(env.local_appdata.as_ref()).unwrap_or(&env.temp).join(APP);
            let local = env.local_appdata.as_ref().map(|l| l.join(APP)).unwrap_or_else(|| user.clone());
            (Mode::PerUser, user, local)
        };
        let (mut cache, mut cache_from) = match (&env.cache_dir, cache_dir_setting(&user)) {
            (Some(c), _) => (absolute(&user, c.clone()), CacheFrom::Env),
            (None, Some(c)) => (absolute(&user, c), CacheFrom::Setting),
            (None, None) => (local.clone(), CacheFrom::Default),
        };
        // a cache root inside the per-game user data would put caches among saves: refused
        if cache_from != CacheFrom::Default && is_within(&cache, &user.join("games")) {
            notes.push(format!("cache folder {} is inside the game data ({}): refused, using {}", cache.display(), user.join("games").display(), local.display()));
            cache = local.clone();
            cache_from = CacheFrom::Default;
        }
        Storage { mode, user, local, cache, cache_from, notes }
    }

    pub fn launcher_toml(&self) -> PathBuf {
        self.user.join("launcher.toml")
    }
    pub fn library_toml(&self) -> PathBuf {
        self.user.join("library.toml")
    }
    pub fn games_dir(&self) -> PathBuf {
        self.user.join("games")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.local.join("logs")
    }
    /// The BCPack format tables file's default place (user-supplied, never in git).
    pub fn bcpack_tables(&self) -> PathBuf {
        self.local.join("bcpack-tables.bin")
    }
    /// A cache with no game (benches, tools run without a title): `<cache>\shared\<kind>`.
    pub fn shared_cache(&self, kind: CacheKind) -> PathBuf {
        self.cache.join("shared").join(kind.folder())
    }

    /// One game's folders (no build: [`GameStore::with_build`] adds it).
    pub fn game(&self, id: &str) -> GameStore {
        GameStore {
            id: id.to_string(),
            build: String::new(),
            dir: self.games_dir().join(id),
            cache: self.cache.join("games").join(id).join("cache"),
            logs: self.logs_dir().join(id),
        }
    }

    /// The game in a folder (its identity read from its manifest), with its build.
    pub fn game_at(&self, game_dir: &Path) -> GameStore {
        let id = identify(game_dir);
        self.game(&id.id).with_build(&id.build)
    }

    /// The environment a child process (the layer) needs to resolve exactly these roots.
    pub fn child_env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![(ENV_DATA_DIR, self.user.clone()), (ENV_LOCAL_DIR, self.local.clone()), (ENV_CACHE_DIR, self.cache.clone())]
    }
}

/// The kinds of regenerable cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// the translated shader library and translation output (per build)
    Shaders,
    /// the driver pipeline cache, the session record, on-demand translations, rect GS (per build)
    Pipelines,
    /// prepared copies of the title's images (per build)
    Images,
    /// decoded BCPack, keyed by content, so kept across builds
    BcPack,
}

impl CacheKind {
    pub fn folder(self) -> &'static str {
        match self {
            CacheKind::Shaders => "shaders",
            CacheKind::Pipelines => "pipelines",
            CacheKind::Images => "images",
            CacheKind::BcPack => "bcpack",
        }
    }
}

/// One game's folders.
#[derive(Debug, Clone, PartialEq)]
pub struct GameStore {
    pub id: String,
    /// the game build ("" when unknown): the per-build caches are under it
    pub build: String,
    /// `<user>\games\<id>`
    pub dir: PathBuf,
    /// `<cache>\games\<id>\cache`
    pub cache: PathBuf,
    /// `<local>\logs\<id>`
    pub logs: PathBuf,
}

/// A user's folder name: the xuid as 16 hex digits (the GDK's own layout of saves).
pub fn user_dir_name(xuid: u64) -> String {
    format!("{xuid:016x}")
}

/// The xuid of a user folder name.
pub fn parse_user(name: &str) -> Option<u64> {
    (name.len() == 16).then(|| u64::from_str_radix(name, 16).ok()).flatten()
}

impl GameStore {
    pub fn with_build(mut self, build: &str) -> GameStore {
        self.build = build.to_string();
        self
    }

    pub fn settings_toml(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }
    /// The name before this layout (migrated to [`Self::settings_toml`]).
    pub fn legacy_settings_toml(&self) -> PathBuf {
        self.dir.join("rebuild.toml")
    }
    pub fn profiles_dir(&self) -> PathBuf {
        self.dir.join("profiles")
    }
    pub fn mods_dir(&self) -> PathBuf {
        self.dir.join("mods")
    }
    pub fn overlay_dir(&self) -> PathBuf {
        self.dir.join("overlay")
    }
    /// The title's S: drive (persistent local storage).
    pub fn storage_dir(&self) -> PathBuf {
        self.dir.join("storage")
    }
    /// Every user's saves: `saves\<user>\<scid>\<container>.gsc`.
    pub fn saves_root(&self) -> PathBuf {
        self.dir.join("saves")
    }
    /// One user's saves.
    pub fn saves_dir(&self, user: u64) -> PathBuf {
        self.saves_root().join(user_dir_name(user))
    }
    /// The users that have saves.
    pub fn save_users(&self) -> Vec<u64> {
        let mut v: Vec<u64> = std::fs::read_dir(self.saves_root())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| parse_user(&e.file_name().to_string_lossy()))
            .collect();
        v.sort_unstable();
        v
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.dir.join("backups")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.logs.clone()
    }
    /// The per-build cache folder (`cache\<build>`; `cache\unknown` without a build).
    pub fn build_cache(&self) -> PathBuf {
        self.cache.join(if self.build.is_empty() { "unknown" } else { &self.build })
    }
    pub fn cache_dir(&self, kind: CacheKind) -> PathBuf {
        match kind {
            CacheKind::BcPack => self.cache.join(kind.folder()),
            _ => self.build_cache().join(kind.folder()),
        }
    }
    pub fn shaders_dir(&self) -> PathBuf {
        self.cache_dir(CacheKind::Shaders)
    }
    pub fn pipelines_dir(&self) -> PathBuf {
        self.cache_dir(CacheKind::Pipelines)
    }
    pub fn images_dir(&self) -> PathBuf {
        self.cache_dir(CacheKind::Images)
    }
    pub fn bcpack_cache_dir(&self) -> PathBuf {
        self.cache_dir(CacheKind::BcPack)
    }

    /// The cache's top folders and their sizes: each build, and `bcpack`.
    pub fn cache_entries(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = std::fs::read_dir(&self.cache)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), dir_size(&e.path())))
            .collect();
        v.sort();
        v
    }

    pub fn sizes(&self) -> Sizes {
        let cache = self.cache_entries();
        Sizes {
            settings: file_size(&self.settings_toml()) + dir_size(&self.profiles_dir()),
            saves: dir_size(&self.saves_root()),
            storage: dir_size(&self.storage_dir()),
            overlay: dir_size(&self.overlay_dir()),
            mods: dir_size(&self.mods_dir()),
            backups: dir_size(&self.backups_dir()),
            cache_total: cache.iter().map(|c| c.1).sum(),
            cache,
            logs: dir_size(&self.logs),
        }
    }

    /// Delete the regenerable cache: everything under `cache\`, or (with `keep`) every build but that one
    /// and nothing shared. Never anything else: the folder must be this game's `cache` and hold none of its
    /// user data. Returns the bytes freed.
    pub fn clear_cache(&self, keep: Option<&str>) -> Result<u64, String> {
        let ok_name = self.cache.file_name().is_some_and(|n| n == "cache") && self.cache.parent().and_then(|p| p.file_name()).is_some_and(|n| n.to_string_lossy() == self.id);
        if !ok_name || !is_game_id(&self.id) {
            return Err(format!("refused: {} is not a game's cache folder", self.cache.display()));
        }
        for user_data in [&self.dir, &self.saves_root(), &self.storage_dir(), &self.overlay_dir(), &self.backups_dir()] {
            if is_within(user_data, &self.cache) {
                return Err(format!("refused: {} would hold {}", self.cache.display(), user_data.display()));
            }
        }
        let mut freed = 0;
        for e in std::fs::read_dir(&self.cache).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(k) = keep {
                if name == k || name == CacheKind::BcPack.folder() {
                    continue;
                }
            }
            let p = e.path();
            let n = if p.is_dir() { dir_size(&p) } else { file_size(&p) };
            let r = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            r.map_err(|e| format!("{}: {e}", p.display()))?;
            freed += n;
        }
        Ok(freed)
    }
}

/// One game's sizes in bytes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sizes {
    pub settings: u64,
    pub saves: u64,
    pub storage: u64,
    pub overlay: u64,
    pub mods: u64,
    pub backups: u64,
    pub cache: Vec<(String, u64)>,
    pub cache_total: u64,
    pub logs: u64,
}

pub fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Bytes of every file under a folder (links not followed).
pub fn dir_size(p: &Path) -> u64 {
    let mut n = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                n += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    n
}

/// The process's roots, resolved on first use.
pub fn storage() -> &'static Storage {
    static S: OnceLock<Storage> = OnceLock::new();
    S.get_or_init(Storage::resolve)
}

/// One user's saves folder for a game: `<data>\games\<game>\saves\<user as 16 hex>`. The API the save
/// editor reads saves through.
pub fn saves_dir(game: &str, user: u64) -> PathBuf {
    storage().game(game).saves_dir(user)
}

static CURRENT: RwLock<Option<GameStore>> = RwLock::new(None);

/// The game this process runs (the layer sets it at launch, before anything opens a cache).
pub fn set_current(game: GameStore) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(game);
}

/// The game this process runs: what [`set_current`] set, else `REBUILD_GAME_ID` (+ `REBUILD_GAME_BUILD`),
/// else the folder in `REBUILD_GAME_DIR`; `None` without any.
pub fn current() -> Option<GameStore> {
    if let Some(g) = CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(g);
    }
    let g = if let Some(id) = std::env::var(ENV_GAME_ID).ok().filter(|i| is_game_id(i)) {
        storage().game(&id).with_build(&std::env::var(ENV_GAME_BUILD).unwrap_or_default())
    } else {
        storage().game_at(&var("REBUILD_GAME_DIR")?)
    };
    set_current(g.clone());
    Some(g)
}

/// The game for a folder as this process sees it: `REBUILD_GAME_ID` / `REBUILD_GAME_BUILD` (the launcher's
/// resolution) when set, else the folder's own identity.
pub fn game_for(game_dir: &Path) -> GameStore {
    match std::env::var(ENV_GAME_ID).ok().filter(|i| is_game_id(i)) {
        Some(id) => {
            let build = std::env::var(ENV_GAME_BUILD).ok().filter(|b| !b.is_empty()).unwrap_or_else(|| identify(game_dir).build);
            storage().game(&id).with_build(&build)
        }
        None => storage().game_at(game_dir),
    }
}

/// A cache folder for this process: the current game's, else the shared one.
pub fn cache_dir(kind: CacheKind) -> PathBuf {
    match current() {
        Some(g) => g.cache_dir(kind),
        None => storage().shared_cache(kind),
    }
}

/// A prepared image copy by file name, in any game's per-build `images` cache (newest first), or in the
/// place before this layout (`%LOCALAPPDATA%\triumph-rebuild\images`).
pub fn find_cached_image(name: &str) -> Option<PathBuf> {
    let st = storage();
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for g in std::fs::read_dir(st.cache.join("games")).into_iter().flatten().flatten() {
        for b in std::fs::read_dir(g.path().join("cache")).into_iter().flatten().flatten() {
            let p = b.path().join("images").join(name);
            if let Ok(m) = std::fs::metadata(&p) {
                found.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), p));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|f| f.1).next().or_else(|| {
        let p = migrate::Legacy::from_env(&Env::from_process()).local?.join("images").join(name);
        p.is_file().then_some(p)
    })
}

/// Whether a title run holds [`RUN_MUTEX`] on this machine.
pub fn title_running() -> bool {
    mutex_exists(RUN_MUTEX)
}

/// Whether a named mutex exists.
#[cfg(windows)]
pub fn mutex_exists(name: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(name)) } {
        Ok(h) => {
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
pub fn mutex_exists(_: &str) -> bool {
    false
}

/// Write through a temporary beside the file, so a crash never leaves half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_file_name(format!(".{}.tmp-{}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// BLAKE3 of a file's bytes, as hex.
pub fn hash_file(p: &Path) -> io::Result<String> {
    Ok(blake3::hash(&std::fs::read(p)?).to_hex().to_string())
}

/// UTC civil time of Unix seconds: (year, month, day, hour, minute, second).
pub fn utc(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `2026-11-28T10:00:00Z`.
pub fn rfc3339(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `20261128T100000Z`, for file names.
pub fn stamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

#[cfg(test)]
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("rebuild-storage-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests;
