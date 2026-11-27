//! Build 4: the pipeline library.
//!
//! ```text
//! pipelines build    the library from the translator's DXIL (REBUILD_PIPELINES_IN, default
//!                    D:\victory-data\pipelines-in) and the immediate sampler words, lifted again from the
//!                    corpus (REBUILD_SHADERS, a copy of shaders.rpf); written to REBUILD_PIPELINES_OUT
//!                    (default D:\victory-data\pipelines)\library.bin. Every program's own root signature is
//!                    serialized and checked.
//! pipelines load     load times (threads 1-8, verified or not) and lookup cost
//! pipelines check    GPU: every root signature in the library created on the device, a compute pipeline for
//!                    every compute program, a graphics pipeline (default state) for every pixel shader with a
//!                    vertex shader whose outputs link. --debug: the debug layer, for refusal reasons.
//!                    Runs only while agents\MEASURING is absent.
//! ```
//! Outputs stay outside git (game-derived bytes).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rebuild_pipelines::bindings::{self, Compiled};
use rebuild_pipelines::layout::{self, StageLayout, USER_DATA_DWORDS};
use rebuild_pipelines::library::{self, Input, Library};
use rebuild_pipelines::{parse_hash, Stage};
use rebuild_shader::signature::Element;

const LOCK: &str = r"D:\Developer\triumph-rebuild\agents\MEASURING";

fn env_path(k: &str, default: &str) -> PathBuf {
    std::env::var_os(k).map(PathBuf::from).unwrap_or_else(|| default.into())
}

fn library_path() -> PathBuf {
    env_path("REBUILD_PIPELINES_OUT", r"D:\victory-data\pipelines").join("library.bin")
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build") => build(),
        Some("load") => load(),
        Some("check") => check(args.iter().any(|a| a == "--debug")),
        _ => {
            eprintln!("pipelines build | load | check [--debug]");
            std::process::exit(2);
        }
    }
}

/// The translator's outputs: (hash, stage) â†’ DXIL path.
fn inputs(dir: &Path) -> BTreeMap<(u64, Stage), PathBuf> {
    let mut m = BTreeMap::new();
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let parts: Vec<&str> = name.split('.').collect();
        if let [h, s, "dxil"] = parts[..] {
            if let (Some(h), Some(s)) = (parse_hash(h), Stage::from_name(s)) {
                m.insert((h, s), e.path());
            }
        }
    }
    m
}

/// What the lifter knows and the DXIL does not: the immediate sampler words, and the highest user-data dword
/// read (root constants must cover it).
struct Lifted {
    words: Vec<[u32; 4]>,
    top_dword: Option<u8>,
}

/// Each wanted program, lifted the way `translate` lifts it.
fn sampler_words(want: &BTreeMap<(u64, Stage), PathBuf>) -> HashMap<(u64, Stage), Lifted> {
    use rebuild_shader::native::{HwStage, StageIo};
    use rebuild_sir::lift;
    let path = std::env::var("REBUILD_SHADERS").unwrap_or_else(|_| {
        eprintln!("pipelines: set REBUILD_SHADERS to a copy of shaders.rpf (never the game folder)");
        std::process::exit(2);
    });
    let data = std::fs::read(&path).expect("read the shaders.rpf copy");
    let walk = rebuild_shader::census::walk(&data, 8);
    let next = AtomicUsize::new(0);
    let out = Mutex::new(HashMap::new());
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(p) = walk.programs.get(i) else { break };
                let stage = match (&p.native.io, p.native.stage) {
                    (StageIo::Ps { .. }, _) => Stage::Ps,
                    (StageIo::Vs { .. }, HwStage::Vs) => Stage::Vs,
                    (StageIo::Cs(_), _) => Stage::Cs,
                    _ => continue,
                };
                let Some(h) = parse_hash(&p.hash) else { continue };
                if !want.contains_key(&(h, stage)) {
                    continue;
                }
                let Ok(insts) = rebuild_shader::gfx10::decode(&p.native.patched_code()) else { continue };
                let bind = lift::Bindings { hw: p.native.stage, layout: &p.native.layout, used: &p.native.used };
                let l = match &p.native.io {
                    StageIo::Ps { inputs, input_addr, .. } => {
                        lift::lift_stage(&insts, lift::Stage::Ps(lift::PsStage { inputs, input_addr: *input_addr }), Some(bind))
                    }
                    StageIo::Vs { inputs, outputs } => lift::lift_stage(&insts, lift::Stage::Vs(lift::VsStage { inputs, outputs }), Some(bind)),
                    StageIo::Cs(io) => lift::lift_stage(&insts, lift::Stage::Cs(*io), Some(bind)),
                    _ => continue,
                };
                let top_dword = l.live_in.iter().filter(|s| !bind.is_heap_base(**s)).filter_map(|s| bind.user_dword(*s)).max();
                out.lock().unwrap().insert((h, stage), Lifted { words: l.immediate_samplers, top_dword });
            });
        }
    });
    out.into_inner().unwrap()
}

