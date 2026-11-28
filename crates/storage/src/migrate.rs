//! Moving the files of the layout before this crate into it, once. **A save is never lost:** user data
//! (saves, the S: storage, settings) is copied, each file verified by hash, and the old files are left where
//! they were with a note beside them; a file the new place already has with different bytes is kept in
//! both places and reported. Only regenerable caches are moved (renamed), and a cache that can't be
//! renamed (another drive) is simply left.
//!
//! ```text
//! before                                              after
//! <launcher home>\library.toml, games\<id>\...        <data>\library.toml, games\<id>\...   (copied when the root changed)
//! games\<hash>\                                       games\<identity>\                      (merged; Library::migrate)
//! games\<id>\rebuild.toml (+ .bak)                    games\<id>\settings.toml (+ .bak)      (renamed)
//! games\<id>\sessions\<stamp>\                        <local>\logs\<id>\<stamp>\              (moved)
//! %APPDATA%\triumph-rebuild\overlay\GameSave\         games\<id>\saves\                      (copied, verified; note left)
//! %APPDATA%\triumph-rebuild\overlay\S\                games\<id>\storage\                    (copied, verified; note left)
//! %APPDATA%\triumph-rebuild\overlay\G\                games\<id>\overlay\G\                  (copied, verified; note left)
//! %LOCALAPPDATA%\triumph-rebuild\bcpack-cache\        games\<id>\cache\bcpack\               (renamed)
//! %LOCALAPPDATA%\triumph-rebuild\pipelines\           games\<id>\cache\<build>\pipelines\    (renamed)
//! %LOCALAPPDATA%\triumph-rebuild\images\              games\<id>\cache\<build>\images\       (renamed)
//! %LOCALAPPDATA%\triumph-rebuild\bcpack-tables.bin    <local>\bcpack-tables.bin              (copied when the root changed)
//! ```
//!
//! The old overlay held one title's files with no game named, so it goes to the first game launched after
//! the update (the only title that runs today).

use crate::{is_within, Env, GameStore, Storage, APP};
use std::io;
use std::path::{Path, PathBuf};

/// The note left in a folder whose files were copied into the new layout.
pub const NOTE: &str = "MIGRATED.txt";

/// Where things were before this layout.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Legacy {
    /// the launcher's home: `REBUILD_LAUNCHER_HOME`, the exe's folder when a `rebuild.toml` was there, else
    /// `%APPDATA%\triumph-rebuild`
    pub home: Option<PathBuf>,
    /// `%APPDATA%\triumph-rebuild` (the write overlay's parent)
    pub roaming: Option<PathBuf>,
    /// `%LOCALAPPDATA%\triumph-rebuild` (caches)
    pub local: Option<PathBuf>,
}

impl Legacy {
    pub fn from_env(env: &Env) -> Legacy {
        let roaming = env.appdata.as_ref().map(|a| a.join(APP));
        let local = env.local_appdata.as_ref().map(|a| a.join(APP));
        let home = env.legacy_home.clone().or_else(|| env.exe_dir.clone().filter(|d| d.join("rebuild.toml").is_file())).or_else(|| roaming.clone());
        Legacy { home, roaming, local }
    }

    /// The write overlay before this layout (`rebuild_vfs::default_overlay_root`).
    pub fn overlay(&self) -> Option<PathBuf> {
        self.roaming.as_ref().map(|r| r.join("overlay"))
    }
}

/// What a migration did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    /// files copied and verified
    pub copied: usize,
    /// folders or files renamed: (from, to)
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// files left where they were, and why (a different file already in the new place, an error)
    pub kept: Vec<(PathBuf, String)>,
    /// one line per step taken, for the log
    pub lines: Vec<String>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.copied == 0 && self.moved.is_empty() && self.kept.is_empty() && self.lines.is_empty()
    }
    fn add(&mut self, other: Report) {
        self.copied += other.copied;
        self.moved.extend(other.moved);
        self.kept.extend(other.kept);
        self.lines.extend(other.lines);
    }
}

fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    Ok(ma.len() == mb.len() && crate::hash_file(a)? == crate::hash_file(b)?)
}

/// Copy one file to a place that has none: through a temporary, verified by hash, then renamed.
pub fn copy_verified(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = dst.with_file_name(format!(".{}.migrate-{}", dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), std::process::id()));
    std::fs::copy(src, &tmp)?;
    if !same_bytes(src, &tmp)? {
        let _ = std::fs::remove_file(&tmp);
        return Err(io::Error::other(format!("{}: the copy does not match", src.display())));
    }
    if dst.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} appeared during the copy", dst.display())));
    }
    std::fs::rename(&tmp, dst).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Copy every file under `src` into `dst` (made as needed), never replacing: a file already there with
