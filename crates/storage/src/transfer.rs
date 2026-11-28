//! Export and import a game's saves and settings as one `.zip` with a manifest.
//!
//! ```text
//! manifest.toml                           format, version, game id, build, users, export time, every file
//!                                         with its size and BLAKE3
//! saves/<user>/<scid>/<container>.gsc     the users' saves, as `games\<id>\saves\` holds them
//! settings/settings.toml                  the game's settings
//! settings/profiles/<name>.toml           the active profile, when it is a saved one
//! ```
//!
//! **Import** checks everything before it changes anything: the manifest's format and version, the game
//! id (an export of another game is refused), every file's size and hash, no file the manifest does not
//! list. It refuses while the title runs. It backs the current saves and settings up first
//! (`games\<id>\backups\before-import-<stamp>.zip`, read back and checked), writes the imported files into
//! a staging folder, then swaps each user's saves folder in whole (so a user's saves are exactly the
//! export's, never a mix). An export's user can be mapped onto this machine's user. Settings keep this
//! machine's `game.dir`.

use crate::archive::{self, Entry};
use crate::{parse_user, user_dir_name, GameStore};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FORMAT: &str = "triumph-rebuild-export";
pub const VERSION: u32 = 1;
const MANIFEST: &str = "manifest.toml";

/// Whose saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Users {
    All,
    One(u64),
}

/// What an export holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct What {
    /// the saves of these users, or none
    pub saves: Option<Users>,
    /// `settings.toml` and the active profile
    pub settings: bool,
}

impl What {
    pub const EVERYTHING: What = What { saves: Some(Users::All), settings: true };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub game_id: String,
    pub build: String,
    /// RFC 3339, UTC
    pub exported: String,
    pub exported_unix: u64,
    pub users: Vec<u64>,
    pub saves: bool,
    pub settings: bool,
    pub profile: Option<String>,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    fn to_toml(&self) -> String {
        let mut t = toml::Table::new();
        let s = |v: &str| toml::Value::String(v.into());
        t.insert("format".into(), s(&self.format));
        t.insert("version".into(), toml::Value::Integer(self.version as i64));
        t.insert("game_id".into(), s(&self.game_id));
        t.insert("build".into(), s(&self.build));
        t.insert("exported".into(), s(&self.exported));
        t.insert("exported_unix".into(), toml::Value::Integer(self.exported_unix as i64));
        t.insert("users".into(), toml::Value::Array(self.users.iter().map(|u| s(&user_dir_name(*u))).collect()));
        t.insert("saves".into(), toml::Value::Boolean(self.saves));
        t.insert("settings".into(), toml::Value::Boolean(self.settings));
        if let Some(p) = &self.profile {
            t.insert("profile".into(), s(p));
        }
        let files = self
            .files
            .iter()
            .map(|f| {
                let mut e = toml::Table::new();
                e.insert("path".into(), s(&f.path));
                e.insert("size".into(), toml::Value::Integer(f.size as i64));
                e.insert("blake3".into(), s(&f.blake3));
                toml::Value::Table(e)
            })
            .collect();
        t.insert("files".into(), toml::Value::Array(files));
        toml::to_string_pretty(&t).expect("manifest serialises")
    }

    fn parse(text: &str) -> Result<Manifest, String> {
        let t: toml::Table = text.parse().map_err(|e| format!("the manifest does not parse: {e}"))?;
        let st = |k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from).ok_or_else(|| format!("the manifest has no {k}"));
        let int = |k: &str| t.get(k).and_then(|v| v.as_integer()).filter(|i| *i >= 0).ok_or_else(|| format!("the manifest has no {k}"));
        let format = st("format")?;
        if format != FORMAT {
            return Err(format!("not an export of this launcher (format {format:?})"));
        }
        let version = int("version")? as u32;
        if version == 0 || version > VERSION {
            return Err(format!("export format version {version} is newer than this launcher reads ({VERSION})"));
        }
        let users = t.get("users").and_then(|v| v.as_array()).ok_or("the manifest has no users")?;
        let users = users.iter().map(|u| u.as_str().and_then(parse_user).ok_or("a user in the manifest is not 16 hex digits")).collect::<Result<Vec<_>, _>>()?;
        let mut files = Vec::new();
        for f in t.get("files").and_then(|v| v.as_array()).ok_or("the manifest has no files")? {
            let g = |k: &str| f.get(k).and_then(|v| v.as_str()).map(String::from).ok_or_else(|| format!("a manifest file has no {k}"));
            let size = f.get("size").and_then(|v| v.as_integer()).filter(|i| *i >= 0).ok_or("a manifest file has no size")? as u64;
            files.push(FileEntry { path: g("path")?, size, blake3: g("blake3")? });
        }
        Ok(Manifest {
            format,
            version,
            game_id: st("game_id")?,
            build: st("build")?,
            exported: st("exported")?,
            exported_unix: int("exported_unix")? as u64,
            users,
            saves: t.get("saves").and_then(|v| v.as_bool()).unwrap_or(false),
            settings: t.get("settings").and_then(|v| v.as_bool()).unwrap_or(false),
            profile: t.get("profile").and_then(|v| v.as_str()).map(String::from),
            files,
        })
    }
}

