use super::*;

fn put(p: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                v.push((e.path().strip_prefix(dir).unwrap().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()));
            }
        }
    }
    v.sort();
    v
}

fn per_user_env(root: &Path) -> Env {
    Env { appdata: Some(root.join("Roaming")), local_appdata: Some(root.join("Local")), temp: root.join("Temp"), ..Env::default() }
}

#[test]
fn portable_when_the_launchers_folder_is_writable() {
    let root = temp_dir("portable");
    let exe = root.join("Triumph");
    std::fs::create_dir_all(&exe).unwrap();
    let st = Storage::resolve_with(&Env { exe_dir: Some(exe.clone()), ..per_user_env(&root) });
    assert_eq!(st.mode, Mode::Portable);
    assert_eq!((&st.user, &st.local, &st.cache), (&exe.join("data"), &exe.join("data"), &exe.join("data")));
    assert!(exe.join("data").is_dir(), "made on first use");
    assert_eq!(std::fs::read_dir(exe.join("data")).unwrap().count(), 0, "the write probe is gone");
    let g = st.game("studio.title").with_build("1.0.0.0");
    assert_eq!(g.saves_dir(0x0009_0000_0000_0001), exe.join(r"data\games\studio.title\saves\0009000000000001"));
    assert_eq!(g.shaders_dir(), exe.join(r"data\games\studio.title\cache\1.0.0.0\shaders"));
    assert_eq!(g.bcpack_cache_dir(), exe.join(r"data\games\studio.title\cache\bcpack"));
    assert_eq!(g.sessions_dir(), exe.join(r"data\logs\studio.title"));
    assert_eq!(g.settings_toml(), exe.join(r"data\games\studio.title\settings.toml"));
}

