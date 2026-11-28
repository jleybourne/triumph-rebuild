use super::*;

use std::sync::OnceLock;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rebuild-shaderlib-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn manifest(version: &str, build: &str, corpus: &Path, state: &str, programs: u64) -> Manifest {
    let (corpus_size, corpus_modified) = corpus_stat(corpus).unwrap();
    Manifest { format: FORMAT, translator: version.into(), game_build: build.into(), corpus_size, corpus_modified, state: state.into(), programs, ..Manifest::default() }
}

#[test]
fn the_manifest_round_trips_and_status_reads_it() {
    let d = temp("status");
    let corpus = d.join("shaders.rpf");
    std::fs::write(&corpus, b"SGD5 corpus").unwrap();
    let lib = d.join("shaders");
    std::fs::create_dir_all(&lib).unwrap();
    assert_eq!(status(&lib, &corpus, "1.0", "v1"), Status::Missing);

    let mut m = manifest("v1", "1.0", &corpus, "building", 10);
    m.dxc = r"C:\dxc\dxc.exe".into();
    m.build_seconds = 12.5;
    assert_eq!(Manifest::parse(&m.to_toml()), Some(m.clone()), "{}", m.to_toml());
    m.write(&lib).unwrap();
    // part built: the journal's lines are the programs done
    std::fs::write(lib.join(JOURNAL), format!("{:016x}.ps {}\n{:016x}.cs {}\ntorn line", 1, "ab".repeat(32), 2, "cd".repeat(32))).unwrap();
    assert_eq!(status(&lib, &corpus, "1.0", "v1"), Status::Partial { done: 2, total: 10 });
    // another translator, another build, another corpus: stale
    assert!(matches!(status(&lib, &corpus, "1.0", "v2"), Status::Stale(w) if w.contains("translator")));
    assert!(matches!(status(&lib, &corpus, "1.1", "v1"), Status::Stale(w) if w.contains("game build")));
    std::fs::write(&corpus, b"SGD5 corpus, patched").unwrap();
    assert!(matches!(status(&lib, &corpus, "1.0", "v1"), Status::Stale(w) if w.contains("shaders.rpf")));
    // complete, but the library file is gone (cleared by hand): missing, so it is built again
    let m = manifest("v1", "1.0", &corpus, "complete", 10);
    m.write(&lib).unwrap();
    assert_eq!(status(&lib, &corpus, "1.0", "v1"), Status::Missing);
    std::fs::write(lib.join(LIBRARY), b"lib").unwrap();
    assert!(status(&lib, &corpus, "1.0", "v1").ready());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn records_round_trip_and_damage_is_found() {
    let r = Record { outcome: Outcome::Ok, stage: Stage::Ps, key: [7; 32], words: vec![[1, 2, 3, 4], [5, 6, 7, 8]], compare: 2, top_dword: Some(9), reason: String::new(), dxil: vec![0xde, 0xad, 0xbe, 0xef] };
    let b = r.encode();
    assert_eq!(Record::decode(&b).unwrap(), r);
    assert_eq!(head(&b), Some((Outcome::Ok, Stage::Ps, [7; 32])));
    let mut bad = b.clone();
    bad[70] ^= 1;
    assert!(Record::decode(&bad).unwrap_err().contains("damaged"));
    assert!(Record::decode(&b[..40]).is_err());
    let refused = Record { outcome: Outcome::Refused, stage: Stage::Cs, key: refusal_key("untranslated v_foo"), words: vec![], compare: 0, top_dword: None, reason: "untranslated v_foo".into(), dxil: vec![] };
    assert_eq!(Record::decode(&refused.encode()).unwrap(), refused);
    assert_eq!(parse_journal_line(&format!("00000000000000ff.vs {}", hex(&[3; 32]))), Some(((255, Stage::Vs), [3; 32])));
    assert_eq!(parse_journal_line("00000000000000ff.vs 03"), None);
}

/// The layer's choice: a dev override first, then the library the launcher built in the game's storage
/// cache, else none (every program on demand), never a stale or part-built one.
#[test]
fn the_layer_takes_a_dev_override_then_the_storage_library_then_the_dev_library_then_none() {
    let d = temp("choose");
    let env = rebuild_storage::Env { data_dir: Some(d.join("data")), temp: d.clone(), ..rebuild_storage::Env::default() };
    let store = rebuild_storage::Storage::resolve_with(&env).game("some.game").with_build("1.2.3.0");
    let shaders = store.shaders_dir();
    assert!(shaders.starts_with(d.join("data")) && shaders.ends_with(r"cache\1.2.3.0\shaders"), "{}", shaders.display());
    std::fs::create_dir_all(&shaders).unwrap();
    let corpus = d.join("shaders.rpf");
    std::fs::write(&corpus, b"x").unwrap();
    assert!(matches!(choose(None, None, Some(&shaders), None), Choice::None(w) if w.contains("no shader library")));
    // part built: not used (its programs come on demand meanwhile)
    manifest(VERSION, "1.2.3.0", &corpus, "building", 5).write(&shaders).unwrap();
    assert!(matches!(choose(None, None, Some(&shaders), None), Choice::None(w) if w.contains("part built")));
    // complete, of another translator: not used
    manifest("another", "1.2.3.0", &corpus, "complete", 5).write(&shaders).unwrap();
    std::fs::write(shaders.join(LIBRARY), b"lib").unwrap();
    assert!(matches!(choose(None, None, Some(&shaders), None), Choice::None(w) if w.contains("another translator")));
    // complete and current: the storage library, over no default at all
    manifest(VERSION, "1.2.3.0", &corpus, "complete", 5).write(&shaders).unwrap();
    assert_eq!(choose(None, None, Some(&shaders), None), Choice::Storage(shaders.join(LIBRARY)));
    // the dev overrides win over it
    let dev = PathBuf::from(r"D:\dev-data\pipelines");
    assert_eq!(choose(None, Some(dev.clone()), Some(&shaders), None), Choice::Explicit(dev.join(LIBRARY)));
    assert_eq!(choose(Some(dev.join("x.bin")), Some(dev.clone()), Some(&shaders), None), Choice::Explicit(dev.join("x.bin")));
    assert!(matches!(choose(None, None, None, None), Choice::None(_)));
    // the developer machine's library: after the storage one, before none, and only when it exists
    let dev_lib = d.join("dev-library.bin");
    assert!(matches!(choose(None, None, None, Some(&dev_lib)), Choice::None(_)), "absent: not chosen");
    std::fs::write(&dev_lib, b"lib").unwrap();
    assert_eq!(choose(None, None, None, Some(&dev_lib)), Choice::Dev(dev_lib.clone()));
    assert_eq!(choose(None, None, Some(&shaders), Some(&dev_lib)), Choice::Storage(shaders.join(LIBRARY)), "the game's own library wins");
    manifest(VERSION, "1.2.3.0", &corpus, "building", 5).write(&shaders).unwrap();
    assert_eq!(choose(None, None, Some(&shaders), Some(&dev_lib)), Choice::Dev(dev_lib.clone()), "part built: the dev library meanwhile");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_empty_library_loads_and_holds_nothing() {
    let l = crate::library::Library::empty(USER_DATA_DWORDS);
    assert!(l.is_empty() && l.get(1, Stage::Ps).is_none());
    assert_eq!(l.user_dwords(), USER_DATA_DWORDS);
}

// ---------------------------------------------------------------------------------------------------------
// with the corpus copy and DXC

/// One SGD5 record holding `payload` (the engine's shader file format, `rebuild_shader::sgd5`).
fn record_bytes(name: &str, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; zlib_rs::compress_bound(payload.len())];
    let config = zlib_rs::DeflateConfig { window_bits: -15, ..zlib_rs::DeflateConfig::new(6) };
    let (packed, rc) = zlib_rs::compress_slice(&mut buf, payload, config);
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    let n = (packed.len() + 12) as u32;
    let mut out = vec![0xF4, 0x01, 0, 0, name.len() as u8 + 1];
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&[0, 0x32]);
    for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(packed);
    out
}