/// Files under a folder as (`/`-separated relative path, full path), sorted; `.tmp` leftovers skipped.
fn files_under(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut v = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                stack.push(p);
            } else if t.is_file() && !p.to_string_lossy().ends_with(".tmp") {
                if let Ok(rel) = p.strip_prefix(dir) {
                    v.push((rel.to_string_lossy().replace('\\', "/"), p));
                }
            }
        }
    }
    v.sort();
    v
}

/// The `profile` a settings file names.
fn profile_of(settings: &[u8]) -> Option<String> {
    let t: toml::Table = std::str::from_utf8(settings).ok()?.parse().ok()?;
    let p = t.get("profile")?.as_str()?.trim().to_string();
    let ok = !p.is_empty() && p.len() <= 64 && p.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' '));
    ok.then_some(p)
}

fn hash(b: &[u8]) -> String {
    blake3::hash(b).to_hex().to_string()
}

/// The archive's entries for `what`, manifest first.
fn collect(game: &GameStore, what: &What) -> Result<(Manifest, Vec<Entry>), String> {
    let mut entries = Vec::new();
    let mut users = Vec::new();
    if let Some(u) = what.saves {
        users = match u {
            Users::All => game.save_users(),
            Users::One(x) => vec![x],
        };
        for &user in &users {
            for (rel, p) in files_under(&game.saves_dir(user)) {
                let data = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                entries.push(Entry { name: format!("saves/{}/{rel}", user_dir_name(user)), data });
            }
        }
        if let Users::One(x) = u {
            if entries.is_empty() {
                return Err(format!("user {} has no saves for this game", user_dir_name(x)));
            }
        }
    }
    let mut profile = None;
    if what.settings {
        if let Ok(data) = std::fs::read(game.settings_toml()) {
            profile = profile_of(&data);
            entries.push(Entry { name: "settings/settings.toml".into(), data });
            if let Some(p) = &profile {
                if let Ok(data) = std::fs::read(game.profiles_dir().join(format!("{p}.toml"))) {
                    entries.push(Entry { name: format!("settings/profiles/{p}.toml"), data });
                }
            }
        }
    }
    if entries.is_empty() {
        return Err("nothing to export: this game has no saves or settings yet".into());
    }
    let now = crate::now_unix();
    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        game_id: game.id.clone(),
        build: game.build.clone(),
        exported: crate::rfc3339(now),
        exported_unix: now,
        users,
        saves: what.saves.is_some(),
        settings: what.settings,
        profile,
        files: entries.iter().map(|e| FileEntry { path: e.name.clone(), size: e.data.len() as u64, blake3: hash(&e.data) }).collect(),
    };
    let mut all = vec![Entry { name: MANIFEST.into(), data: manifest.to_toml().into_bytes() }];
    all.extend(entries);
    Ok((manifest, all))
}

/// Export a game's saves and/or settings to one `.zip` (written atomically). The save editor's API.
pub fn export(game: &GameStore, what: &What, path: &Path) -> Result<Manifest, String> {
    let (m, entries) = collect(game, what)?;
    archive::write(path, &entries)?;
    Ok(m)
}

/// A file's place in an export, checked: saves of a user, the settings, or a profile.
fn known_path(p: &str) -> bool {
    let parts: Vec<&str> = p.split('/').collect();
    match parts.as_slice() {
        ["saves", user, rest @ ..] => parse_user(user).is_some() && !rest.is_empty(),
        ["settings", "settings.toml"] => true,
        ["settings", "profiles", name] => name.ends_with(".toml") && name.len() > 5,
        _ => false,
    }
}

