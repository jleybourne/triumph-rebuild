//! One **Import** for every way saves come in. [`detect`] looks at a file or folder and says what it is;
//! [`plan`] reads and checks all of it and says what each container will do (add, replace, unchanged);
//! [`apply`] checks again and writes, each container through [`store::write_container`] (backup first,
//! refused while the title runs). Nothing is written unless every container passes.
//!
//! | input | detected as |
//! |:--|:--|
//! | a `.zip` holding `manifest.toml` | a storage export: the caller hands it to `rebuild_storage::import` (its own preview) |
//! | a `.gsc` (or its `.gsc.json`) | one exported container, its manifest required |
//! | a `.json` with `"format": "rebuild-save-section"` | one decoded save, for the container it names |
//! | a folder of `<user>/<scid>/*.gsc` with manifests | a folder of container exports |
//! | a folder with `<name>~<hash>/v<N>/…` at any level | saves from the older build |
//!
//! Anything else is "not a save or export we recognise", and nothing changes.

use crate::format::Save;
use crate::json::{Fields, Json};
use crate::store::{self, Written};
use crate::{section, triumph};
use rebuild_gdk::areas::xgamesave::{file, Stored};
use std::path::{Path, PathBuf};

/// What an input is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A storage export (`.zip` with `manifest.toml`): imported by `rebuild_storage`, not here.
    StorageExport,
    Container,
    /// A decoded save for the container it names.
    Section(String),
    ExportFolder,
    OlderBuild,
}

impl Kind {
    /// The detected-type line.
    pub fn label(&self) -> String {
        match self {
            Kind::StorageExport => "an export of saves and settings (.zip)".into(),
            Kind::Container => "one exported container (.gsc with its manifest)".into(),
            Kind::Section(c) => format!("a decoded save (JSON) for {c}"),
            Kind::ExportFolder => "a folder of exported containers".into(),
            Kind::OlderBuild => "saves from the older build".into(),
        }
    }
}

pub const UNKNOWN: &str = "not a save or export we recognise: nothing was changed";

fn is_zip(p: &Path) -> bool {
    let mut magic = [0u8; 4];
    std::fs::File::open(p).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic)).is_ok() && magic == *b"PK\x03\x04"
}

/// The `.gsc` beside a `.gsc.json`, or the file itself.
fn gsc_of(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_suffix(".json") {
        Some(g) if g.ends_with(".gsc") => PathBuf::from(g),
        _ => p.to_path_buf(),
    }
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

/// Every exported container (a `.gsc` with its `.gsc.json` beside it) at or under `p`, a few levels
/// deep; `store::export_all` puts them in `<user>/<scid>/`, which names the user and configuration.
fn export_files(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut level = vec![p.to_path_buf()];
    for _ in 0..5 {
        let mut next = Vec::new();
        for d in level {
            if let Ok(r) = std::fs::read_dir(&d) {
                for f in r.flatten().map(|e| e.path()) {
                    if f.is_dir() {
                        next.push(f);
                    } else if ext(&f) == "gsc" && PathBuf::from(format!("{}.json", f.display())).is_file() {
                        out.push(f);
                    }
                }
            }
        }
        level = next;
    }
    out.sort();
    out
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn has_older_build(p: &Path) -> bool {
    let mut level = vec![p.to_path_buf()];
    for _ in 0..6 {
        let mut next = Vec::new();
        for d in level {
            if triumph::is_container_dir(&d) {
                return true;
            }
            next.extend(subdirs(&d));
        }
        level = next;
    }
    false
}

/// What `path` is. A recognised kind with a broken payload is still detected here; [`plan`] refuses it.
pub fn detect(path: &Path) -> Result<Kind, String> {
    if path.is_dir() {
        let exports = !export_files(path).is_empty();
        let older = has_older_build(path);
        return match (exports, older) {
            (true, true) => Err("this folder holds both exported containers and saves from the older build: choose one of the two folders inside it".into()),
            (true, false) => Ok(Kind::ExportFolder),
            (false, true) => Ok(Kind::OlderBuild),
            (false, false) => Err(UNKNOWN.into()),
        };
    }
    if !path.is_file() {
        return Err(format!("{} does not exist", path.display()));
    }
    if is_zip(path) {
        return match rebuild_storage::transfer::read_export(path) {
            Ok(_) => Ok(Kind::StorageExport),
            Err(e) if e.contains("no manifest.toml") => Err(UNKNOWN.into()),
            // an export of ours, but damaged: say so rather than "unknown"
            Err(e) => Err(format!("an export of saves and settings, but it cannot be used: {e}")),
        };
    }
    let g = gsc_of(path);
    if ext(&g) == "gsc" {
        let bytes = std::fs::read(&g).map_err(|e| format!("{}: {e}", g.display()))?;
        return match file::decode(&bytes) {
            Ok(_) => Ok(Kind::Container),
            Err(_) => Err(UNKNOWN.into()),
        };
    }
    if ext(path) == "json" {
        let text = std::fs::read_to_string(path).map_err(|_| UNKNOWN.to_string())?;
        let Ok(Json::Obj(pairs)) = Json::parse(&text) else { return Err(UNKNOWN.into()) };
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        if get("format") == Some(Json::str(section::FORMAT)) {
            if let Some(Json::Str(c)) = get("container") {
                return Ok(Kind::Section(c));
            }
        }
        return Err(UNKNOWN.into());
    }
    Err(UNKNOWN.into())
}

/// What a container's import will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add,
    Replace,
    Unchanged,
}

/// One container of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The provider folder it lands in, and the file.
    pub dir: PathBuf,
    pub path: PathBuf,
    pub user: String,
    pub scid: String,
    pub stored: Stored,
    pub change: Change,
    /// FNV-1a 64 of the file it replaces, if any (a plan is refused if that file changed since).
    pub before: Option<u64>,
}