fn write_corpus(path: &Path, containers: &[&Vec<u8>]) {
    let mut f = rebuild_shader::sgd5::MAGIC.to_vec();
    f.extend([0u8; 12]);
    for (i, c) in containers.iter().enumerate() {
        f.extend(record_bytes(&format!("X_test_{i}"), c));
    }
    std::fs::write(path, f).unwrap();
}

struct Fixture {
    /// small containers that lift, two of each stage, then a spare
    containers: Vec<(u64, Stage, Vec<u8>)>,
    dxc: PathBuf,
}

/// A few small programs from the corpus copy (`REBUILD_SHADERS`, else `D:\victory-data\shaders.rpf`; never
/// the game folder) that the translator lifts, and DXC. `None` (the test skips): no copy, no DXC, or the
/// measurement lock is held.
fn fixture() -> Option<&'static Fixture> {
    static F: OnceLock<Option<Fixture>> = OnceLock::new();
    F.get_or_init(|| {
        if Path::new(r"D:\Developer\triumph-rebuild\agents\MEASURING").exists() {
            eprintln!("skipped: the measurement lock is present");
            return None;
        }
        let rpf = std::env::var_os("REBUILD_SHADERS").map_or_else(|| PathBuf::from(r"D:\victory-data\shaders.rpf"), PathBuf::from);
        let Ok(data) = std::fs::read(&rpf) else {
            eprintln!("skipped: no corpus copy at {}", rpf.display());
            return None;
        };
        let Some(dxc) = translate::dxc_path() else {
            eprintln!("skipped: no dxc.exe");
            return None;
        };
        let t = Instant::now();
        let mut per: HashMap<Stage, usize> = HashMap::new();
        let mut containers = Vec::new();
        let mut seen = HashSet::new();
        for r in rebuild_shader::sgd5::records(&data) {
            if [Stage::Vs, Stage::Ps, Stage::Cs].iter().all(|s| per.get(s).copied().unwrap_or(0) >= 3) {
                break;
            }
            let Some(bytes) = r.unpack() else { continue };
            let Ok((p, isg1, osg1)) = translate::native_of(&bytes) else { continue };
            let Some(stage) = translate::host_stage(&p) else { continue };
            if per.get(&stage).copied().unwrap_or(0) >= 3 || translate::check_kind(&p).is_err() {
                continue;
            }
            let Ok(h) = translate::lift_hlsl(&p, &isg1, &osg1, true) else { continue };
            let Some(hash) = parse_hash(&blake3::hash(&bytes).to_hex()[..16]) else { continue };
            if h.src.len() > 12_000 || !seen.insert(hash) {
                continue;
            }
            *per.entry(stage).or_default() += 1;
            containers.push((hash, stage, bytes));
        }
        // two of each stage first, the spare last
        let mut out: Vec<(u64, Stage, Vec<u8>)> = Vec::new();
        for s in [Stage::Vs, Stage::Ps, Stage::Cs] {
            out.extend(containers.iter().filter(|c| c.1 == s).take(2).cloned());
        }
        if let Some(spare) = containers.iter().find(|c| !out.iter().any(|o| o.0 == c.0)) {
            out.push(spare.clone());
        }
        if out.len() < 4 {
            eprintln!("skipped: too few small translatable programs in the copy");
            return None;
        }
        eprintln!("fixture: {} programs picked in {:.1} s", out.len(), t.elapsed().as_secs_f64());
        Some(Fixture { containers: out, dxc })
    })
    .as_ref()
}

