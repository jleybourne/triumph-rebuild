use super::*;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh folder under the system temp dir, removed on drop.
pub(crate) struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rebuild-vfs-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// Hash of every name, size, modification time and byte under `root`, in sorted order.
pub(crate) fn hash_tree(root: &Path) -> (u64, usize) {
    fn walk(dir: &Path, rel: &str, h: &mut DefaultHasher, files: &mut usize) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().into_string().unwrap();
            let rel = format!("{rel}/{name}");
            // the file's own record: a directory entry's copy of the times is updated lazily by NTFS
            let meta = fs::metadata(e.path()).unwrap();
            h.write(rel.as_bytes());
            h.write_u64(meta.len());
            h.write_u8(meta.permissions().readonly() as u8);
            if let Ok(t) = meta.modified() {
                h.write_u128(t.duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
            }
            if meta.is_dir() {
                walk(&e.path(), &rel, h, files);
            } else {
                h.write(&fs::read(e.path()).unwrap());
                *files += 1;
            }
        }
    }
    let mut h = DefaultHasher::new();
    let mut files = 0;
    walk(root, "", &mut h, &mut files);
    (h.finish(), files)
}

/// base:   Data/Level1.bin "base-level1", Data/Sub/Deep.txt "deep", Readme.TXT "readme", Empty/
/// mod a:  data/level1.bin "a-level1", data/new.bin "a-new"
/// mod b:  DATA/LEVEL1.BIN "b-level1"
struct World {
    scratch: Scratch,
    base: PathBuf,
    mods: ModSet,
}

impl World {
    fn new(tag: &str) -> Self {
        let scratch = Scratch::new(tag);
        let base = scratch.0.join("game");
        put(&base, "Data/Level1.bin", "base-level1");
        put(&base, "Data/Sub/Deep.txt", "deep");
        put(&base, "Readme.TXT", "readme");
        fs::create_dir_all(base.join("Empty")).unwrap();
        let mods_dir = scratch.0.join("mods");
        put(&mods_dir, "a/data/level1.bin", "a-level1");
        put(&mods_dir, "a/data/new.bin", "a-new");
        put(&mods_dir, "b/DATA/LEVEL1.BIN", "b-level1");
        let mods = ModSet::discover(&mods_dir).unwrap();
        World { base, mods, scratch }
    }

    fn vfs(&self) -> Vfs {
        VfsBuilder::new(self.scratch.0.join("overlay"))
            .package('G', &self.base, &self.mods)
            .writable('T')
            .writable('S')
            .build()
            .unwrap()
    }
}

fn read_all(v: &Vfs, path: &str) -> String {
    let h = v.open(path, OpenOptions::read()).unwrap();
    let mut buf = vec![0; v.size(h).unwrap() as usize];
    assert_eq!(v.read_at(h, 0, &mut buf).unwrap(), buf.len());
    v.close(h).unwrap();
    String::from_utf8(buf).unwrap()
}

fn write_file(v: &Vfs, path: &str, d: Disposition, body: &str) {
    let h = v.open(path, OpenOptions::write(d)).unwrap();
    v.set_size(h, 0).unwrap();
    v.write_at(h, 0, body.as_bytes()).unwrap();
    v.close(h).unwrap();
}

// ---- V1 ----------------------------------------------------------------------------------------