/// the same bytes is done, with other bytes it is kept (both copies stay) and reported. `skip` names
/// folders (relative to `src`, `/`-separated) not to copy. Returns whether every file is now in `dst`.
pub fn copy_tree(src: &Path, dst: &Path, skip: &[&str], report: &mut Report) -> bool {
    let mut complete = true;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf(), String::new())];
    while let Some((s, d, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&s) else {
            report.kept.push((s.clone(), "could not be read".into()));
            complete = false;
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let (sp, dp) = (e.path(), d.join(&name));
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                if !skip.contains(&r.as_str()) {
                    stack.push((sp, dp, r));
                }
                continue;
            }
            if !t.is_file() || name == NOTE {
                continue;
            }
            if dp.exists() {
                match same_bytes(&sp, &dp) {
                    Ok(true) => {}
                    Ok(false) => report.kept.push((sp, format!("{} already exists with other contents: both kept", dp.display()))),
                    Err(err) => {
                        report.kept.push((sp, err.to_string()));
                        complete = false;
                    }
                }
                continue;
            }
            match copy_verified(&sp, &dp) {
                Ok(()) => report.copied += 1,
                Err(err) => {
                    report.kept.push((sp, err.to_string()));
                    complete = false;
                }
            }
        }
    }
    complete
}

/// Move a folder: a rename when `dst` does not exist; otherwise child by child (folders merged, a file
/// present in both stays in `src`, reported). `src` is removed once empty.
pub fn merge_move(src: &Path, dst: &Path, report: &mut Report) {
    if !src.is_dir() {
        return;
    }
    if !dst.exists() {
        if let Some(p) = dst.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        match std::fs::rename(src, dst) {
            Ok(()) => return report.moved.push((src.to_path_buf(), dst.to_path_buf())),
            Err(_) => {
                // another drive: copy what can be copied, verified; the old folder stays
                if copy_tree(src, dst, &[], report) {
                    let _ = std::fs::remove_dir_all(src);
                    report.moved.push((src.to_path_buf(), dst.to_path_buf()));
                }
                return;
            }
        }
    }
    for e in std::fs::read_dir(src).into_iter().flatten().flatten() {
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if s.is_dir() {
            merge_move(&s, &d, report);
        } else if !d.exists() {
            match std::fs::rename(&s, &d) {
                Ok(()) => report.moved.push((s, d)),
                Err(err) => report.kept.push((s, err.to_string())),
            }
        } else if same_bytes(&s, &d).unwrap_or(false) {
            let _ = std::fs::remove_file(&s);
        } else {
            report.kept.push((s, format!("{} already exists with other contents: both kept", d.display())));
        }
    }
    let _ = std::fs::remove_dir(src);
}

fn note(dir: &Path, text: &str) {
    let _ = crate::write_atomic(&dir.join(NOTE), text.as_bytes());
}

/// The launcher's files from its old home, when the data root is now elsewhere (e.g. portable after
/// per-user): `library.toml`, `launcher.toml`, `games\` (sessions not copied: they are logs), each copied
/// and verified, the old home left with a note. Only when the new root has no library yet.
pub fn launcher_home(st: &Storage, legacy: &Legacy) -> Report {
    let mut r = Report::default();
    let Some(old) = &legacy.home else { return r };
    if is_within(old, &st.user) && is_within(&st.user, old) {
        return r;
    }
    if !old.join("library.toml").is_file() || st.library_toml().exists() || old.join(NOTE).exists() {
        return r;
    }
    let mut complete = true;
    for f in ["library.toml", "launcher.toml"] {
        let (s, d) = (old.join(f), st.user.join(f));
        if s.is_file() && !d.exists() {
            match copy_verified(&s, &d) {
                Ok(()) => r.copied += 1,
                Err(e) => {
                    complete = false;
                    r.kept.push((s, e.to_string()));
                }
            }
        }
    }
    let games = old.join("games");
    if games.is_dir() {
        let skip: Vec<String> = std::fs::read_dir(&games).into_iter().flatten().flatten().map(|e| format!("{}/sessions", e.file_name().to_string_lossy())).collect();
        let skip: Vec<&str> = skip.iter().map(String::as_str).collect();
        complete &= copy_tree(&games, &st.games_dir(), &skip, &mut r);
    }
    r.lines.push(format!("launcher data copied from {} to {} ({} files)", old.display(), st.user.display(), r.copied));
    if complete {
        note(old, &format!("The launcher's data was copied to {} on {}.\r\nNothing here was deleted; this folder can be removed once you are sure.\r\n", st.user.display(), crate::rfc3339(crate::now_unix())));
    }
    r
}