fn build() {
    let dir = env_path("REBUILD_PIPELINES_IN", r"D:\victory-data\pipelines-in");
    let want = inputs(&dir);
    println!("pipelines build â€” {} translated programs in {}", want.len(), dir.display());

    let t = Instant::now();
    let words = sampler_words(&want);
    let lift_time = t.elapsed();
    println!("immediate sampler words  {} programs lifted again from the corpus in {:.0} ms (not library time)", words.len(), ms(lift_time));
    let top = |s: Stage| words.iter().filter(|(k, _)| k.1 == s).filter_map(|(_, l)| l.top_dword).max();
    println!("highest user dword read  vs {:?} · ps {:?} · cs {:?} (root constants: {USER_DATA_DWORDS} per stage)", top(Stage::Vs), top(Stage::Ps), top(Stage::Cs));

    // library build: read the DXIL, layouts from its bindings, every program's own root checked, the file
    let t_build = Instant::now();
    let items: Vec<(&(u64, Stage), &PathBuf)> = want.iter().collect();
    let read: Vec<Result<(Input, Compiled), String>> = library::parallel_map(&items, 8, |((h, s), p)| {
        let dxil = std::fs::read(p).map_err(|e| format!("read: {e}"))?;
        let c = bindings::read(&dxil)?;
        let lifted = words.get(&(*h, *s)).ok_or("not found in the corpus")?;
        let l = StageLayout::from_bindings(&c.bindings, &lifted.words)?;
        if let Some(d) = lifted.top_dword.filter(|d| u32::from(*d) >= USER_DATA_DWORDS) {
            if l.user_data {
                return Err(format!("reads user-data dword {d}, past the {USER_DATA_DWORDS} root constants"));
            }
        }
        Ok((Input { hash: *h, stage: *s, dxil, layout: l }, c))
    });
    let t_read = t_build.elapsed();
    let mut refused: BTreeMap<String, usize> = BTreeMap::new();
    let mut programs = Vec::new();
    for r in read {
        match r {
            Ok((i, _)) => programs.push(i),
            Err(e) => *refused.entry(e).or_default() += 1,
        }
    }
    // every program's own root signature (graphics ones beside an empty other stage), serialized once per
    // distinct (stage, layout)
    let t = Instant::now();
    let mut own: Vec<(Stage, StageLayout)> = programs.iter().map(|p| (p.stage, p.layout.clone())).collect();
    own.sort();
    own.dedup();
    let own_ok: Vec<Result<Vec<u8>, String>> = library::parallel_map(&own, 8, |(s, l)| {
        let d = layout::single(*s, l, USER_DATA_DWORDS);
        d.check().and_then(|_| rebuild_pipelines::d3d::serialize(&d))
    });
    let own_time = t.elapsed();
    let own_bad: Vec<_> = own.iter().zip(&own_ok).filter_map(|(o, r)| r.as_ref().err().map(|e| (o.clone(), e.clone()))).collect();
    let bad_layouts: HashSet<(Stage, StageLayout)> = own_bad.iter().map(|(o, _)| o.clone()).collect();
    let n_bad = programs.iter().filter(|p| bad_layouts.contains(&(p.stage, p.layout.clone()))).count();

    let mut by_stage: BTreeMap<&str, usize> = BTreeMap::new();
    let mut with: [usize; 3] = [0; 3];
    for p in &programs {
        *by_stage.entry(p.stage.name()).or_default() += 1;
        with[0] += usize::from(p.layout.user_data);
        with[1] += usize::from(p.layout.shadow);
        with[2] += usize::from(!p.layout.samplers.is_empty());
    }
    let n = programs.len();
    let path = library_path();
    let st = library::build(&path, USER_DATA_DWORDS, programs, 8, rebuild_pipelines::d3d::serialize).unwrap_or_else(|e| panic!("library: {e}"));
    let total = t_build.elapsed();

    println!("programs                 {n} in the library ({}) Â· {} refused", by_stage.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" Â· "), refused.values().sum::<usize>());
    for (e, c) in &refused {
        println!("  refused  {c:>5}  {e}");
    }
    println!("bindings                 user data {} Â· shadow table {} Â· immediate samplers {}", with[0], with[1], with[2]);
    println!(
        "own root signatures      {} distinct (stage, layout) serialized by the host in {:.1} ms Â· {} refused ({} programs)",
        own.len(),
        ms(own_time),
        own_bad.len(),
        n_bad
    );
    for ((s, l), e) in own_bad.iter().take(10) {
        println!("  refused  {} {:?}: {e}", s.name(), l);
    }
    println!(
        "library                  {} layouts Â· {} compute roots Â· {} vertexÃ—pixel pairs Â· {} root signatures ({} KB)",
        st.layouts,
        st.compute_roots,
        st.pairs,
        st.roots,
        st.root_bytes / 1024
    );
    println!("file                     {} ({:.1} MB; DXIL {:.1} MB)", path.display(), st.file_bytes as f64 / 1e6, st.dxil_bytes as f64 / 1e6);
    println!(
        "build time               {:.0} ms: read + bindings {:.0} ms Â· own roots {:.0} ms Â· library roots {:.0} ms Â· write {:.0} ms",
        ms(total),
        ms(t_read),
        ms(own_time),
        ms(st.serialize),
        ms(st.write)
    );
}