/// Read and check an export: the manifest, and its files by path. Nothing on disk changes.
pub fn read_export(path: &Path) -> Result<(Manifest, BTreeMap<String, Vec<u8>>), String> {
    let entries = archive::read(path)?;
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut manifest = None;
    for e in entries {
        if e.name == MANIFEST {
            manifest = Some(Manifest::parse(std::str::from_utf8(&e.data).map_err(|_| "the manifest is not UTF-8")?)?);
        } else {
            files.insert(e.name, e.data);
        }
    }
    let m = manifest.ok_or("not an export of this launcher (no manifest.toml)")?;
    if !crate::is_game_id(&m.game_id) {
        return Err(format!("the manifest's game id {:?} is not valid", m.game_id));
    }
    if m.files.len() != files.len() {
        return Err(format!("the manifest lists {} files, the archive holds {}", m.files.len(), files.len()));
    }
    for f in &m.files {
        if !known_path(&f.path) {
            return Err(format!("{}: not a place an export holds", f.path));
        }
        let data = files.get(&f.path).ok_or_else(|| format!("{}: listed in the manifest, missing from the archive", f.path))?;
        if data.len() as u64 != f.size || hash(data) != f.blake3 {
            return Err(format!("{}: does not match the manifest (changed or damaged)", f.path));
        }
    }
    Ok((m, files))
}

/// How to import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOptions {
    pub saves: bool,
    pub settings: bool,
    /// only this user of the export (required to map an export of several users)
    pub from_user: Option<u64>,
    /// this machine's user: the export's user is imported as this one
    pub to_user: Option<u64>,
}

impl ImportOptions {
    /// Everything in the export; an export of one user who is not this machine's lands as this machine's
    /// offline user ([`crate::DEFAULT_USER`]).
    pub fn for_this_machine(m: &Manifest) -> ImportOptions {
        let to_user = (m.users.len() == 1 && m.users[0] != crate::DEFAULT_USER).then_some(crate::DEFAULT_USER);
        ImportOptions { to_user, ..ImportOptions::default() }
    }
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions { saves: true, settings: true, from_user: None, to_user: None }
    }
}

/// What an import will do, file by file (paths relative to the game's folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub manifest: Manifest,
    /// (export's user, the user it lands as)
    pub users: Vec<(u64, u64)>,
    pub added: Vec<String>,
    pub replaced: Vec<String>,
    /// saves of an imported user that the export does not have (the user's saves become the export's)
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
}

impl Plan {
    pub fn changes(&self) -> usize {
        self.added.len() + self.replaced.len() + self.removed.len()
    }
}

/// The result of an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub plan: Plan,
    /// the backup taken first (none when there was nothing to back up or nothing changed)
    pub backup: Option<PathBuf>,
    pub notes: Vec<String>,
}

struct Staged {
    /// (target user, files relative to the user's folder)
    users: Vec<(u64, Vec<(String, Vec<u8>)>)>,
    /// (target path, bytes)
    settings: Vec<(PathBuf, Vec<u8>)>,
    notes: Vec<String>,
}

