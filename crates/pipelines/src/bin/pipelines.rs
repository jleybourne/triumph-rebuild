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
//! pipelines async    GPU: the same 117 compute + 1,374 graphics pipelines through the pool and the pipeline
//!                    library cache, as the title would ask for them (all requested in a burst, then each
//!                    waited for in order, as first draws would). Cache and session record in
//!                    REBUILD_PSO_CACHE_DIR (default the shared pipelines cache's bench folder).
//!                    --threads N      workers (default: logical processors less two, at most 12)
//!                    --fresh          delete the cache and the session record first
//!                    --no-cache       no pipeline library
//!                    --cold           root signatures salted so the driver's own cache misses (implies
//!                                     --no-cache): what a first run on a new driver costs
//!                    --prewarm        queue the last session's pipelines at launch
//!                    --title-delay MS the title's first pipeline comes this long after launch (fork: 1,786)
//! pipelines ondemand GPU: compute programs outside the library (an empty one), as the BVH builder's are: the
//!                    first N compute containers of the corpus (REBUILD_SHADERS) offered and requested through
//!                    the pool, translated on its workers (lift, HLSL, DXC), pipelines created; then a second
//!                    session over the same disk cache (REBUILD_PSO_CACHE_DIR\ondemand-bench, deleted first).
//!                    --count N (default 95) · --threads N. Runs only with agents\MEASURING absent, holding
//!                    agents\CARD; at most 4 DXC processes at once.
//! ```
//! The sampler words come from the translator's `<hash>.<stage>.samplers` sidecars when present; only
//! programs without one are lifted again from the corpus (`build --lift-all` lifts every program and compares).
//! Outputs stay outside git (game-derived bytes).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rebuild_pipelines::bindings::{self, Compiled};
use rebuild_pipelines::layout::{self, StageLayout, USER_DATA_DWORDS};
use rebuild_pipelines::library::{self, Input, Library};
use rebuild_pipelines::{parse_hash, sidecar, Stage};
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
        Some("async") => run_async(&args[1..]),
        Some("ondemand") => run_ondemand(&args[1..]),
        _ => {
            eprintln!("pipelines build [--lift-all] | load | check [--debug] | async [--threads N] [--fresh] [--no-cache] [--cold] [--prewarm] [--title-delay MS] | ondemand [--count N] [--threads N]");
            std::process::exit(2);
        }
    }
}

/// The translator's outputs: (hash, stage) → DXIL path.
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
    compare: u32,
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
                // this lifter does not report comparison samplers; the sidecar does
                out.lock().unwrap().insert((h, stage), Lifted { words: l.immediate_samplers, compare: 0, top_dword });
            });
        }
    });
    out.into_inner().unwrap()
}