fn load() {
    let path = library_path();
    println!("pipelines load â€” {}", path.display());
    for verify in [false, true] {
        for threads in [1, 2, 4, 8] {
            let mut times: Vec<Duration> = (0..7).map(|_| Library::load(&path, threads, verify).expect("load").1.total).collect();
            times.sort();
            println!(
                "  threads {threads}  verify {:<5}  best {:>6.2} ms  median {:>6.2} ms  (warm file cache)",
                verify,
                ms(times[0]),
                ms(times[3])
            );
        }
    }
    let (lib, st) = Library::load(&path, 4, true).expect("load");
    let keys: Vec<(u64, Stage)> = lib.iter().map(|e| (e.hash, e.stage)).collect();
    let t = Instant::now();
    let mut found = 0usize;
    for _ in 0..100 {
        for &(h, s) in &keys {
            found += usize::from(lib.get(h, s).is_some());
        }
    }
    let per = t.elapsed().as_secs_f64() * 1e9 / found as f64;
    println!("  {} programs, {:.1} MB Â· lookup hash â†’ blobs {per:.0} ns", lib.len(), st.bytes as f64 / 1e6);
}

/// A pixel shader's inputs are all among the vertex shader's outputs (same name, index, register,
/// component type, and the components it reads); system values the rasterizer makes are skipped.
fn links(vs_out: &[Element], ps_in: &[Element]) -> bool {
    ps_in.iter().filter(|e| !matches!(e.system, 7 | 9 | 10 | 66)).all(|e| {
        vs_out.iter().any(|o| {
            o.name.eq_ignore_ascii_case(&e.name) && o.index == e.index && o.register == e.register && o.comp == e.comp && o.mask & e.mask == e.mask
        })
    })
}