#[test]
fn per_user_when_the_folder_is_not_writable_or_is_a_cargo_target() {
    let root = temp_dir("peruser");
    // a "folder" under a file can't be made: as unwritable as Program Files
    put(&root.join("blocker"), b"file");
    let st = Storage::resolve_with(&Env { exe_dir: Some(root.join("blocker").join("app")), ..per_user_env(&root) });
    assert_eq!(st.mode, Mode::PerUser);
    assert_eq!(st.user, root.join(r"Roaming\triumph-rebuild"));
    assert_eq!(st.local, root.join(r"Local\triumph-rebuild"));
    assert_eq!(st.cache, st.local, "caches are local");
    let g = st.game("studio.title").with_build("2");
    assert!(g.saves_root().starts_with(&st.user) && g.pipelines_dir().starts_with(&st.local) && g.sessions_dir().starts_with(&st.local));
    // a dev build: target\release, marked by cargo's CACHEDIR.TAG; never portable (cargo clean would take saves)
    let target = root.join("repo").join("target");
    std::fs::create_dir_all(target.join("release").join("deps")).unwrap();
    put(&target.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172789f06886806bc55");
    for exe in [target.join("release"), target.join("release").join("deps")] {
        let st = Storage::resolve_with(&Env { exe_dir: Some(exe.clone()), ..per_user_env(&root) });
        assert_eq!(st.mode, Mode::PerUser, "{}", exe.display());
        assert!(!exe.join("data").exists());
    }
    // no APPDATA at all: everything under LOCALAPPDATA, and with neither the temp folder
    let st = Storage::resolve_with(&Env { appdata: None, ..per_user_env(&root) });
    assert_eq!(st.user, root.join(r"Local\triumph-rebuild"));
    let st = Storage::resolve_with(&Env { temp: root.join("T"), ..Env::default() });
    assert_eq!((st.user.clone(), st.local.clone()), (root.join(r"T\triumph-rebuild"), root.join(r"T\triumph-rebuild")));
}

#[test]
fn the_environment_overrides_everything() {
    let root = temp_dir("explicit");
    let exe = root.join("Triumph");
    std::fs::create_dir_all(&exe).unwrap();
    let st = Storage::resolve_with(&Env { data_dir: Some(root.join("D")), exe_dir: Some(exe.clone()), ..per_user_env(&root) });
    assert_eq!(st.mode, Mode::Explicit);
    assert_eq!((&st.user, &st.local, &st.cache), (&root.join("D"), &root.join("D"), &root.join("D")));
    assert!(!exe.join("data").exists(), "not portable when told otherwise");
    // the launcher hands its own roots to the layer: the child resolves exactly the same folders
    let per_user = Storage::resolve_with(&per_user_env(&root));
    let mut child = per_user_env(&root);
    for (k, v) in per_user.child_env() {
        match k {
            ENV_DATA_DIR => child.data_dir = Some(v),
            ENV_LOCAL_DIR => child.local_dir = Some(v),
            ENV_CACHE_DIR => child.cache_dir = Some(v),
            _ => unreachable!(),
        }
    }
    let c = Storage::resolve_with(&child);
    assert_eq!((c.user, c.local, c.cache), (per_user.user, per_user.local, per_user.cache));
    // the launcher home variable from before this layout still works
    let st = Storage::resolve_with(&Env { legacy_home: Some(root.join("H")), ..per_user_env(&root) });
    assert_eq!((st.mode, st.user), (Mode::Explicit, root.join("H")));
}

#[test]
fn the_cache_folder_moves_by_env_or_setting() {
    let root = temp_dir("cachedir");
    let env = per_user_env(&root);
    let st = Storage::resolve_with(&env);
    assert_eq!(st.cache_from, CacheFrom::Default);
    // the launcher setting, in launcher.toml
    put(&st.launcher_toml(), format!("[settings]\n\"launcher.theme\" = \"dark\"\n\"storage.cache_dir\" = '{}'\n", root.join("E").display()).as_bytes());
    let st = Storage::resolve_with(&env);
    assert_eq!((st.cache.clone(), st.cache_from), (root.join("E"), CacheFrom::Setting));
    let g = st.game("studio.title").with_build("1");
    assert!(g.shaders_dir().starts_with(root.join("E")));
    assert!(g.saves_root().starts_with(&st.user), "saves never move with the cache");
    assert!(g.sessions_dir().starts_with(&st.local), "nor do logs");
    // the variable wins over the setting
    let st = Storage::resolve_with(&Env { cache_dir: Some(root.join("F")), ..env.clone() });
    assert_eq!((st.cache.clone(), st.cache_from), (root.join("F"), CacheFrom::Env));
    // a relative setting is under the data root
    put(&st.launcher_toml(), b"[settings]\n\"storage.cache_dir\" = 'big-cache'\n");
    assert_eq!(Storage::resolve_with(&env).cache, st.user.join("big-cache"));
    // empty = default
    put(&st.launcher_toml(), b"[settings]\n\"storage.cache_dir\" = ''\n");
    assert_eq!(Storage::resolve_with(&env).cache_from, CacheFrom::Default);
    // a cache folder among the saves is refused
    let inside = st.user.join("games").join("studio.title").join("saves");
    let st = Storage::resolve_with(&Env { cache_dir: Some(inside), ..env.clone() });
    assert_eq!((st.cache.clone(), st.cache_from), (st.local.clone(), CacheFrom::Default));
    assert_eq!(st.notes.len(), 1);
}

/// Every kind of user data and every kind of cache, in a root where the caches share the data root
/// (portable), and in one with the cache root elsewhere.
#[test]
fn clearing_the_cache_never_touches_saves() {
    for split in [false, true] {
        let root = temp_dir("clear");
        let env = Env { data_dir: Some(root.join("data")), cache_dir: split.then(|| root.join("cache")), temp: root.clone(), ..Env::default() };
        let st = Storage::resolve_with(&env);
        let g = st.game("studio.title").with_build("1.1");
        put(&g.saves_dir(1).join("scid/a.gsc"), b"save");
        put(&g.storage_dir().join("settings.xml"), b"storage");
        put(&g.overlay_dir().join("G/x"), b"overlay");
        put(&g.settings_toml(), b"[settings]\n");
        put(&g.backups_dir().join("before-import.zip"), b"zip");
        put(&g.sessions_dir().join("20261128T000000Z-x/session.toml"), b"log");
        put(&g.shaders_dir().join("lib.bin"), &[1; 1000]);
        put(&g.pipelines_dir().join("pso-cache.bin"), &[2; 500]);
        put(&g.bcpack_cache_dir().join("ab.bin"), &[3; 200]);
        put(&g.clone().with_build("1.0").images_dir().join("old.dll"), &[4; 100]);
        let user = |g: &GameStore| [g.saves_root(), g.storage_dir(), g.overlay_dir(), g.backups_dir()].map(|d| tree(&d)).to_vec();
        let user_before = user(&g);
        let settings_before = std::fs::read(g.settings_toml()).unwrap();
        let logs_before = tree(&g.logs);
        let s = g.sizes();
        assert_eq!((s.saves, s.storage, s.cache_total), (4, 7, 1800));
        assert_eq!(s.cache.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["1.0", "1.1", "bcpack"]);

        // old builds only: the current build and the content-keyed bcpack stay
        assert_eq!(g.clear_cache(Some("1.1")).unwrap(), 100);
        assert_eq!(g.cache_entries().iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["1.1", "bcpack"]);
        // everything
        assert_eq!(g.clear_cache(None).unwrap(), 1700);
        assert_eq!(g.sizes().cache_total, 0);
        assert_eq!(user(&g), user_before, "saves, storage, overlay and backups are untouched");
        assert_eq!(std::fs::read(g.settings_toml()).unwrap(), settings_before);
        assert!(user_before[0].len() == 1 && user_before[3].len() == 1);
        assert_eq!(tree(&g.logs), logs_before);
    }
}

#[test]
fn clearing_refuses_anything_but_a_games_own_cache_folder() {
    let root = temp_dir("clear-refuse");
    let st = Storage::resolve_with(&Env { data_dir: Some(root.clone()), temp: root.clone(), ..Env::default() });
    let g = st.game("studio.title");
    put(&g.saves_dir(1).join("s/a.gsc"), b"save");
    // a forged store whose "cache" is the game's own folder, or its saves
    for cache in [g.dir.clone(), g.saves_root(), g.saves_root().join("cache"), root.clone()] {
        let forged = GameStore { cache: cache.clone(), ..g.clone() };
        assert!(forged.clear_cache(None).is_err(), "{}", cache.display());
    }
    let bad_id = GameStore { id: "..".into(), ..g.clone() };
    assert!(bad_id.clear_cache(None).is_err());
    assert_eq!(std::fs::read(g.saves_dir(1).join("s/a.gsc")).unwrap(), b"save");
    // a missing cache is simply empty
    assert_eq!(g.clear_cache(None).unwrap(), 0);
}

#[test]
fn users_and_times() {
    assert_eq!(user_dir_name(0x0009_0000_0000_0001), "0009000000000001");
    assert_eq!(parse_user("0009000000000001"), Some(0x0009_0000_0000_0001));
    assert_eq!(parse_user("9000000000001"), None);
    assert_eq!(parse_user("zz09000000000001"), None);
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(1_795_793_412), "2026-11-27T15:30:12Z");
    assert_eq!(stamp(951_782_400), "20000229T000000Z");
    let root = temp_dir("users");
    let st = Storage::resolve_with(&Env { data_dir: Some(root.clone()), temp: root.clone(), ..Env::default() });
    let g = st.game("studio.title");
    put(&g.saves_dir(2).join("s/a"), b"");
    put(&g.saves_dir(1).join("s/a"), b"");
    put(&g.saves_root().join("not-a-user/x"), b"");
    assert_eq!(g.save_users(), vec![1, 2]);
}