fn prepare(game: &GameStore, path: &Path, o: &ImportOptions) -> Result<(Plan, Staged), String> {
    let (m, files) = read_export(path)?;
    if m.game_id != game.id {
        return Err(format!("this export is of game {:?}, not this one ({:?}): refused", m.game_id, game.id));
    }
    let mut users: Vec<(u64, u64)> = Vec::new();
    if o.saves && m.saves {
        let chosen: Vec<u64> = match o.from_user {
            Some(u) if m.users.contains(&u) => vec![u],
            Some(u) => return Err(format!("the export has no user {}", user_dir_name(u))),
            None => m.users.clone(),
        };
        users = match o.to_user {
            Some(to) if chosen.len() == 1 => vec![(chosen[0], to)],
            Some(_) if chosen.is_empty() => vec![],
            Some(_) => return Err(format!("the export holds {} users: choose the one to import as this user", chosen.len())),
            None => chosen.iter().map(|&u| (u, u)).collect(),
        };
    }
    let mut plan = Plan { manifest: m.clone(), users: users.clone(), added: vec![], replaced: vec![], removed: vec![], unchanged: vec![] };
    let mut staged = Staged { users: vec![], settings: vec![], notes: vec![] };
    let compare = |plan: &mut Plan, shown: String, target: &Path, data: &[u8]| match std::fs::read(target) {
        Ok(old) if old == data => plan.unchanged.push(shown),
        Ok(_) => plan.replaced.push(shown),
        Err(_) => plan.added.push(shown),
    };
    for &(from, to) in &users {
        let prefix = format!("saves/{}/", user_dir_name(from));
        let mine: Vec<(String, Vec<u8>)> = files.iter().filter_map(|(k, v)| k.strip_prefix(&prefix).map(|r| (r.to_string(), v.clone()))).collect();
        let dir = game.saves_dir(to);
        for (rel, data) in &mine {
            compare(&mut plan, format!("saves/{}/{rel}", user_dir_name(to)), &dir.join(rel), data);
        }
        for (rel, _) in files_under(&dir) {
            if !mine.iter().any(|(r, _)| r.eq_ignore_ascii_case(&rel)) {
                plan.removed.push(format!("saves/{}/{rel}", user_dir_name(to)));
            }
        }
        staged.users.push((to, mine));
    }
    if o.settings && m.settings {
        if let Some(data) = files.get("settings/settings.toml") {
            let data = keep_game_dir(data, &game.settings_toml(), &mut staged.notes);
            compare(&mut plan, "settings.toml".into(), &game.settings_toml(), &data);
            staged.settings.push((game.settings_toml(), data));
        }
        for (k, v) in files.iter().filter(|(k, _)| k.starts_with("settings/profiles/")) {
            let name = &k["settings/profiles/".len()..];
            let target = game.profiles_dir().join(name);
            compare(&mut plan, format!("profiles/{name}"), &target, v);
            staged.settings.push((target, v.clone()));
        }
    }
    Ok((plan, staged))
}

/// Imported settings keep this machine's game folder (`game.dir`): the export's names the other machine's.
fn keep_game_dir(imported: &[u8], current: &Path, notes: &mut Vec<String>) -> Vec<u8> {
    let here = std::fs::read_to_string(current).ok().and_then(|t| t.parse::<toml::Table>().ok()).and_then(|t| t.get("settings")?.get("game.dir")?.as_str().map(String::from));
    let Some(here) = here else { return imported.to_vec() };
    let Some(mut t) = std::str::from_utf8(imported).ok().and_then(|s| s.parse::<toml::Table>().ok()) else { return imported.to_vec() };
    let theirs = t.get("settings").and_then(|s| s.get("game.dir")).and_then(|v| v.as_str()).map(String::from);
    if theirs.as_deref() == Some(here.as_str()) {
        return imported.to_vec();
    }
    if let Some(s) = t.get_mut("settings").and_then(|s| s.as_table_mut()) {
        s.insert("game.dir".into(), toml::Value::String(here.clone()));
    }
    notes.push(format!("game.dir kept as this machine's {here}"));
    toml::to_string_pretty(&t).map(String::into_bytes).unwrap_or_else(|_| imported.to_vec())
}

/// What importing `path` would do. Checks everything; changes nothing.
pub fn inspect(game: &GameStore, path: &Path, o: &ImportOptions) -> Result<Plan, String> {
    prepare(game, path, o).map(|(p, _)| p)
}

/// Import an export, refused while the title runs.
pub fn import(game: &GameStore, path: &Path, o: &ImportOptions) -> Result<Imported, String> {
    import_with(game, path, o, crate::title_running)
}