fn config(f: &Fixture, corpus: &Path, dir: &Path, version: &str) -> Config {
    let mut c = Config::new(corpus.to_path_buf(), dir.to_path_buf(), "1.0.0.0", f.dxc.clone());
    // the owner's rule while other lanes run: few DXC processes
    c.workers = 2;
    c.version = version.into();
    // normal priority: a test should not wait behind other lanes' work
    c.background = false;
    c
}

/// A build stopped part way (Cancel, the launcher closed) starts again where it stopped: the programs on
/// disk are not compiled again, and the library is byte-identical to one built in one go.
#[test]
fn a_stopped_build_resumes_where_it_stopped_and_gives_the_same_library() {
    let Some(f) = fixture() else { return };
    let d = temp("resume");
    let corpus = d.join("shaders.rpf");
    let base: Vec<&Vec<u8>> = f.containers.iter().take(f.containers.len() - 1).map(|c| &c.2).collect();
    write_corpus(&corpus, &base);
    let n = base.len();

    // in one go
    let whole = d.join("whole");
    let r = build(&config(f, &corpus, &whole, "v1"), &Control::new()).unwrap();
    assert!(r.complete, "{r:?}");
    assert_eq!((r.programs, r.compiled, r.reused), (n, n, 0), "{r:?}");
    assert_eq!(r.in_library + r.refused + r.failed, n, "{r:?}");
    assert!(r.in_library >= 2, "{r:?} {:?}", r.failures);

    // stopped after two compiles, one worker so exactly two
    let part = d.join("part");
    let mut c = config(f, &corpus, &part, "v1");
    c.workers = 1;
    c.stop_after = Some(2);
    let ctl = Control::new();
    let r = build(&c, &ctl).unwrap();
    assert!(!r.complete && r.compiled == 2, "{r:?}");
    assert_eq!(ctl.progress().phase, Phase::Paused);
    assert_eq!(status(&part, &corpus, "1.0.0.0", "v1"), Status::Partial { done: 2, total: n as u64 });
    assert!(usable(&part).is_err(), "a part-built library is never loaded");
    // resumed: only the rest goes through DXC
    c.stop_after = None;
    let ctl = Control::new();
    let r = build(&c, &ctl).unwrap();
    assert!(r.complete, "{r:?}");
    assert_eq!((r.compiled, r.reused), (n - 2, 2), "{r:?}");
    let p = ctl.progress();
    assert_eq!((p.phase, p.done, p.total), (Phase::Done, n, n));
    assert!(status(&part, &corpus, "1.0.0.0", "v1").ready());
    assert_eq!(std::fs::read(part.join(LIBRARY)).unwrap(), std::fs::read(whole.join(LIBRARY)).unwrap(), "the resumed library is the one-go library");
    // and nothing is left to do
    let r = build(&c, &Control::new()).unwrap();
    assert_eq!((r.compiled, r.reused), (0, n), "{r:?}");
    let _ = std::fs::remove_dir_all(&d);
}