#[test]
fn v1_package_prefix_maps_onto_the_base() {
    let w = World::new("v1g");
    let v = VfsBuilder::new(w.scratch.0.join("overlay")).package('G', &w.base, &ModSet::default()).build().unwrap();
    for p in [r"G:\Readme.TXT", r"g:\readme.txt", "G:/README.txt", r"G:readme.txt", r"\\?\G:\Readme.TXT", r"G:\Data\..\readme.txt"] {
        let r = v.resolve(p).unwrap_or_else(|e| panic!("{p}: {e}"));
        assert_eq!(r.layer, Layer::Base, "{p}");
        assert_eq!(r.host, fs::canonicalize(&w.base).unwrap().join("Readme.TXT"), "{p}: host keeps the disk's case");
        assert_eq!(r.len, 6);
    }
    assert!(v.resolve(r"G:\DATA\sub\DEEP.TXT").unwrap().host.ends_with("Data/Sub/Deep.txt"));
    assert!(v.resolve("G:").unwrap().is_dir);
    assert!(v.resolve(r"G:\data").unwrap().is_dir);
    assert!(matches!(v.resolve(r"G:\nope.bin"), Err(Error::NotFound)));
    assert!(matches!(v.resolve(r"G:\nodir\x.bin"), Err(Error::PathNotFound)));
    assert!(matches!(v.resolve(r"G:\Readme.TXT\x"), Err(Error::PathNotFound)), "a file is not a directory");
    assert!(matches!(v.resolve(r"T:\x"), Err(Error::NoDrive('T'))));
    assert!(matches!(v.resolve(r"data\x"), Err(Error::NotOnDrive(_))));
}

#[test]
fn v1_temp_and_storage_drives_are_overlay_only() {
    let w = World::new("v1ts");
    let v = w.vfs();
    for drive in ['T', 'S'] {
        assert!(v.enumerate(&format!("{drive}:\\")).unwrap().is_empty());
        v.create_dir(&format!("{drive}:\\Save0")).unwrap();
        write_file(&v, &format!("{drive}:\\save0\\Profile.dat"), Disposition::CreateNew, "p");
        let r = v.resolve(&format!("{drive}:/SAVE0/profile.DAT")).unwrap();
        assert_eq!(r.layer, Layer::Overlay);
        assert!(r.host.starts_with(w.scratch.0.join("overlay").join(drive.to_string())));
        assert!(r.host.ends_with("Save0/Profile.dat"), "created in the title's case");
    }
    assert!(!v.exists(r"T:\Readme.TXT"), "the base is the package drive's only");
}

#[test]
fn v1_overlay_inside_the_base_is_refused() {
    let w = World::new("v1unsafe");
    let r = VfsBuilder::new(w.base.join("overlay")).package('G', &w.base, &ModSet::default()).build();
    assert!(matches!(r, Err(Error::Unsafe(_))));
    let r = VfsBuilder::new(w.scratch.0.join("o")).writable('T').writable('t').build();
    assert!(matches!(r, Err(Error::Unsafe(_))), "a letter mounted twice");
}

#[test]
fn v1_a_refused_overlay_creates_nothing() {
    let w = World::new("v1nocreate");
    let before: Vec<_> = fs::read_dir(&w.base).unwrap().map(|e| e.unwrap().file_name()).collect();
    // nested, not yet existing, letters in another case than on disk: still found inside the base
    let base = if cfg!(windows) { w.base.to_string_lossy().to_uppercase() } else { w.base.to_string_lossy().into_owned() };
    let inside = PathBuf::from(base).join("deep").join("overlay");
    let r = VfsBuilder::new(&inside).writable('T').package('G', &w.base, &ModSet::default()).build();
    assert!(matches!(r, Err(Error::Unsafe(_))));
    let after: Vec<_> = fs::read_dir(&w.base).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(before, after, "no folder made in the base");
    // a base inside the drive's overlay folder is refused too
    let held = w.scratch.0.join("root").join("G").join("x");
    fs::create_dir_all(&held).unwrap();
    let r = VfsBuilder::new(w.scratch.0.join("root")).package('G', &held, &ModSet::default()).build();
    assert!(matches!(r, Err(Error::Unsafe(_))));
    // and a separate overlay still builds
    assert!(VfsBuilder::new(w.scratch.0.join("o")).package('G', &w.base, &ModSet::default()).build().is_ok());
}

// ---- V2 ----------------------------------------------------------------------------------------