#[cfg(windows)]
fn check(debug: bool) {
    use rebuild_pipelines::d3d::Gpu;
    if Path::new(LOCK).exists() {
        eprintln!("pipelines check: the measurement lock is present ({LOCK}); not touching the GPU");
        std::process::exit(3);
    }
    let path = library_path();
    let (lib, _) = Library::load(&path, 4, true).expect("load the library");
    let gpu = Gpu::new(debug).unwrap_or_else(|e| panic!("{e}"));
    println!("pipelines check â€” {} programs Â· device {} Â· debug layer {}", lib.len(), gpu.adapter, if debug { "on" } else { "off" });
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut note = |kind: &str, e: &str| *reasons.entry(format!("{kind}: {}", e.chars().take(220).collect::<String>())).or_default() += 1;

    // root signatures
    let t = Instant::now();
    let mut roots = HashMap::new();
    let (mut ok, mut bad) = (0, 0);
    for blob in lib.roots() {
        match gpu.root_signature(blob) {
            Ok(r) => {
                ok += 1;
                roots.insert(blob.as_ptr() as usize, r);
            }
            Err(e) => {
                bad += 1;
                note("root", &e);
            }
        }
    }
    for m in gpu.messages() {
        note("debug layer, accepted root", &m);
    }
    println!("root signatures   {ok} accepted Â· {bad} refused Â· {:.1} ms ({:.1} Âµs each)", ms(t.elapsed()), t.elapsed().as_secs_f64() * 1e6 / (ok + bad).max(1) as f64);
    let root_of = |b: &[u8]| roots.get(&(b.as_ptr() as usize));

    // compute pipelines
    let t = Instant::now();
    let (mut ok, mut bad) = (0, 0);
    let mut keep = Vec::new();
    for e in lib.iter().filter(|e| e.stage == Stage::Cs) {
        let Some(root) = e.root.and_then(root_of) else {
            bad += 1;
            note("compute", "its root signature was refused");
            continue;
        };
        match gpu.compute(root, e.dxil) {
            Ok(p) => {
                ok += 1;
                keep.push(p);
            }
            Err(er) => {
                bad += 1;
                note("compute", &er);
            }
        }
    }
    for m in gpu.messages() {
        note("debug layer, accepted compute", &m);
    }
    let n = (ok + bad).max(1);
    println!("compute pipelines {ok} created Â· {bad} refused Â· {:.1} ms ({:.2} ms each)", ms(t.elapsed()), ms(t.elapsed()) / n as f64);

    // graphics pipelines: each pixel shader with the first vertex shader whose outputs link
    let sig = |e: &library::Entry| bindings::read(e.dxil).expect("library DXIL reads");
    let vss: Vec<(library::Entry, Compiled)> = lib.iter().filter(|e| e.stage == Stage::Vs).map(|e| (e, sig(&e))).collect();
    let (mut ok, mut bad, mut unpaired) = (0, 0, 0);
    let mut created = Duration::ZERO;
    for ps in lib.iter().filter(|e| e.stage == Stage::Ps) {
        let pc = sig(&ps);
        let Some((vs, vc)) = vss.iter().find(|(_, vc)| links(&vc.outputs, &pc.inputs)) else {
            unpaired += 1;
            continue;
        };
        let Some(root) = lib.graphics_root(vs, &ps).and_then(root_of) else {
            bad += 1;
            note("graphics", "no root signature for the pair");
            continue;
        };
        let t = Instant::now();
        let r = gpu.graphics(root, vs.dxil, &vc.inputs, ps.dxil, &pc.outputs);
        created += t.elapsed();
        match r {
            Ok(p) => {
                ok += 1;
                keep.push(p);
            }
            Err(er) => {
                bad += 1;
                note("graphics", &er);
            }
        }
    }
    let n = (ok + bad).max(1);
    for m in gpu.messages() {
        note("debug layer, accepted object", &m);
    }
    println!(
        "graphics pipelines {ok} created Â· {bad} refused Â· {unpaired} pixel shaders with no linking vertex shader in the set Â· {:.1} ms ({:.2} ms each)",
        ms(created),
        ms(created) / n as f64
    );
    if !reasons.is_empty() {
        println!("\n  refusals by reason");
        for (r, c) in &reasons {
            println!("  {c:>5}  {r}");
        }
    }
}

#[cfg(not(windows))]
fn check(_: bool) {
    eprintln!("pipelines check needs Windows");
}