/// [`import`] with the "is the title running" check given (tests).
pub fn import_with(game: &GameStore, path: &Path, o: &ImportOptions, running: impl Fn() -> bool) -> Result<Imported, String> {
    if running() {
        return Err("the title is running: close it before importing saves".into());
    }
    let (plan, staged) = prepare(game, path, o)?;
    if plan.changes() == 0 {
        return Ok(Imported { plan, backup: None, notes: staged.notes });
    }
    // 1. the backup, checked by reading it back
    let mut backup = None;
    let has_data = !game.save_users().is_empty() || game.settings_toml().is_file();
    if has_data {
        let p = game.backups_dir().join(format!("before-import-{}.zip", crate::stamp(crate::now_unix())));
        let p = unique(p);
        let made = export(game, &What::EVERYTHING, &p).map_err(|e| format!("backup before import failed, nothing changed: {e}"))?;
        let (back, _) = read_export(&p).map_err(|e| format!("the backup {} does not read back, nothing changed: {e}", p.display()))?;
        if back.files != made.files {
            return Err(format!("the backup {} does not match, nothing changed", p.display()));
        }
        backup = Some(p);
    }
    // 2. stage every file and check it
    let staging = game.dir.join(format!(".import-{}-{}", std::process::id(), crate::now_unix()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| -> Result<(), String> {
        for (to, files) in &staged.users {
            let dir = staging.join("saves").join(user_dir_name(*to));
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            for (rel, data) in files {
                let p = dir.join(rel);
                crate::write_atomic(&p, data).map_err(|e| format!("{}: {e}", p.display()))?;
                if std::fs::read(&p).map_err(|e| e.to_string())? != *data {
                    return Err(format!("{}: staged copy does not match", p.display()));
                }
            }
        }
        // 3. swap each user's folder in whole; on a failure the old folder goes back
        std::fs::create_dir_all(game.saves_root()).map_err(|e| e.to_string())?;
        let old_dir = staging.join("old");
        std::fs::create_dir_all(&old_dir).map_err(|e| e.to_string())?;
        for (to, _) in &staged.users {
            let (target, new, old) = (game.saves_dir(*to), staging.join("saves").join(user_dir_name(*to)), old_dir.join(user_dir_name(*to)));
            let had = target.exists();
            if had {
                std::fs::rename(&target, &old).map_err(|e| format!("{}: {e} (nothing changed for this user)", target.display()))?;
            }
            if let Err(e) = std::fs::rename(&new, &target) {
                if had {
                    let _ = std::fs::rename(&old, &target);
                }
                return Err(format!("{}: {e} (the user's saves were put back)", target.display()));
            }
        }
        for (target, data) in &staged.settings {
            crate::write_atomic(target, data).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result.map_err(|e| match &backup {
        Some(b) => format!("{e}; the backup is {}", b.display()),
        None => e,
    })?;
    Ok(Imported { plan, backup, notes: staged.notes })
}

fn unique(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (2..).map(|i| p.with_file_name(format!("{stem}-{i}.zip"))).find(|c| !c.exists()).expect("a free name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{temp_dir, Env, Storage};

    const U1: u64 = 0x0009_0000_0000_0001;
    const U2: u64 = 0x0009_0000_0000_0002;

    fn root(tag: &str) -> Storage {
        let d = temp_dir(tag);
        Storage::resolve_with(&Env { data_dir: Some(d.clone()), temp: d, ..Env::default() })
    }

    fn put(p: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// (relative path, bytes) of every file under a folder.
    fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
        files_under(dir).into_iter().map(|(r, p)| (r, std::fs::read(p).unwrap())).collect()
    }

    fn game_with_saves(st: &Storage) -> GameStore {
        let g = st.game("studio.title").with_build("1.0.0.0");
        put(&g.saves_dir(U1).join("scid-1/profile.gsc"), &[1u8; 3000]);
        put(&g.saves_dir(U1).join("scid-1/slot~0123456789abcdef.gsc"), b"slot");
        put(&g.saves_dir(U2).join("scid-1/profile.gsc"), b"second user");
        put(&g.settings_toml(), b"profile = \"mine\"\n\n[settings]\n\"game.dir\" = 'C:\\games\\title'\n\"display.width\" = 2560\n");
        put(&g.profiles_dir().join("mine.toml"), b"profile = \"mine\"\n");
        put(&g.profiles_dir().join("other.toml"), b"not exported");
        g
    }

    #[test]
    fn export_then_import_into_a_fresh_root_is_byte_identical() {
        let a = game_with_saves(&root("x-a"));
        let zip = a.dir.parent().unwrap().join("export.zip");
        let m = export(&a, &What::EVERYTHING, &zip).unwrap();
        assert_eq!(m.users, vec![U1, U2]);
        assert_eq!(m.profile.as_deref(), Some("mine"));
        assert_eq!(m.files.len(), 5, "3 saves, settings, the active profile only");
        let b = root("x-b").game("studio.title");
        let plan = inspect(&b, &zip, &ImportOptions::default()).unwrap();
        assert_eq!((plan.added.len(), plan.replaced.len(), plan.removed.len()), (5, 0, 0));
        let done = import_with(&b, &zip, &ImportOptions::default(), || false).unwrap();
        assert_eq!(done.backup, None, "nothing to back up in a fresh root");
        assert_eq!(tree(&b.saves_root()), tree(&a.saves_root()));
        assert_eq!(std::fs::read(b.settings_toml()).unwrap(), std::fs::read(a.settings_toml()).unwrap());
        assert_eq!(std::fs::read(b.profiles_dir().join("mine.toml")).unwrap(), std::fs::read(a.profiles_dir().join("mine.toml")).unwrap());
        assert!(!b.profiles_dir().join("other.toml").exists());
        // and again: nothing changes
        let again = import_with(&b, &zip, &ImportOptions::default(), || false).unwrap();
        assert_eq!(again.plan.changes(), 0);
        assert!(std::fs::read_dir(&b.dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".import")), "no staging left");
    }

    #[test]
    fn damaged_or_mismatched_exports_are_refused_with_no_change() {
        let a = game_with_saves(&root("x-bad-a"));
        let zip = a.dir.parent().unwrap().join("export.zip");
        export(&a, &What::EVERYTHING, &zip).unwrap();
        let st = root("x-bad-b");
        let b = st.game("studio.title");
        put(&b.saves_dir(U1).join("scid-1/profile.gsc"), b"mine, keep me");
        let before = tree(&st.user);
        let no = |p: &Path, why: &str| {
            let e = import_with(&b, p, &ImportOptions::default(), || false).unwrap_err();
            assert!(e.contains(why), "{e}");
            assert_eq!(tree(&st.user), before, "{why}: nothing changed");
        };
        // a flipped byte in a save's body (stored: 4 bytes don't deflate smaller; it follows its local name)
        let mut bytes = std::fs::read(&zip).unwrap();
        let name = b"saves/0009000000000001/scid-1/slot~0123456789abcdef.gsc";
        let at = bytes.windows(name.len()).position(|w| w == name).unwrap() + name.len();
        assert_eq!(&bytes[at..at + 4], b"slot");
        bytes[at] ^= 0x55;
        let bad = st.user.join("../bad.zip");
        std::fs::write(&bad, &bytes).unwrap();
        no(&bad, "damaged");
        // a file swapped behind the manifest's back (the archive itself is sound)
        let (m, mut files) = read_export(&zip).unwrap();
        files.insert("saves/0009000000000001/scid-1/profile.gsc".into(), b"tampered".to_vec());
        let mut entries = vec![Entry { name: MANIFEST.into(), data: m.to_toml().into_bytes() }];
        entries.extend(files.iter().map(|(k, v)| Entry { name: k.clone(), data: v.clone() }));
        let tampered = st.user.join("../tampered.zip");
        archive::write(&tampered, &entries).unwrap();
        no(&tampered, "does not match the manifest");
        // an extra file the manifest does not list
        entries[1].data = std::fs::read(a.saves_dir(U1).join("scid-1/profile.gsc")).unwrap();
        let mut extra = entries.clone();
        extra.push(Entry { name: "saves/0009000000000001/scid-1/sneaky.gsc".into(), data: b"x".to_vec() });
        let p = st.user.join("../extra.zip");
        archive::write(&p, &extra).unwrap();
        no(&p, "lists 5 files");
        // another game's export
        let other = root("x-bad-c");
        let og = other.game("another.title");
        put(&og.saves_dir(U1).join("s/a.gsc"), b"other game");
        let oz = other.user.join("../other.zip");
        export(&og, &What::EVERYTHING, &oz).unwrap();
        no(&oz, "not this one");
        // not ours at all
        let p = st.user.join("../plain.zip");
        archive::write(&p, &[Entry { name: "readme.txt".into(), data: b"hi".to_vec() }]).unwrap();
        no(&p, "no manifest");
        // while the title runs
        let e = import_with(&b, &zip, &ImportOptions::default(), || true).unwrap_err();
        assert!(e.contains("running"));
        assert_eq!(tree(&st.user), before, "running: nothing changed");
    }

    #[test]
    fn a_backup_is_taken_before_anything_is_replaced() {
        let a = game_with_saves(&root("x-bk-a"));
        let zip = a.dir.parent().unwrap().join("export.zip");
        export(&a, &What::EVERYTHING, &zip).unwrap();
        let b = root("x-bk-b").game("studio.title");
        put(&b.saves_dir(U1).join("scid-1/profile.gsc"), b"old profile");
        put(&b.saves_dir(U1).join("scid-1/only-here.gsc"), b"old extra");
        put(&b.settings_toml(), b"[settings]\n\"game.dir\" = 'D:\\here'\n");
        let old_saves = tree(&b.saves_root());
        let plan = inspect(&b, &zip, &ImportOptions::default()).unwrap();
        assert_eq!(plan.replaced, vec!["saves/0009000000000001/scid-1/profile.gsc".to_string(), "settings.toml".to_string()]);
        assert_eq!(plan.removed, vec!["saves/0009000000000001/scid-1/only-here.gsc".to_string()]);
        let done = import_with(&b, &zip, &ImportOptions::default(), || false).unwrap();
        // the backup holds exactly what was there before the import
        let backup = done.backup.unwrap();
        assert!(backup.starts_with(b.backups_dir()));
        let (bm, files) = read_export(&backup).unwrap();
        assert_eq!(bm.users, vec![U1]);
        let backed: Vec<(String, Vec<u8>)> = files.iter().filter_map(|(k, v)| k.strip_prefix("saves/").map(|r| (r.to_string(), v.clone()))).collect();
        assert_eq!(backed, old_saves);
        assert_eq!(files["settings/settings.toml"], b"[settings]\n\"game.dir\" = 'D:\\here'\n");
        // the import replaced the user's saves whole, and kept this machine's game folder
        assert_eq!(tree(&b.saves_root()), tree(&a.saves_root()));
        let s = std::fs::read_to_string(b.settings_toml()).unwrap();
        assert!(s.contains("D:\\\\here") || s.contains("'D:\\here'"), "{s}");
        assert!(s.contains("2560"));
        assert!(done.notes.iter().any(|n| n.contains("game.dir")));
        // restoring the backup gives the old saves back
        import_with(&b, &backup, &ImportOptions::default(), || false).unwrap();
        assert_eq!(tree(&b.saves_root()).into_iter().filter(|(k, _)| k.starts_with("0009000000000001")).collect::<Vec<_>>(), old_saves);
    }

    #[test]
    fn one_user_exported_and_imported_as_this_machines_user() {
        let a = game_with_saves(&root("x-map-a"));
        let zip = a.dir.parent().unwrap().join("one.zip");
        let m = export(&a, &What { saves: Some(Users::One(U2)), settings: false }, &zip).unwrap();
        assert_eq!((m.users.clone(), m.files.len()), (vec![U2], 1));
        let st = root("x-map-b");
        let b = st.game("studio.title");
        let me = 0x0009_0000_0000_00aa;
        put(&b.saves_dir(U1).join("scid-1/profile.gsc"), b"untouched");
        let o = ImportOptions { to_user: Some(me), ..ImportOptions::default() };
        let done = import_with(&b, &zip, &o, || false).unwrap();
        assert_eq!(done.plan.users, vec![(U2, me)]);
        assert_eq!(std::fs::read(b.saves_dir(me).join("scid-1/profile.gsc")).unwrap(), b"second user");
        assert_eq!(std::fs::read(b.saves_dir(U1).join("scid-1/profile.gsc")).unwrap(), b"untouched", "other users are not touched");
        // several users onto one: refused unless one is chosen
        let all = a.dir.parent().unwrap().join("all.zip");
        export(&a, &What { saves: Some(Users::All), settings: false }, &all).unwrap();
        assert!(inspect(&b, &all, &o).unwrap_err().contains("choose"));
        let pick = ImportOptions { from_user: Some(U1), to_user: Some(me), ..ImportOptions::default() };
        assert_eq!(inspect(&b, &all, &pick).unwrap().users, vec![(U1, me)]);
        assert!(export(&a, &What { saves: Some(Users::One(0x77)), settings: false }, &zip).is_err(), "a user with no saves");
        // this machine's defaults: one foreign user maps to the offline user; several are kept as they are
        let one = read_export(&a.dir.parent().unwrap().join("one.zip")).map(|x| x.0);
        assert_eq!(one.unwrap().users, vec![U2], "a failed export writes nothing");
        let (m_all, _) = read_export(&all).unwrap();
        assert_eq!(ImportOptions::for_this_machine(&m_all).to_user, None);
        let single = Manifest { users: vec![U2], ..m_all.clone() };
        assert_eq!(ImportOptions::for_this_machine(&single).to_user, Some(crate::DEFAULT_USER));
        let mine = Manifest { users: vec![crate::DEFAULT_USER], ..m_all };
        assert_eq!(ImportOptions::for_this_machine(&mine).to_user, None);
    }
}