#[test]
fn v2_write_heavy_run_leaves_the_base_unchanged() {
    let w = World::new("v2");
    // a wider base: 20 directories x 25 files, some read-only
    for i in 0..20 {
        for j in 0..25 {
            put(&w.base, &format!("Bulk{i:02}/File{j:02}.dat"), &format!("bulk {i} {j}"));
        }
    }
    let ro = w.base.join("Bulk00/File00.dat");
    let mut perms = fs::metadata(&ro).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&ro, perms).unwrap();
    let mods_root = w.scratch.0.join("mods");
    let before = hash_tree(&w.base);
    let mods_before = hash_tree(&mods_root);
    assert_eq!(before.1, 503);

    let v = w.vfs();
    let mut ops = 0;
    // 1. overwrite every bulk file in place (copy-up, then write), half through OpenExisting, half CreateAlways
    for i in 0..20 {
        for j in 0..25 {
            let p = format!(r"G:\bulk{i:02}\FILE{j:02}.DAT");
            let d = if j % 2 == 0 { Disposition::OpenExisting } else { Disposition::CreateAlways };
            write_file(&v, &p, d, &format!("new {i} {j}"));
            ops += 1;
        }
    }
    // 2. append to a base file without truncating
    let h = v.open(r"G:\Readme.txt", OpenOptions::write(Disposition::OpenAlways)).unwrap();
    let end = v.size(h).unwrap();
    v.write_at(h, end, b"+more").unwrap();
    v.close(h).unwrap();
    assert_eq!(read_all(&v, r"G:\README.TXT"), "readme+more");
    // 3. delete base files and a base directory tree
    for j in 0..25 {
        v.delete(&format!(r"G:\Bulk19\File{j:02}.dat")).unwrap();
        ops += 1;
    }
    v.remove_dir(r"G:\Bulk19").unwrap();
    assert!(!v.exists(r"G:\Bulk19"));
    v.remove_dir(r"G:\Empty").unwrap();
    // 4. rename base files and a whole base directory
    v.rename(r"G:\Data\Sub\Deep.txt", r"G:\Data\Shallow.txt", false).unwrap();
    v.rename(r"G:\Bulk18", r"G:\Moved\..\Renamed18", false).unwrap();
    assert!(!v.exists(r"G:\Bulk18\File00.dat"));
    assert_eq!(read_all(&v, r"G:\renamed18\file00.dat"), "new 18 0");
    // 5. create, then re-create what was deleted
    v.create_dir(r"G:\Bulk19").unwrap();
    write_file(&v, r"G:\Bulk19\Fresh.bin", Disposition::CreateNew, "fresh");
    assert_eq!(v.enumerate(r"G:\Bulk19").unwrap().len(), 1, "deleted base files stay hidden under the new directory");
    // 6. the read-only base file was copied up writable
    assert_eq!(read_all(&v, r"G:\Bulk00\File00.dat"), "new 0 0");
    // 7. many small writes at offsets on one file
    let h = v.open(r"G:\Data\Level1.bin", OpenOptions::write(Disposition::OpenExisting)).unwrap();
    for k in 0..1000u64 {
        v.write_at(h, k * 3, b"xyz").unwrap();
        ops += 1;
    }
    v.close(h).unwrap();
    assert_eq!(v.resolve(r"G:\Data\Level1.bin").unwrap().len, 3000);

    assert!(ops > 1500);
    assert_eq!(v.open_handles(), 0);
    assert_eq!(hash_tree(&w.base), before, "base folder changed");
    assert_eq!(hash_tree(&mods_root), mods_before, "a mod folder changed");

    // the state persists into the next run: same overlay folder, fresh Vfs
    drop(v);
    let v = w.vfs();
    assert!(!v.exists(r"G:\Bulk18"));
    assert!(!v.exists(r"G:\Data\Sub\Deep.txt"));
    assert!(!v.exists(r"G:\Empty"));
    assert_eq!(read_all(&v, r"G:\data\shallow.txt"), "deep");
    assert_eq!(read_all(&v, r"G:\Bulk05\File07.dat"), "new 5 7");
    assert_eq!(v.enumerate(r"G:\Bulk19").unwrap().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["Fresh.bin"]);
    assert_eq!(hash_tree(&w.base), before);
    let mut perms = fs::metadata(&ro).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&ro, perms).unwrap();
}