/// A game's folders under a new id (the identity instead of the old path hash): user data, caches and
/// logs, merged into whatever the new id already has.
pub fn rename_game(st: &Storage, old_id: &str, new_id: &str) -> Report {
    let mut r = Report::default();
    if old_id == new_id || !crate::is_game_id(old_id) || !crate::is_game_id(new_id) {
        return r;
    }
    let (a, b) = (st.game(old_id), st.game(new_id));
    for (s, d) in [(a.dir.clone(), b.dir.clone()), (a.cache.parent().map(Path::to_path_buf).unwrap_or_default(), b.cache.parent().map(Path::to_path_buf).unwrap_or_default()), (a.logs.clone(), b.logs.clone())] {
        if s.is_dir() {
            merge_move(&s, &d, &mut r);
        }
    }
    if !r.is_empty() {
        r.lines.push(format!("game {old_id} is now {new_id} (its package identity)"));
    }
    r
}

/// A game's folder in the new shape: `rebuild.toml` → `settings.toml` (and its `.bak`), its sessions moved
/// to the logs.
pub fn game_layout(game: &GameStore) -> Report {
    let mut r = Report::default();
    for (old, new) in [(game.legacy_settings_toml(), game.settings_toml()), (game.dir.join("rebuild.toml.bak"), game.dir.join("settings.toml.bak"))] {
        if old.is_file() {
            if new.exists() {
                r.kept.push((old, format!("{} already exists: both kept", new.display())));
            } else {
                match std::fs::rename(&old, &new) {
                    Ok(()) => r.moved.push((old, new)),
                    Err(e) => r.kept.push((old, e.to_string())),
                }
            }
        }
    }
    let sessions = game.dir.join("sessions");
    if sessions.is_dir() {
        merge_move(&sessions, &game.logs, &mut r);
    }
    r
}