fn build() {
    let dir = env_path("REBUILD_PIPELINES_IN", r"D:\victory-data\pipelines-in");
    let want = inputs(&dir);
    println!("pipelines build — {} translated programs in {}", want.len(), dir.display());

    // the immediate samplers: from the translator's sidecars when present, else lifted again from the archive
    let lift_all = std::env::args().any(|a| a == "--lift-all");
    let t = Instant::now();
    let mut words: HashMap<(u64, Stage), Lifted> = HashMap::new();
    let mut bad_sidecars = Vec::new();
    let items: Vec<(&(u64, Stage), &PathBuf)> = want.iter().collect();
    for ((k, _), r) in items.iter().zip(library::parallel_map(&items, 8, |(_, p)| sidecar::read_beside(p))) {
        match r {
            Ok(Some(s)) => {
                words.insert(**k, Lifted { words: s.words, compare: s.compare, top_dword: s.top_dword.map(|d| d.min(255) as u8) });
            }
            Ok(None) => {}
            Err(e) => bad_sidecars.push(e),
        }
    }
    let from_sidecars = words.len();
    let with_top = words.values().filter(|l| l.top_dword.is_some()).count();
    println!("sampler sidecars         {from_sidecars} read in {:.1} ms · {} malformed (lifted instead)", ms(t.elapsed()), bad_sidecars.len());
    for e in bad_sidecars.iter().take(5) {
        println!("  {e}");
    }
    let missing: BTreeMap<(u64, Stage), PathBuf> = want.iter().filter(|(k, _)| lift_all || !words.contains_key(k)).map(|(k, p)| (*k, p.clone())).collect();
    if !missing.is_empty() {
        let t = Instant::now();
        let lifted = sampler_words(&missing);
        println!("immediate sampler words  {} programs lifted again from the corpus in {:.0} ms (not library time)", lifted.len(), ms(t.elapsed()));
        let top = |s: Stage| lifted.iter().filter(|(k, _)| k.1 == s).filter_map(|(_, l)| l.top_dword).max();
        println!("highest user dword read  vs {:?} · ps {:?} · cs {:?} (root constants: {USER_DATA_DWORDS} per stage)", top(Stage::Vs), top(Stage::Ps), top(Stage::Cs));
        let (mut same, mut differ) = (0, Vec::new());
        for (k, l) in lifted {
            match words.get(&k) {
                Some(s) if s.words == l.words => same += 1,
                Some(_) => differ.push(k),
                None => {
                    words.insert(k, l);
                }
            }
        }
        if lift_all {
            println!("sidecar vs lifter        {same} agree · {} differ (the sidecar is used)", differ.len());
            for (h, s) in differ.iter().take(5) {
                println!("  {h:016x}.{}", s.name());
            }
        }
    } else {
        println!("immediate sampler words  all from sidecars: no corpus walk ({with_top} carry their highest user dword; the rest are not checked against the root constants)");
    }
    let compare = words.values().filter(|l| l.compare != 0).count();
    println!("comparison samplers      {compare} programs");

    // library build: read the DXIL, layouts from its bindings, every program's own root checked, the file
    let t_build = Instant::now();
    let items: Vec<(&(u64, Stage), &PathBuf)> = want.iter().collect();
    let read: Vec<Result<(Input, Compiled), String>> = library::parallel_map(&items, 8, |((h, s), p)| {
        let dxil = std::fs::read(p).map_err(|e| format!("read: {e}"))?;
        let c = bindings::read(&dxil)?;
        let lifted = words.get(&(*h, *s)).ok_or("not found in the corpus")?;
        let l = StageLayout::from_bindings(&c.bindings, &lifted.words, lifted.compare)?;
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

    println!("programs                 {n} in the library ({}) · {} refused", by_stage.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" · "), refused.values().sum::<usize>());
    for (e, c) in &refused {
        println!("  refused  {c:>5}  {e}");
    }
    println!("bindings                 user data {} · shadow table {} · immediate samplers {}", with[0], with[1], with[2]);
    println!(
        "own root signatures      {} distinct (stage, layout) serialized by the host in {:.1} ms · {} refused ({} programs)",
        own.len(),
        ms(own_time),
        own_bad.len(),
        n_bad
    );
    for ((s, l), e) in own_bad.iter().take(10) {
        println!("  refused  {} {:?}: {e}", s.name(), l);
    }
    println!(
        "library                  {} layouts · {} compute roots · {} vertex×pixel pairs · {} root signatures ({} KB)",
        st.layouts,
        st.compute_roots,
        st.pairs,
        st.roots,
        st.root_bytes / 1024
    );
    println!("file                     {} ({:.1} MB; DXIL {:.1} MB)", path.display(), st.file_bytes as f64 / 1e6, st.dxil_bytes as f64 / 1e6);
    println!(
        "build time               {:.0} ms: read + bindings {:.0} ms · own roots {:.0} ms · library roots {:.0} ms · write {:.0} ms",
        ms(total),
        ms(t_read),
        ms(own_time),
        ms(st.serialize),
        ms(st.write)
    );
}

fn load() {
    let path = library_path();
    println!("pipelines load — {}", path.display());
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
    println!("  {} programs, {:.1} MB · lookup hash → blobs {per:.0} ns", lib.len(), st.bytes as f64 / 1e6);
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
    println!("pipelines check — {} programs · device {} · debug layer {}", lib.len(), gpu.adapter, if debug { "on" } else { "off" });
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
    println!("root signatures   {ok} accepted · {bad} refused · {:.1} ms ({:.1} µs each)", ms(t.elapsed()), t.elapsed().as_secs_f64() * 1e6 / (ok + bad).max(1) as f64);
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
    println!("compute pipelines {ok} created · {bad} refused · {:.1} ms ({:.2} ms each)", ms(t.elapsed()), ms(t.elapsed()) / n as f64);

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
        "graphics pipelines {ok} created · {bad} refused · {unpaired} pixel shaders with no linking vertex shader in the set · {:.1} ms ({:.2} ms each)",
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

/// The pipelines `check` makes, as descriptions: every compute program, then each pixel shader with the
/// first vertex shader whose outputs link, in the default state `check` uses (the fork: compute first,
/// then vertex + pixel).
#[cfg(windows)]
fn workload(lib: &Library) -> Vec<rebuild_pipelines::state::Desc> {
    use rebuild_pipelines::d3d::{format, target_format};
    use rebuild_pipelines::state::{Desc, GraphicsState, InputElement};
    let mut out: Vec<Desc> = lib.iter().filter(|e| e.stage == Stage::Cs).map(|e| Desc::Compute { cs: e.hash }).collect();
    let sig = |e: &library::Entry| bindings::read(e.dxil).expect("library DXIL reads");
    let vss: Vec<(library::Entry, Compiled)> = lib.iter().filter(|e| e.stage == Stage::Vs).map(|e| (e, sig(&e))).collect();
    for ps in lib.iter().filter(|e| e.stage == Stage::Ps) {
        let pc = sig(&ps);
        let Some((vs, vc)) = vss.iter().find(|(_, vc)| links(&vc.outputs, &pc.inputs)) else { continue };
        let mut s = GraphicsState {
            input: vc
                .inputs
                .iter()
                .filter(|e| e.system == 0)
                .map(|e| InputElement {
                    name: e.name.clone(),
                    index: e.index,
                    format: format(e.comp, 8 - e.mask.leading_zeros()).0 as u32,
                    slot: 0,
                    offset: u32::MAX, // D3D12_APPEND_ALIGNED_ELEMENT
                    per_instance: false,
                    step_rate: 0,
                })
                .collect(),
            ..Default::default()
        };
        for e in &pc.outputs {
            match e.system {
                64 if e.index < 8 => {
                    let i = e.index as usize;
                    if s.targets.len() <= i {
                        s.targets.resize(i + 1, 0);
                    }
                    s.targets[i] = target_format(e.comp, 8 - e.mask.leading_zeros()).0 as u32;
                }
                65 | 67 | 68 => {
                    s.depth_format = 40; // D32_FLOAT
                    s.depth_stencil.depth = true;
                    s.depth_stencil.depth_func = 8; // ALWAYS
                }
                _ => {}
            }
        }
        out.push(Desc::Graphics { vs: vs.hash, ps: Some(ps.hash), state: Box::new(s) });
    }
    out
}

#[cfg(windows)]
fn run_async(args: &[String]) {
    use rebuild_pipelines::d3d::Gpu;
    use rebuild_pipelines::pool::{cpu_core_limit, default_threads, Pool};
    use rebuild_pipelines::pso::{library_support, Programs, PsoCache, PsoCreator};
    use rebuild_pipelines::session::{default_dir, Record};
    use rebuild_pipelines::state::{Desc, Key};
    if Path::new(LOCK).exists() {
        eprintln!("pipelines async: the measurement lock is present ({LOCK}); not touching the GPU");
        std::process::exit(3);
    }
    let flag = |f: &str| args.iter().any(|a| a == f);
    let val = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
    let cold = flag("--cold");
    let no_cache = cold || flag("--no-cache");
    let threads = val("--threads").map_or_else(|| default_threads(cpu_core_limit()), |n| n as usize);
    let delay = Duration::from_millis(val("--title-delay").unwrap_or(0));
    let dir = std::env::var_os("REBUILD_PSO_CACHE_DIR").map(PathBuf::from).unwrap_or_else(|| default_dir().join("bench"));
    let cache_path = dir.join("pso-cache.bin");
    let session_path = dir.join("session.bin");
    if flag("--fresh") {
        let _ = std::fs::remove_file(&cache_path);
        let _ = std::fs::remove_file(&session_path);
    }

    let (lib, _) = Library::load(&library_path(), 4, true).expect("load the program library (pipelines build)");
    let work = workload(&lib);
    let n_cs = work.iter().filter(|d| matches!(d, Desc::Compute { .. })).count();
    let t = Instant::now();
    let keys: Vec<Key> = work.iter().map(Key::of).collect();
    let key_time = t.elapsed();
    let gpu = Gpu::new(false).unwrap_or_else(|e| panic!("{e}"));
    let (supported, sflags) = library_support(&gpu.device);
    let salt = cold.then(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos() % 1_000_000);
    println!(
        "pipelines async — {} pipelines ({n_cs} compute, {} graphics) · device {} · pipeline libraries {} (shader cache flags {sflags:#x}) · {threads} workers{}",
        work.len(),
        work.len() - n_cs,
        gpu.adapter,
        if supported { "supported" } else { "NOT supported" },
        salt.map(|s| format!(" · cold (root signatures salted: b{s}, space900)")).unwrap_or_default()
    );

    // launch: the cache and the session record, then the pool
    let t_launch = Instant::now();
    let cache = (!no_cache).then(|| PsoCache::open(&gpu.device, &cache_path).expect("open the pipeline cache"));
    if let Some(c) = &cache {
        let r = &c.report;
        let (in_cs, in_gfx) = (keys[..n_cs].iter().filter(|k| c.contains(k)).count(), keys[n_cs..].iter().filter(|k| c.contains(k)).count());
        println!(
            "cache open               {} keys · {:.1} MB · read + check {:.1} ms · CreatePipelineLibrary {:.1} ms{}",
            r.keys,
            r.bytes as f64 / 1e6,
            ms(r.read),
            ms(r.create),
            r.refused.as_ref().map(|w| format!(" · refused: {w}")).unwrap_or_default()
        );
        println!("in the cache at start    compute {in_cs} / {n_cs} · graphics {in_gfx} / {}", work.len() - n_cs);
    }
    let (record, why) = Record::load(&session_path);
    if let Some(w) = why {
        println!("session record           {w}");
    }
    let programs = Programs::new(&gpu.device, lib, salt);
    let pool = Pool::new(PsoCreator::new(&gpu.device, programs, cache), threads);
    if flag("--prewarm") {
        for e in &record.entries {
            pool.prewarm(e.key, e.desc.clone());
        }
        println!("prewarm                  {} pipelines from the last session queued at launch", record.entries.len());
    }
    let launch = t_launch.elapsed();
    std::thread::sleep(delay);

    // the title: every pipeline requested, then each first draw waits for its pipeline
    let t = Instant::now();
    let handles: Vec<_> = work.iter().zip(&keys).map(|(d, k)| pool.request(*k, d.clone())).collect();
    let requested = t.elapsed();
    let mut failed = BTreeMap::<String, usize>::new();
    let mut first_wait = None;
    for h in &handles {
        if let Err(e) = pool.wait(*h) {
            *failed.entry(e.chars().take(160).collect()).or_default() += 1;
        }
        first_wait.get_or_insert(t.elapsed());
    }
    let wall = t.elapsed();
    let ps = pool.stats();
    let cs = pool.creator().stats();
    println!("launch                   {:.1} ms (cache open, pool start, prewarm queued) · then {:.0} ms until the title's first request", ms(launch), ms(delay));
    println!(
        "requests                 {} in {:.2} ms ({:.2} us each, returned at once) · {} deduplicated · Key::of {:.2} us each",
        ps.requests,
        ms(requested),
        ms(requested) * 1e3 / ps.requests.max(1) as f64,
        ps.deduplicated,
        ms(key_time) * 1e3 / keys.len().max(1) as f64
    );
    for (what, c) in [("compute", cs.compute), ("graphics", cs.graphics)] {
        println!(
            "{what:<9}                {} from the cache ({:.3} ms each) · {} created by the driver ({:.3} ms each, per worker) · {} cache mismatches · {} refused",
            c.from_cache,
            c.load_ms(),
            c.created,
            c.create_ms(),
            c.cache_mismatch,
            c.refused
        );
    }
    println!(
        "title's view             all ready {:.1} ms after the first request ({:.3} ms a pipeline on {threads} workers) · first draw waited {:.2} ms · blocked in wait {:.1} ms over {} waits · {} created inline",
        ms(wall),
        ms(wall) / work.len() as f64,
        ms(first_wait.unwrap_or_default()),
        ms(ps.wait_time),
        ps.waits_blocked,
        ps.inline
    );
    for (e, n) in &failed {
        println!("  failed {n:>5}  {e}");
    }
    let rec = record.merge(&pool.requested());
    let t = Instant::now();
    rec.save(&session_path).expect("save the session record");
    println!("session record           {} pipelines ({} sessions) saved in {:.1} ms · {}", rec.entries.len(), rec.sessions, ms(t.elapsed()), session_path.display());
    if let Some(c) = pool.creator().cache() {
        match c.save() {
            Ok(r) => println!(
                "cache save               {} keys · {:.1} MB · Serialize {:.1} ms · hash + write {:.1} ms · {}",
                r.keys,
                r.bytes as f64 / 1e6,
                ms(r.serialize),
                ms(r.write),
                cache_path.display()
            ),
            Err(e) => println!("cache save               failed: {e}"),
        }
        for e in c.events.lock().unwrap().iter().take(10) {
            println!("  event  {e}");
        }
    }
    drop(pool);
}

#[cfg(not(windows))]
fn run_async(_: &[String]) {
    eprintln!("pipelines async needs Windows");
}

#[cfg(not(windows))]
fn run_ondemand(_: &[String]) {
    eprintln!("pipelines ondemand needs Windows");
}

/// Programs outside the library, translated on demand: a cold session (every program through DXC) and a warm
/// one (from the disk cache), each as the title would ask: all requested at load, then waited for in order.
#[cfg(windows)]
fn run_ondemand(args: &[String]) {
    use rebuild_pipelines::d3d::Gpu;
    use rebuild_pipelines::ondemand::OnDemand;
    use rebuild_pipelines::pool::{cpu_core_limit, default_threads, Pool};
    use rebuild_pipelines::pso::{Programs, PsoCreator};
    use rebuild_pipelines::state::{Desc, Key};
    use rebuild_pipelines::translate;
    // the card only with MEASURING absent and the exclusive agents\CARD lock, released however this ends
    let _card = rebuild_pipelines::d3d::CardLock::take().unwrap_or_else(|e| {
        eprintln!("pipelines ondemand: {e}; not touching the card");
        std::process::exit(3);
    });
    let val = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<usize>().ok());
    let count = val("--count").unwrap_or(95);
    let threads = val("--threads").unwrap_or_else(|| default_threads(cpu_core_limit()));
    let rpf = std::env::var("REBUILD_SHADERS").unwrap_or_else(|_| {
        eprintln!("pipelines: set REBUILD_SHADERS to a copy of shaders.rpf (never the game folder)");
        rebuild_cardlock::exit(2); // the card lock goes too
    });
    let t = Instant::now();
    let data = std::fs::read(&rpf).expect("read the shaders.rpf copy");
    let progs = translate::compute_containers(&data, count, |_| true);
    drop(data);
    println!("pipelines ondemand — {} compute containers (the first in the corpus) found in {:.0} ms · {threads} workers", progs.len(), ms(t.elapsed()));

    let dir = std::env::var_os("REBUILD_PSO_CACHE_DIR").map(PathBuf::from).unwrap_or_else(rebuild_pipelines::session::default_dir).join("ondemand-bench");
    let _ = std::fs::remove_dir_all(&dir);
    let empty = dir.join("empty.lib");
    library::build(&empty, USER_DATA_DWORDS, Vec::new(), 1, rebuild_pipelines::d3d::serialize).expect("an empty library");
    let gpu = Gpu::new(false).unwrap_or_else(|e| panic!("{e}"));
    for session in ["cold", "warm"] {
        let lib = Library::load(&empty, 1, true).expect("load the empty library").0;
        let programs = Programs::new(&gpu.device, lib, None).with_ondemand(OnDemand::new(Some(dir.join("programs"))));
        let pool = Pool::new(PsoCreator::new(&gpu.device, programs, None), threads);
        let t = Instant::now();
        let handles: Vec<_> = progs
            .iter()
            .map(|(h, bytes)| {
                pool.creator().programs().offer_compute(*h, bytes);
                let d = Desc::Compute { cs: *h };
                pool.request(Key::of(&d), d)
            })
            .collect();
        let requested = t.elapsed();
        let mut failed = BTreeMap::<String, usize>::new();
        for h in &handles {
            if let Err(e) = pool.wait(*h) {
                let e = e.split(" (").next().unwrap_or(e);
                *failed.entry(e.chars().take(100).collect()).or_default() += 1;
            }
        }
        let wall = t.elapsed();
        let s = pool.creator().programs().ondemand().unwrap().stats();
        let c = pool.creator().stats().compute;
        let per = |d: Duration, n: u64| if n == 0 { 0.0 } else { ms(d) / n as f64 };
        let looked = s.translated + s.from_disk + s.failed;
        println!("{session}");
        println!("  title's create calls   {} in {:.3} ms ({:.2} us each: hash, offer, request)", progs.len(), ms(requested), ms(requested) * 1e3 / progs.len().max(1) as f64);
        println!(
            "  programs               {} translated (dxc) · {} from disk · {} stale · {} failed",
            s.translated, s.from_disk, s.stale, s.failed
        );
        println!(
            "  per program            parse {:.3} ms · lift + HLSL {:.3} ms · dxc {:.1} ms · disk {:.3} ms · PSO {:.2} ms (per worker, {threads} at once)",
            per(s.parse, looked),
            per(s.lift, looked),
            per(s.dxc, s.translated),
            per(s.disk, looked),
            c.create_ms()
        );
        println!("  all ready              {:.1} ms after the first request ({} pipelines)", ms(wall), c.created);
        for (e, n) in &failed {
            println!("  failed {n:>4}            {e}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