#[test]
fn v2_dispositions_and_errors() {
    let w = World::new("v2d");
    let v = w.vfs();
    assert!(matches!(v.open(r"G:\Readme.txt", OpenOptions::write(Disposition::CreateNew)), Err(Error::Exists)));
    assert!(matches!(v.open(r"G:\none.bin", OpenOptions::read()), Err(Error::NotFound)));
    assert!(matches!(v.open(r"G:\none\x.bin", OpenOptions::write(Disposition::CreateAlways)), Err(Error::PathNotFound)));
    assert!(matches!(v.open(r"G:\none.bin", OpenOptions::write(Disposition::TruncateExisting)), Err(Error::NotFound)));
    assert!(matches!(v.open(r"G:\Data", OpenOptions::read()), Err(Error::IsADirectory)));
    assert!(matches!(v.delete(r"G:\Data"), Err(Error::IsADirectory)));
    assert!(matches!(v.remove_dir(r"G:\Data"), Err(Error::DirectoryNotEmpty)));
    assert!(matches!(v.remove_dir(r"G:\Readme.txt"), Err(Error::NotADirectory)));
    assert!(matches!(v.create_dir(r"G:\data"), Err(Error::Exists)));
    assert!(matches!(v.rename(r"G:\Readme.txt", r"T:\x", false), Err(Error::CrossDrive)));
    assert!(matches!(v.rename(r"G:\Data", r"G:\Data\Sub\X", false), Err(Error::InvalidPath(_))));
    assert!(matches!(v.rename(r"G:\Readme.txt", r"G:\Data\Level1.bin", false), Err(Error::Exists)));
    v.rename(r"G:\Readme.txt", r"G:\Data\Level1.bin", true).unwrap();
    assert_eq!(read_all(&v, r"G:\Data\Level1.bin"), "readme");
    assert!(!v.exists(r"G:\Readme.txt"));
    // TruncateExisting on a base file shadows it with an empty overlay file
    let h = v.open(r"G:\Data\Sub\Deep.txt", OpenOptions::write(Disposition::TruncateExisting)).unwrap();
    assert_eq!(v.size(h).unwrap(), 0);
    v.close(h).unwrap();
    // read-only handles refuse writes
    let h = v.open(r"G:\Data\Sub\Deep.txt", OpenOptions::read()).unwrap();
    assert!(matches!(v.write_at(h, 0, b"x"), Err(Error::ReadOnlyHandle)));
    v.close(h).unwrap();
    // case-only rename keeps one entry
    write_file(&v, r"T:\a.txt", Disposition::CreateNew, "a");
    v.rename(r"T:\a.txt", r"T:\A.TXT", false).unwrap();
    assert_eq!(v.enumerate("T:").unwrap().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["A.TXT"]);
}

// ---- V3 ----------------------------------------------------------------------------------------