#[test]
fn the_current_game_and_its_caches() {
    let root = temp_dir("current");
    let pkg = root.join("pkg");
    put(&pkg.join("MicrosoftGame.config"), br#"<Game><Identity Name="Studio.Title" Version="3.0.1.0" /></Game>"#);
    let st = Storage::resolve_with(&Env { data_dir: Some(root.join("data")), temp: root.clone(), ..Env::default() });
    let g = st.game_at(&pkg);
    assert_eq!((g.id.as_str(), g.build.as_str()), ("studio.title", "3.0.1.0"));
    assert_eq!(g.images_dir(), root.join(r"data\games\studio.title\cache\3.0.1.0\images"));
    assert_eq!(st.shared_cache(CacheKind::Pipelines), root.join(r"data\shared\pipelines"));
    assert_eq!(st.game("x").build_cache(), root.join(r"data\games\x\cache\unknown"));
}

#[cfg(windows)]
#[test]
fn a_held_mutex_is_seen() {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::CreateMutexW;
    // its own name: holding the real run mutex here would refuse a title run on this machine
    let name = format!("Local\\triumph-rebuild-storage-test-{}", std::process::id());
    assert!(!mutex_exists(&name));
    let h = unsafe { CreateMutexW(None, true, &HSTRING::from(name.as_str())) }.unwrap();
    assert!(mutex_exists(&name));
    unsafe { CloseHandle(h) }.unwrap();
    assert!(!mutex_exists(&name));
}