/// Everything an import would do, checked; nothing written yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub kind: Kind,
    pub source: PathBuf,
    /// Empty for [`Kind::StorageExport`] (`rebuild_storage::inspect` plans that one).
    pub items: Vec<Item>,
}

impl Plan {
    pub fn changes(&self) -> usize {
        self.items.iter().filter(|i| i.change != Change::Unchanged).count()
    }
}

fn item(root: &Path, user: &str, scid: &str, stored: Stored) -> Result<Item, String> {
    if let Some(why) = store::refuse(&stored) {
        return Err(format!("{}: {why}", stored.name));
    }
    let dir = root.join(user).join(scid);
    let path = file::path(&dir, &stored.name);
    let old = std::fs::read(&path).ok();
    let change = match &old {
        Some(o) if *o == file::encode(&stored) => Change::Unchanged,
        Some(_) => Change::Replace,
        None => Change::Add,
    };
    Ok(Item { dir, path, user: user.into(), scid: scid.into(), stored, change, before: old.as_deref().map(store::fnv64) })
}

fn names_of(gsc: &Path) -> (String, String) {
    let scid = gsc.parent();
    let user = scid.and_then(Path::parent);
    let n = |p: Option<&Path>| p.and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (n(user), n(scid))
}

/// Reads and checks everything `path` holds for the saves folder `root`; `open` is the container the
/// editor has open (where a decoded save goes when several users have one of that name).
pub fn plan(root: &Path, path: &Path, open: Option<&Path>) -> Result<Plan, String> {
    let kind = detect(path)?;
    let mut items = Vec::new();
    match &kind {
        Kind::StorageExport => {}
        Kind::Container => {
            let gsc = gsc_of(path);
            let c = store::check_export(&gsc)?;
            // a single container goes beside the one of its name that is open, or into its export's folders
            let (user, scid) = match open.filter(|o| file::decode(&std::fs::read(o).unwrap_or_default()).is_ok_and(|x| x.name == c.name)) {
                Some(o) => names_of(o),
                None => manifest_names(&gsc)?,
            };
            items.push(item(root, &user, &scid, c)?);
        }
        Kind::ExportFolder => {
            for f in export_files(path) {
                let c = store::check_export(&f).map_err(|e| format!("{}: {e}", f.display()))?;
                let (user, scid) = manifest_names(&f)?;
                items.push(item(root, &user, &scid, c)?);
            }
        }
        Kind::OlderBuild => {
            for f in triumph::find(path)? {
                if !triumph::plain_names(&f) {
                    return Err(format!("{}/{}: folder names the saves folder would not use as they are", f.user, f.scid));
                }
                items.push(item(root, &f.user.clone(), &f.scid.clone(), f.stored)?);
            }
        }
        Kind::Section(name) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut targets: Vec<PathBuf> = store::list(root).into_iter().filter(|e| e.container.as_ref().is_some_and(|c| c.0 == *name)).map(|e| e.path).collect();
            if let Some(o) = open.filter(|o| targets.iter().any(|t| t == o)) {
                targets = vec![o.to_path_buf()];
            }
            let target = match targets.len() {
                0 => return Err(format!("there is no container {name:?} to import this decoded save into")),
                1 => targets.remove(0),
                n => return Err(format!("{n} users have a container {name:?}: open the one to import into, then import again")),
            };
            let cur_c = store::read(&target)?;
            let cur: Save = store::save_of(&cur_c).ok_or("that container holds no save")??;
            let s = section::import(&text, name, &cur)?;
            let mut c = store::with_save(&cur_c, &s);
            if s != cur {
                c.modified = store::now();
            }
            let (user, scid) = names_of(&target);
            items.push(item(root, &user, &scid, c)?);
        }
    }
    if kind != Kind::StorageExport && items.is_empty() {
        return Err(format!("{} holds no containers: nothing was changed", path.display()));
    }
    let mut seen: Vec<&PathBuf> = items.iter().map(|i| &i.path).collect();
    seen.sort();
    if seen.windows(2).any(|w| w[0] == w[1]) {
        return Err("two of the inputs are the same container for the same user: nothing was changed".into());
    }
    Ok(Plan { kind, source: path.to_path_buf(), items })
}