#[test]
fn v3_later_mods_win() {
    let w = World::new("v3order");
    let v = w.vfs();
    assert_eq!(w.mods.mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(read_all(&v, r"G:\Data\Level1.bin"), "b-level1");
    assert_eq!(v.resolve(r"G:\Data\Level1.bin").unwrap().layer, Layer::Mod("b".into()));
    assert_eq!(read_all(&v, r"G:\Data\New.bin"), "a-new");
    assert_eq!(read_all(&v, r"G:\Data\Sub\Deep.txt"), "deep", "the base shows through where no mod has the file");

    let mut mods = w.mods.clone();
    assert!(mods.move_to("b", 0));
    let v = VfsBuilder::new(w.scratch.0.join("o2")).package('G', &w.base, &mods).build().unwrap();
    assert_eq!(read_all(&v, r"G:\Data\Level1.bin"), "a-level1", "reordered: a is now last");

    // the merged listing: one entry per name, the top layer's
    let names: Vec<_> = v.enumerate(r"G:\data").unwrap().into_iter().map(|e| (e.name, e.layer)).collect();
    assert_eq!(
        names,
        [
            ("level1.bin".to_string(), Layer::Mod("a".into())),
            ("new.bin".to_string(), Layer::Mod("a".into())),
            ("Sub".to_string(), Layer::Base)
        ]
    );
}

#[test]
fn v3_enable_disable() {
    let w = World::new("v3en");
    let mut mods = w.mods.clone();
    assert!(mods.set_enabled("b", false));
    assert!(!mods.set_enabled("zzz", false));
    let v = VfsBuilder::new(w.scratch.0.join("o1")).package('G', &w.base, &mods).build().unwrap();
    assert_eq!(read_all(&v, r"G:\Data\Level1.bin"), "a-level1");
    mods.set_enabled("a", false);
    let v = VfsBuilder::new(w.scratch.0.join("o2")).package('G', &w.base, &mods).build().unwrap();
    assert_eq!(read_all(&v, r"G:\Data\Level1.bin"), "base-level1");
    assert!(!v.exists(r"G:\Data\New.bin"), "a disabled mod adds nothing");
    assert!(mods.conflicts().unwrap().is_empty());
}

#[test]
fn v3_conflicts() {
    let w = World::new("v3c");
    let mods_dir = w.scratch.0.join("mods");
    put(&mods_dir, "c/Data/New.bin", "c-new");
    put(&mods_dir, "c/Only/C.txt", "c");
    let mut mods = ModSet::discover(&mods_dir).unwrap();
    let c = mods.conflicts().unwrap();
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].path, "DATA/LEVEL1.BIN", "in the winner's case");
    assert_eq!(c[0].providers, ["a", "b"]);
    assert_eq!(c[0].winner, "b");
    assert_eq!(c[1].path, "Data/New.bin");
    assert_eq!(c[1].providers, ["a", "c"]);
    assert_eq!(c[1].winner, "c");
    mods.move_to("a", 2);
    let c = mods.conflicts().unwrap();
    assert_eq!(c[0].winner, "a");
    assert_eq!(c[1].winner, "a");
    mods.set_enabled("a", false);
    assert!(mods.conflicts().unwrap().is_empty());
    assert!(ModSet::discover(&w.scratch.0.join("no-mods")).unwrap().mods.is_empty());
}

#[test]
fn v3_a_write_over_a_mod_file_lands_in_the_overlay() {
    let w = World::new("v3w");
    let before = hash_tree(&w.scratch.0.join("mods"));
    let v = w.vfs();
    let h = v.open(r"G:\Data\Level1.bin", OpenOptions::write(Disposition::OpenExisting)).unwrap();
    v.write_at(h, 0, b"B").unwrap();
    v.close(h).unwrap();
    assert_eq!(read_all(&v, r"G:\Data\Level1.bin"), "B-level1", "copied up from the winning mod");
    v.delete(r"G:\Data\New.bin").unwrap();
    assert!(!v.exists(r"G:\Data\New.bin"));
    assert_eq!(hash_tree(&w.scratch.0.join("mods")), before);
}

// ---- V4 ----------------------------------------------------------------------------------------