/// The runtime's files from before this layout, into the game being launched: the old write overlay's
/// saves, S: storage and G: writes (copied, verified, the old folder left with a note), the old caches
/// (renamed), and the BCPack tables file (copied when the local root changed). Idempotent.
pub fn game_data(st: &Storage, game: &GameStore, legacy: &Legacy) -> Report {
    let mut r = Report::default();
    if let Some(ov) = legacy.overlay().filter(|o| o.is_dir() && !o.join(NOTE).exists()) {
        let mut part = Report::default();
        let mut complete = true;
        for (from, to) in [("GameSave", game.saves_root()), ("S", game.storage_dir()), ("G", game.overlay_dir().join("G"))] {
            let src = ov.join(from);
            if src.is_dir() {
                complete &= copy_tree(&src, &to, &[], &mut part);
            }
        }
        part.lines.push(format!("old overlay {}: {} files copied into game {} (saves, storage, G: writes); {} kept", ov.display(), part.copied, game.id, part.kept.len()));
        if complete {
            note(
                &ov,
                &format!(
                    "Saves (GameSave), storage (S) and G: writes here were copied to {} on {}, each file verified.\r\nNothing here was deleted. {} file(s) were already there with other contents and are kept in both places.\r\nThis folder can be removed once you are sure.\r\n",
                    game.dir.display(),
                    crate::rfc3339(crate::now_unix()),
                    part.kept.len()
                ),
            );
        }
        r.add(part);
    }
    if let Some(local) = &legacy.local {
        for (old, new) in [(local.join("bcpack-cache"), game.bcpack_cache_dir()), (local.join("pipelines"), game.pipelines_dir()), (local.join("images"), game.images_dir())] {
            if old.is_dir() && !new.exists() {
                if let Some(p) = new.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                match std::fs::rename(&old, &new) {
                    Ok(()) => r.moved.push((old, new)),
                    Err(e) => r.lines.push(format!("cache {} left where it is ({e}); it is regenerated", old.display())),
                }
            }
        }
        let tables = local.join("bcpack-tables.bin");
        let dst = st.bcpack_tables();
        if tables.is_file() && !dst.exists() && !(is_within(&tables, &dst) && is_within(&dst, &tables)) {
            match copy_verified(&tables, &dst) {
                Ok(()) => r.copied += 1,
                Err(e) => r.kept.push((tables, e.to_string())),
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp_dir;

    fn put(p: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// (relative path, bytes) of every file under a folder.
    pub(crate) fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut v = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                } else {
                    v.push((e.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/"), std::fs::read(e.path()).unwrap()));
                }
            }
        }
        v.sort();
        v
    }

    fn storage(root: &Path) -> Storage {
        Storage::resolve_with(&Env { data_dir: Some(root.join("data")), local_dir: Some(root.join("local")), temp: root.to_path_buf(), ..Env::default() })
    }

    #[test]
    fn the_old_overlay_is_copied_verified_and_never_deleted() {
        let root = temp_dir("mig-overlay");
        let legacy = Legacy { home: None, roaming: Some(root.join("roaming")), local: Some(root.join("oldlocal")) };
        let ov = legacy.overlay().unwrap();
        put(&ov.join("GameSave/0009000000000001/scid-1/profile.gsc"), b"save one");
        put(&ov.join("GameSave/0009000000000001/scid-1/slot2.gsc"), b"save two");
        put(&ov.join("S/settings.xml"), b"<settings/>");
        put(&ov.join("T/scratch.bin"), b"temp");
        put(&ov.join("G/written.dat"), b"g write");
        let before = tree(&ov);
        let st = storage(&root);
        let g = st.game("studio.title").with_build("1.0.0.0");
        // the new place already has a different slot2 (a newer save): it is kept, and so is the old one
        put(&g.saves_dir(0x0009_0000_0000_0001).join("scid-1/slot2.gsc"), b"newer save");

        let r = game_data(&st, &g, &legacy);
        assert_eq!(r.copied, 3, "{r:#?}");
        assert_eq!(r.kept.len(), 1, "the conflict is reported");
        assert_eq!(std::fs::read(g.saves_dir(0x0009_0000_0000_0001).join("scid-1/profile.gsc")).unwrap(), b"save one");
        assert_eq!(std::fs::read(g.saves_dir(0x0009_0000_0000_0001).join("scid-1/slot2.gsc")).unwrap(), b"newer save", "never replaced");
        assert_eq!(std::fs::read(g.storage_dir().join("settings.xml")).unwrap(), b"<settings/>");
        assert_eq!(std::fs::read(g.overlay_dir().join("G/written.dat")).unwrap(), b"g write");
        assert!(!g.overlay_dir().join("T").exists(), "T: is emptied every run: not copied");
        // every old file is still there, byte for byte, with a note beside them
        let mut after = tree(&ov);
        after.retain(|(n, _)| n != NOTE);
        assert_eq!(after, before);
        assert!(ov.join(NOTE).is_file());
        // once only
        let again = game_data(&st, &g, &legacy);
        assert_eq!(again.copied, 0);
        assert!(again.kept.is_empty());
    }

    #[test]
    fn a_failed_copy_leaves_no_note_so_it_is_tried_again() {
        let root = temp_dir("mig-retry");
        let legacy = Legacy { roaming: Some(root.join("roaming")), ..Legacy::default() };
        let ov = legacy.overlay().unwrap();
        put(&ov.join("GameSave/0009000000000001/s/a.gsc"), b"a");
        let st = storage(&root);
        let g = st.game("studio.title");
        // a file where the saves folder should be: nothing can be copied under it
        put(&g.saves_root(), b"in the way");
        let r = game_data(&st, &g, &legacy);
        assert_eq!(r.copied, 0);
        assert!(!r.kept.is_empty());
        assert!(!ov.join(NOTE).exists());
        std::fs::remove_file(g.saves_root()).unwrap();
        let r = game_data(&st, &g, &legacy);
        assert_eq!(r.copied, 1);
        assert!(ov.join(NOTE).exists());
        assert_eq!(std::fs::read(ov.join("GameSave/0009000000000001/s/a.gsc")).unwrap(), b"a", "the old file is kept");
    }

    #[test]
    fn caches_are_renamed_and_the_tables_copied() {
        let root = temp_dir("mig-cache");
        let legacy = Legacy { local: Some(root.join("oldlocal")), ..Legacy::default() };
        let old = legacy.local.clone().unwrap();
        put(&old.join("bcpack-cache/ab/cd.bin"), b"decoded");
        put(&old.join("pipelines/pso-cache.bin"), b"pso");
        put(&old.join("images/game-123.dll"), b"MZ");
        put(&old.join("bcpack-tables.bin"), &[5u8; 64]);
        let st = storage(&root);
        let g = st.game("studio.title").with_build("1.0.0.0");
        let r = game_data(&st, &g, &legacy);
        assert_eq!(r.moved.len(), 3, "{r:#?}");
        assert_eq!(std::fs::read(g.bcpack_cache_dir().join("ab/cd.bin")).unwrap(), b"decoded");
        assert_eq!(std::fs::read(g.pipelines_dir().join("pso-cache.bin")).unwrap(), b"pso");
        assert_eq!(std::fs::read(g.images_dir().join("game-123.dll")).unwrap(), b"MZ");
        assert!(g.pipelines_dir().starts_with(g.cache.join("1.0.0.0")));
        assert_eq!(std::fs::read(st.bcpack_tables()).unwrap(), vec![5u8; 64]);
        assert!(old.join("bcpack-tables.bin").is_file(), "copied, not moved");
    }

    #[test]
    fn settings_are_renamed_and_sessions_become_logs() {
        let root = temp_dir("mig-layout");
        let st = storage(&root);
        let g = st.game("studio.title");
        put(&g.legacy_settings_toml(), b"[settings]\n");
        put(&g.dir.join("rebuild.toml.bak"), b"[settings]\n# bak\n");
        put(&g.dir.join("sessions/20261127T153012Z-default/session.toml"), b"[session]\n");
        let r = game_layout(&g);
        assert_eq!(r.moved.len(), 3, "{r:#?}");
        assert_eq!(std::fs::read(g.settings_toml()).unwrap(), b"[settings]\n");
        assert!(g.dir.join("settings.toml.bak").is_file());
        assert!(g.sessions_dir().join("20261127T153012Z-default/session.toml").is_file());
        assert!(!g.dir.join("sessions").exists());
        assert!(game_layout(&g).is_empty(), "once");
    }

    #[test]
    fn a_game_renamed_to_its_identity_merges_without_losing_files() {
        let root = temp_dir("mig-rename");
        let st = storage(&root);
        let (old, new) = (st.game("0123456789abcdef"), st.game("studio.title"));
        put(&old.settings_toml(), b"old settings");
        put(&old.mods_dir().join("m/x.txt"), b"mod");
        put(&old.saves_dir(1).join("s/a.gsc"), b"old a");
        // the runtime migration ran first and made the new folder with a save of its own
        put(&new.saves_dir(1).join("s/a.gsc"), b"new a");
        put(&new.saves_dir(1).join("s/b.gsc"), b"new b");
        let r = rename_game(&st, &old.id, &new.id);
        assert_eq!(std::fs::read(new.settings_toml()).unwrap(), b"old settings");
        assert_eq!(std::fs::read(new.mods_dir().join("m/x.txt")).unwrap(), b"mod");
        assert_eq!(std::fs::read(new.saves_dir(1).join("s/a.gsc")).unwrap(), b"new a");
        assert_eq!(std::fs::read(new.saves_dir(1).join("s/b.gsc")).unwrap(), b"new b");
        assert_eq!(std::fs::read(old.saves_dir(1).join("s/a.gsc")).unwrap(), b"old a", "the conflicting old save stays");
        assert_eq!(r.kept.len(), 1);
        assert!(!old.settings_toml().exists());
    }

    #[test]
    fn the_launcher_home_is_copied_when_the_root_changes() {
        let root = temp_dir("mig-home");
        let home = root.join("roaming");
        put(&home.join("library.toml"), b"selected = \"studio.title\"\n");
        put(&home.join("launcher.toml"), b"[settings]\n");
        put(&home.join("games/studio.title/rebuild.toml"), b"[settings]\n");
        put(&home.join("games/studio.title/sessions/20261127T153012Z-x/frame-line.log"), b"fps 30\n");
        let legacy = Legacy { home: Some(home.clone()), roaming: Some(home.clone()), local: None };
        let st = Storage::resolve_with(&Env { exe_dir: Some(root.join("portable")), temp: root.clone(), ..Env::default() });
        assert_eq!(st.mode, crate::Mode::Portable);
        let r = launcher_home(&st, &legacy);
        assert_eq!(r.copied, 3, "{r:#?}");
        assert!(st.user.join("games/studio.title/rebuild.toml").is_file());
        assert!(!st.user.join("games/studio.title/sessions").exists(), "logs are not copied");
        assert!(home.join("library.toml").is_file() && home.join(NOTE).is_file());
        assert!(launcher_home(&st, &legacy).is_empty(), "once");
        // the same root: nothing to do
        let same = Storage::resolve_with(&Env { data_dir: Some(home.clone()), temp: root.clone(), ..Env::default() });
        assert!(launcher_home(&same, &Legacy { home: Some(home.clone()), ..Legacy::default() }).is_empty());
    }
}