/// A library of another translator version is stale and is rebuilt. The rebuild lifts everything again but
/// compiles only programs whose HLSL changed (here none, so no DXC at all), and is then current.
#[test]
fn a_translator_version_change_rebuilds_and_compiles_only_what_changed() {
    let Some(f) = fixture() else { return };
    let d = temp("version");
    let corpus = d.join("shaders.rpf");
    let base: Vec<&Vec<u8>> = f.containers.iter().take(f.containers.len() - 1).map(|c| &c.2).collect();
    write_corpus(&corpus, &base);
    let lib = d.join("shaders");
    let r = build(&config(f, &corpus, &lib, "old"), &Control::new()).unwrap();
    assert!(r.complete && r.compiled == base.len(), "{r:?}");
    let before = std::fs::read(lib.join(LIBRARY)).unwrap();
    assert!(matches!(status(&lib, &corpus, "1.0.0.0", "new"), Status::Stale(w) if w.contains("translator")));
    let r = build(&config(f, &corpus, &lib, "new"), &Control::new()).unwrap();
    assert!(r.complete, "{r:?}");
    assert_eq!((r.compiled, r.reused), (0, base.len()), "the same HLSL is never compiled twice: {r:?}");
    assert!(status(&lib, &corpus, "1.0.0.0", "new").ready());
    assert_eq!(Manifest::read(&lib).unwrap().translator, "new");
    assert_eq!(std::fs::read(lib.join(LIBRARY)).unwrap(), before);
    // a program whose record no longer matches (another DXC, a damaged file) is compiled again
    let victim = f.containers.iter().find(|c| lib.join(PROGRAMS).join(record_name(c.0, c.1)).is_file()).unwrap();
    let rec_path = lib.join(PROGRAMS).join(record_name(victim.0, victim.1));
    let mut b = std::fs::read(&rec_path).unwrap();
    b[30] ^= 0xff; // its key
    std::fs::write(&rec_path, b).unwrap();
    let r = build(&config(f, &corpus, &lib, "newer"), &Control::new()).unwrap();
    assert_eq!((r.compiled, r.reused), (1, base.len() - 1), "{r:?}");
    assert_eq!(std::fs::read(lib.join(LIBRARY)).unwrap(), before);
    let _ = std::fs::remove_dir_all(&d);
}

/// A game patch that changes one program: that program alone is compiled, the old one leaves the library,
/// and its record is deleted.
#[test]
fn one_changed_program_is_the_only_one_compiled_again() {
    let Some(f) = fixture() else { return };
    let d = temp("incremental");
    let corpus = d.join("shaders.rpf");
    let n = f.containers.len() - 1;
    let a: Vec<&Vec<u8>> = f.containers.iter().take(n).map(|c| &c.2).collect();
    write_corpus(&corpus, &a);
    let lib = d.join("shaders");
    let r = build(&config(f, &corpus, &lib, "v1"), &Control::new()).unwrap();
    assert!(r.complete, "{r:?}");
    // the last program replaced by the spare
    let (gone, new) = (&f.containers[n - 1], &f.containers[n]);
    let mut b = a.clone();
    b[n - 1] = &new.2;
    write_corpus(&corpus, &b);
    assert!(matches!(status(&lib, &corpus, "1.0.0.0", "v1"), Status::Stale(w) if w.contains("shaders.rpf")) || status(&lib, &corpus, "1.0.0.0", "v1").ready());
    let r = build(&config(f, &corpus, &lib, "v1"), &Control::new()).unwrap();
    assert!(r.complete, "{r:?}");
    assert_eq!((r.programs, r.compiled, r.reused), (n, 1, n - 1), "{r:?}");
    let (l, _) = crate::library::Library::load(&lib.join(LIBRARY), 2, true).unwrap();
    assert!(l.get(gone.0, gone.1).is_none(), "the replaced program left the library");
    assert!(!lib.join(PROGRAMS).join(record_name(gone.0, gone.1)).exists(), "and its record was deleted");
    let new_rec = Record::decode(&std::fs::read(lib.join(PROGRAMS).join(record_name(new.0, new.1))).unwrap()).unwrap();
    assert_eq!(l.get(new.0, new.1).is_some(), new_rec.outcome == Outcome::Ok);
    let _ = std::fs::remove_dir_all(&d);
}