#[test]
fn v4_handles() {
    let w = World::new("v4");
    let v = w.vfs();
    let a = v.open(r"G:\Readme.txt", OpenOptions::read()).unwrap();
    let b = v.open(r"G:\Data\Level1.bin", OpenOptions::read()).unwrap();
    assert_ne!(a, b);
    let mut buf = [0u8; 4];
    assert_eq!(v.read_at(a, 2, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"adme");
    assert_eq!(v.read_at(a, 4, &mut buf).unwrap(), 2, "short at the end");
    assert_eq!(v.read_at(a, 100, &mut buf).unwrap(), 0);
    v.close(a).unwrap();
    assert!(matches!(v.close(a), Err(Error::BadHandle)));
    let c = v.open(r"G:\Readme.txt", OpenOptions::read()).unwrap();
    assert!(matches!(v.size(a), Err(Error::BadHandle)), "a stale handle never reaches the reused slot");
    assert_eq!(v.size(c).unwrap(), 6);
    v.close(b).unwrap();
    v.close(c).unwrap();
    assert_eq!(v.open_handles(), 0);
    let names: Vec<_> = v.enumerate("G:").unwrap().into_iter().map(|e| (e.name, e.is_dir)).collect();
    assert_eq!(
        names,
        [("DATA".into(), true), ("Empty".into(), true), ("Readme.TXT".into(), false)] as [(String, bool); 3]
    );
}

#[test]
fn v4_shared_between_threads() {
    let w = World::new("v4t");
    let v = w.vfs();
    std::thread::scope(|s| {
        for t in 0..8 {
            let v = &v;
            s.spawn(move || {
                for i in 0..50 {
                    let p = format!(r"T:\t{t}-{i}.bin");
                    write_file(v, &p, Disposition::CreateNew, &format!("{t}/{i}"));
                    assert_eq!(read_all(v, &p), format!("{t}/{i}"));
                    assert_eq!(read_all(v, r"G:\Data\Level1.bin"), "b-level1");
                }
            });
        }
    });
    assert_eq!(v.enumerate("T:").unwrap().len(), 400);
    assert_eq!(v.open_handles(), 0);
}

/// The real game folder as the base (`REBUILD_GAME_DIR`; skipped when unset). Writes over the
/// smallest files and deletes and renames others, all through the overlay, and checks that every
/// name, size, time and attribute in the game folder, and every byte of each touched file, is the
/// same afterwards.
#[test]
fn v2_game_folder_unchanged() {
    let Some(game) = std::env::var_os("REBUILD_GAME_DIR").map(PathBuf::from) else {
        eprintln!("REBUILD_GAME_DIR unset: skipped");
        return;
    };
    fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, fs::Metadata)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let rel = format!("{rel}\\{}", e.file_name().into_string().unwrap());
            let meta = fs::metadata(e.path()).unwrap();
            if meta.is_dir() {
                walk(&e.path(), &rel, out);
            }
            out.push((rel, meta));
        }
    }
    let snapshot = |touched: &[String]| {
        let mut all = Vec::new();
        walk(&game, "", &mut all);
        let mut h = DefaultHasher::new();
        for (rel, meta) in &all {
            h.write(rel.as_bytes());
            h.write_u64(meta.len());
            h.write_u8(meta.permissions().readonly() as u8);
            h.write_u128(meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        }
        for rel in touched {
            h.write(&fs::read(game.join(rel.trim_start_matches('\\'))).unwrap());
        }
        (h.finish(), all)
    };
    let (_, all) = snapshot(&[]);
    let mut files: Vec<_> = all.iter().filter(|(_, m)| m.is_file()).collect();
    files.sort_by_key(|(_, m)| m.len());
    let touched: Vec<String> = files.iter().take(12).map(|(r, _)| r.clone()).collect();
    let before = snapshot(&touched).0;

    let scratch = Scratch::new("game");
    let v = VfsBuilder::new(&scratch.0).package('G', &game, &ModSet::default()).build().unwrap();
    for (i, rel) in touched.iter().enumerate() {
        let p = format!("G:{}", rel.to_uppercase());
        match i % 3 {
            0 => {
                let h = v.open(&p, OpenOptions::write(Disposition::OpenExisting)).unwrap();
                let n = v.size(h).unwrap();
                v.write_at(h, n, b"overlay write").unwrap();
                v.close(h).unwrap();
            }
            1 => v.delete(&p).unwrap(),
            _ => v.rename(&p, &format!("{p}.renamed"), false).unwrap(),
        }
    }
    for (rel, _) in files.iter().take(200) {
        let h = v.open(&format!("G:{rel}"), OpenOptions::read());
        if let Ok(h) = h {
            v.close(h).unwrap();
        }
    }
    drop(v);
    let after = snapshot(&touched).0;
    eprintln!("game folder: {} entries, {} touched files, hash {before:016x} before, {after:016x} after", all.len(), touched.len());
    assert_eq!(before, after, "the game folder changed");
}