/// The user and configuration an exported container's manifest records.
fn manifest_names(gsc: &Path) -> Result<(String, String), String> {
    let man = PathBuf::from(format!("{}.json", gsc.display()));
    let text = std::fs::read_to_string(&man).map_err(|e| format!("{}: {e}", man.display()))?;
    let mut f = Fields::of(Json::parse(&text)?, "manifest")?;
    let user = f.string("user")?;
    let scid = f.string("scid")?;
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'));
    if !ok(&user) || !ok(&scid) {
        return Err("the manifest's user and configuration are not folder names the saves folder uses".into());
    }
    Ok((user, scid))
}

/// Carries out a plan made by [`plan`]: it is made again (the files may have changed since), must say
/// the same, and then every changed container is written with a backup first.
pub fn apply(root: &Path, p: &Plan, open: Option<&Path>, running: &dyn Fn() -> bool) -> Result<Vec<Written>, String> {
    if p.kind == Kind::StorageExport {
        return Err("an export of saves and settings is imported by the storage import".into());
    }
    if running() {
        return Err("the title is running: saves are only written while it is closed".into());
    }
    let again = plan(root, &p.source, open)?;
    let strip = |p: &Plan| p.items.iter().map(|i| (i.path.clone(), i.change, i.before, file::encode(&i.stored))).collect::<Vec<_>>();
    if strip(&again) != strip(p) {
        return Err("the input or the saves changed since the preview: look again before importing".into());
    }
    again.items.iter().filter(|i| i.change != Change::Unchanged).map(|i| store::write_container(root, &i.dir, &i.stored, running)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{build, DATA_BLOB, HEADER_BLOB};

    fn temp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!("rebuild-saves-import-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn container(name: &str, body: u8) -> Stored {
        let s = build(5, name, 0x6b07_786d, "", &[body; 40]).unwrap();
        Stored { name: name.into(), display: name.into(), modified: 1_764_000_000, blobs: vec![(HEADER_BLOB.into(), s.header.to_bytes()), (DATA_BLOB.into(), s.data())] }
    }

    const U: &str = "0009000000000001";
    const S: &str = "00000000-0000-0000-0000-00007952987a";

    /// A saves folder with `names`, each a container.
    fn ours(base: &Path, names: &[&str]) -> PathBuf {
        let root = base.join("saves");
        let dir = root.join(U).join(S);
        std::fs::create_dir_all(&dir).unwrap();
        for n in names {
            let c = container(n, 1);
            file::write(&file::path(&dir, n), &c).unwrap();
        }
        root
    }

    fn fnv64(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }

    /// The older build's layout for `c` under `base/saves/U/S/`.
    fn older(base: &Path, c: &Stored) -> PathBuf {
        let d = base.join("saves").join(U).join(S).join(format!("{}~{:016x}", c.name, fnv64(&c.name))).join("v3");
        std::fs::create_dir_all(&d).unwrap();
        let mut man = Vec::new();
        for s in [c.name.as_str(), c.display.as_str()].into_iter().chain(c.blobs.iter().map(|b| b.0.as_str())) {
            man.extend_from_slice(&(s.len() as u32).to_le_bytes());
            man.extend_from_slice(s.as_bytes());
        }
        std::fs::write(d.join("manifest"), man).unwrap();
        for (i, b) in c.blobs.iter().enumerate() {
            std::fs::write(d.join(i.to_string()), &b.1).unwrap();
        }
        d.parent().unwrap().to_path_buf()
    }

    const IDLE: &dyn Fn() -> bool = &|| false;

    #[test]
    fn a_container_export() {
        let base = temp("one");
        let root = ours(&base, &["Player"]);
        let (gsc, man) = store::export_container(&file::path(&root.join(U).join(S), "Player"), &base.join("out")).unwrap();
        assert_eq!(detect(&gsc).unwrap(), Kind::Container);
        assert_eq!(detect(&man).unwrap(), Kind::Container);
        // into a fresh saves folder: added, in the manifest's user and configuration
        let fresh = base.join("fresh");
        let p = plan(&fresh, &man, None).unwrap();
        assert_eq!((p.items.len(), p.items[0].change, p.items[0].user.as_str()), (1, Change::Add, U));
        let w = apply(&fresh, &p, None, IDLE).unwrap();
        assert_eq!(std::fs::read(&w[0].path).unwrap(), std::fs::read(&gsc).unwrap());
        // over itself: unchanged, nothing written
        let p = plan(&root, &gsc, None).unwrap();
        assert_eq!((p.items[0].change, p.changes()), (Change::Unchanged, 0));
        assert!(apply(&root, &p, None, IDLE).unwrap().is_empty());
        // without its manifest: recognised, refused
        std::fs::remove_file(&man).unwrap();
        assert_eq!(detect(&gsc).unwrap(), Kind::Container);
        assert!(plan(&fresh, &gsc, None).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_decoded_save() {
        let base = temp("section");
        let root = ours(&base, &["Player"]);
        let path = file::path(&root.join(U).join(S), "Player");
        let before = std::fs::read(&path).unwrap();
        let cur = store::save_of(&store::read(&path).unwrap()).unwrap().unwrap();
        let j = base.join("player.save.json");
        std::fs::write(&j, section::export("Player", &cur).replace("\"name\": \"Player\"", "\"name\": \"Renamed\"")).unwrap();
        assert_eq!(detect(&j).unwrap(), Kind::Section("Player".into()));
        let p = plan(&root, &j, None).unwrap();
        assert_eq!(p.items[0].change, Change::Replace);
        assert!(apply(&root, &p, None, &|| true).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let w = apply(&root, &p, None, IDLE).unwrap();
        assert!(w[0].backup.is_some());
        assert_eq!(store::save_of(&store::read(&path).unwrap()).unwrap().unwrap().header.name, "Renamed");
        // a decoded save for a container that is not here
        std::fs::write(&j, section::export("Nobody", &cur)).unwrap();
        assert!(plan(&root, &j, None).unwrap_err().contains("no container"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_decoded_save_with_two_owners_needs_the_open_one() {
        let base = temp("twoowners");
        let root = ours(&base, &["Player"]);
        let other = root.join("0009000000000002").join(S);
        std::fs::create_dir_all(&other).unwrap();
        file::write(&file::path(&other, "Player"), &container("Player", 1)).unwrap();
        let cur = store::save_of(&container("Player", 1)).unwrap().unwrap();
        let j = base.join("p.json");
        std::fs::write(&j, section::export("Player", &cur)).unwrap();
        assert!(plan(&root, &j, None).unwrap_err().contains("2 users"));
        let open = file::path(&other, "Player");
        let p = plan(&root, &j, Some(&open)).unwrap();
        assert_eq!(p.items[0].path, open);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_of_exports() {
        let base = temp("folder");
        let root = ours(&base, &["Player", "SettingsProfileSettings"]);
        let out = base.join("all");
        store::export_all(&root, &out).unwrap();
        assert_eq!(detect(&out).unwrap(), Kind::ExportFolder);
        let fresh = base.join("fresh");
        let p = plan(&fresh, &out, None).unwrap();
        assert_eq!(p.changes(), 2);
        assert_eq!(apply(&fresh, &p, None, IDLE).unwrap().len(), 2);
        // one damaged export: the whole folder is refused
        let f = export_files(&out)[0].clone();
        let mut b = std::fs::read(&f).unwrap();
        let n = b.len();
        b[n - 1] ^= 1;
        std::fs::write(&f, b).unwrap();
        let third = base.join("third");
        assert!(plan(&third, &out, None).is_err());
        assert!(!third.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_older_build_at_any_level() {
        let base = temp("older");
        let src = base.join("victory");
        let cdir = older(&src, &container("SettingsProfileSettings", 2));
        older(&src, &container("Player", 3));
        let levels = [src.clone(), src.join("saves"), src.join("saves").join(U), src.join("saves").join(U).join(S)];
        for (k, l) in levels.iter().enumerate() {
            assert_eq!(detect(l).unwrap(), Kind::OlderBuild, "level {k}");
            let p = plan(&base.join(format!("fresh{k}")), l, None).unwrap();
            assert_eq!(p.items.len(), 2, "level {k}");
            assert!(p.items.iter().all(|i| i.user == U && i.scid == S));
        }
        // one container folder on its own
        assert_eq!(detect(&cdir).unwrap(), Kind::OlderBuild);
        let p = plan(&base.join("fresh-one"), &cdir, None).unwrap();
        assert_eq!((p.items.len(), p.items[0].stored.name.as_str(), p.items[0].user.as_str()), (1, "SettingsProfileSettings", U));
        // the older build's folders are never touched
        let w = apply(&base.join("fresh-one"), &p, None, IDLE).unwrap();
        assert_eq!(w.len(), 1);
        assert!(cdir.join("v3").join("manifest").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ambiguous_and_unknown_inputs_change_nothing() {
        let base = temp("unknown");
        let root = ours(&base, &["Player"]);
        // both kinds in one folder
        let mixed = base.join("mixed");
        store::export_all(&root, &mixed.join("exports")).unwrap();
        older(&mixed.join("old"), &container("Player", 4));
        assert!(detect(&mixed).unwrap_err().contains("both"));
        assert_eq!(detect(&mixed.join("exports")).unwrap(), Kind::ExportFolder);
        // unrecognised files and folders
        let cases = [("notes.txt", b"hello".to_vec()), ("x.json", br#"{"format": "something else"}"#.to_vec()), ("bad.json", b"{".to_vec()), ("fake.gsc", b"RBGSAVE1 but no".to_vec()), ("a.zip", b"PK\x03\x04garbage".to_vec())];
        for (n, b) in cases {
            let f = base.join(n);
            std::fs::write(&f, b).unwrap();
            let e = detect(&f).unwrap_err();
            assert!(e.contains("not a save or export we recognise") || n == "a.zip", "{n}: {e}");
            assert!(plan(&root, &f, None).is_err());
        }
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(detect(&empty).unwrap_err(), UNKNOWN);
        assert!(detect(&base.join("missing")).is_err());
        assert_eq!(store::list(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A storage export is recognised (and left to the storage import); a zip without its manifest is not.
    #[test]
    fn storage_exports() {
        let base = temp("zip");
        let env = rebuild_storage::Env { appdata: Some(base.join("Roaming")), local_appdata: Some(base.join("Local")), temp: base.join("Temp"), ..Default::default() };
        let st = rebuild_storage::Storage::resolve_with(&env);
        let g = st.game("studio.title").with_build("1.0.0.0");
        let dir = g.saves_dir(0x0009_0000_0000_0001).join(S);
        std::fs::create_dir_all(&dir).unwrap();
        file::write(&file::path(&dir, "Player"), &container("Player", 1)).unwrap();
        let zip = base.join("export.zip");
        rebuild_storage::export(&g, &rebuild_storage::What::EVERYTHING, &zip).unwrap();
        assert_eq!(detect(&zip).unwrap(), Kind::StorageExport);
        let p = plan(&g.saves_root(), &zip, None).unwrap();
        assert_eq!((&p.kind, p.items.len()), (&Kind::StorageExport, 0));
        assert!(apply(&g.saves_root(), &p, None, IDLE).is_err());
        let plain = base.join("plain.zip");
        rebuild_storage::archive::write(&plain, &[rebuild_storage::archive::Entry { name: "readme.txt".into(), data: b"hi".to_vec() }]).unwrap();
        assert_eq!(detect(&plain).unwrap_err(), UNKNOWN);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_plan_is_checked_again_before_writing() {
        let base = temp("again");
        let root = ours(&base, &["Player"]);
        let src = base.join("victory");
        older(&src, &container("Player", 9));
        let p = plan(&root, &src, None).unwrap();
        assert_eq!(p.items[0].change, Change::Replace);
        // the saves changed after the preview
        file::write(&file::path(&root.join(U).join(S), "Player"), &container("Player", 7)).unwrap();
        assert!(apply(&root, &p, None, IDLE).unwrap_err().contains("changed since the preview"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
