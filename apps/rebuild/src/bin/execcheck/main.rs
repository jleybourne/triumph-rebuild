//! Build 4: the execution check. Each translated program runs on the card beside the oracle's translation of
//! the same program (the fork's DXIL: data, not code), on the same deterministic inputs, and every output word is
//! compared. A program **counts** only when every word matches on every input set, and at least one set wrote
//! something (a run that writes nothing proves nothing: verdict `vacuous`).
//!
//! ```text
//! REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin execcheck -- [--stage cs|ps|vs|all]
//!     [--only <hash>[,<hash>...] | @<file of hashes>] [--v0] [--limit N] [--debug] [--gis] [--card] [--out <dir>]
//! ```
//!
//! - Programs: every agnostic VS/PS/CS the translator takes (`rebuild_sir::lift::lift_program`), compiled with
//!   DXC as `translate` does. Ours and the HLSL go under `--out` (default `D:\victory-data\exec`, outside git);
//!   the oracle is `REBUILD_ORACLE_DIR` (default `D:\victory-data\rebuild-oracle`): `<hash>.dxil` and `<hash>.vsh`.
//! - Inputs, built from what the program reads ([`rebuild_sir::lift::Lifted::desc_uses`] and its root layout):
//!   user data as root constants (`b0`), each descriptor table at its own place in the heap, and in each slot a
//!   view of the kind the program reads it as, holding seeded data (three input sets: small integers, floats,
//!   a mix). Textures hold floats in [0, 1); UAVs start from seeded words so a write is always visible.
//! - Outputs: every UAV word (compute); render targets and depth (pixel); every output-signature component,
//!   through stream output (vertex).
//! - Results: `<out>/exec.csv` (hash, stage, blocks, verdict, reason, detail), one row per program, written as it
//!   goes. The GPU runs in a worker process; if the device is lost the supervisor names the program (`crash`) and
//!   carries on with the next.
//!
//! Before each program the worker waits while `agents\MEASURING` exists in the lead's tree (`REBUILD_LEAD_DIR`,
//! default `D:\Developer\triumph-rebuild`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rebuild_shader::census;
use rebuild_shader::features::{self, Findings, WaveClass};
use rebuild_shader::native::{HwStage, RootParam, StageIo, UsedResource};
use rebuild_shader::signature::{self, Element};
use rebuild_sir::lift::res::{DescUse, Heap, View};

mod gpu;
mod ps;
mod run;
mod vs;

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned()
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

pub fn dxc() -> Option<PathBuf> {
    rebuild_pipelines::translate::dxc_path()
}

/// Parallel translation and DXC: logical processors − 4, at most 12, so the desktop stays responsive.
fn workers() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).saturating_sub(4).clamp(1, 12)
}

/// One translated program and what the harness needs to feed it.
pub struct Prog {
    pub hash: String,
    pub stage: &'static str,
    /// one basic block (the v0 set) or several
    pub single: bool,
    pub dxil: Vec<u8>,
    /// our HLSL (the pixel path builds its feeding vertex shader from the input struct)
    pub src: String,
    pub uses: Vec<DescUse>,
    pub immediate: Vec<[u32; 4]>,
    pub compare: u32,
    pub layout: Vec<RootParam>,
    pub used: Vec<UsedResource>,
    pub threads: [u32; 3],
    pub inputs: Vec<Element>,
    pub outputs: Vec<Element>,
}

/// Why a program was not run.
pub struct Skip {
    pub hash: String,
    pub stage: &'static str,
    pub single: bool,
    pub reason: String,
}

/// Lifts, emits and compiles (DXC only when the HLSL changed) every agnostic program of the stages asked for.
fn prepare(walk: &census::Walk, stages: &[&'static str], out: &Path, only: Option<&BTreeSet<String>>) -> (Vec<Prog>, Vec<Skip>) {
    let dxc = dxc().expect("dxc.exe (Windows SDK) or REBUILD_DXC");
    let next = AtomicUsize::new(0);
    let progs = Mutex::new(Vec::new());
    let skips = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers().min(rebuild_pipelines::translate::MAX_DXC) {
            s.spawn(|| {
                let mut f = Findings::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = walk.programs.get(i) else { break };
                    if only.is_some_and(|set| !set.contains(&p.hash)) {
                        continue;
                    }
                    let kind = match (&p.native.io, p.native.stage) {
                        (StageIo::Ps { .. }, _) => "ps",
                        (StageIo::Cs(_), _) => "cs",
                        (StageIo::Vs { .. }, HwStage::Vs) => "vs",
                        _ => continue,
                    };
                    if !stages.contains(&kind) {
                        continue;
                    }
                    let row = features::analyse(&p.native, &mut f);
                    if row.class() != WaveClass::Agnostic || row.decode_error.is_some() {
                        continue;
                    }
                    let single = row.blocks == 1;
                    if !single && flag("--v0") {
                        continue; // the straight-line set only
                    }
                    let skip = |reason: String| skips.lock().unwrap().push(Skip { hash: p.hash.clone(), stage: kind, single, reason });
                    match translate(p, kind, &dxc, out) {
                        Ok(mut prog) => {
                            prog.single = single;
                            progs.lock().unwrap().push(prog);
                        }
                        Err(e) => skip(e),
                    }
                }
            });
        }
    });
    let mut progs = progs.into_inner().unwrap();
    progs.sort_by(|a: &Prog, b: &Prog| (a.stage, &a.hash).cmp(&(b.stage, &b.hash)));
    (progs, skips.into_inner().unwrap())
}

fn translate(p: &census::Program, kind: &'static str, dxc: &Path, out: &Path) -> Result<Prog, String> {
    use rebuild_pipelines::translate as tr;
    let h = tr::lift_hlsl(&p.native, &p.isg1, &p.osg1, true).map_err(|r| format!("not translated: {}", r.reason.split(" row ").next().unwrap_or(&r.reason)))?;
    let hl = out.join(format!("{}.{kind}.hlsl", p.hash));
    let obj = out.join(format!("{}.{kind}.dxil", p.hash));
    let same = std::fs::read_to_string(&hl).is_ok_and(|old| old == h.src) && obj.exists();
    if !same {
        std::fs::write(&hl, &h.src).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&obj);
        if flag("--gis") {
            // the experiment: our HLSL with IEEE strictness (-Gis), to name what DXC's default float freedom changes
            let r = std::process::Command::new(dxc)
                .args(["-T", &format!("{}_6_6", h.stage.name()), "-Gis"])
                .args(tr::DXC_ARGS)
                .arg(&obj)
                .arg(&hl)
                .output()
                .map_err(|e| e.to_string())?;
            if !r.status.success() {
                return Err("not compiled: dxc error".into());
            }
        } else {
            tr::compile_file(dxc, h.stage, &hl, &obj).map_err(|_| "not compiled: dxc error".to_string())?;
        }
    }
    let dxil = std::fs::read(&obj).map_err(|e| e.to_string())?;
    let threads = match &p.native.io {
        StageIo::Cs(io) => io.threads,
        _ => [1, 1, 1],
    };
    // the container's own signatures (what the oracle's pipeline links by)
    let c = rebuild_shader::Container::parse(&dxil).map_err(|e| e.to_string())?;
    let csig = |f| c.part(f).map(|x| signature::parse(x.data).unwrap_or_default()).unwrap_or_default();
    let (inputs, outputs) = (csig(rebuild_shader::FourCC::ISG1), csig(rebuild_shader::FourCC::OSG1));
    drop(c);
    Ok(Prog {
        hash: p.hash.clone(),
        stage: kind,
        single: true,
        dxil,
        src: h.src,
        uses: h.uses,
        immediate: h.samplers.words,
        compare: h.samplers.compare,
        layout: p.native.layout.clone(),
        used: p.native.used.clone(),
        threads,
        inputs,
        outputs,
    })
}

/// The oracle's entry: its DXIL and the static samplers its root signature carries (`VSH1`, the fork's shader
/// cache format).
pub struct Oracle {
    pub dxil: Vec<u8>,
    /// D3D12 static sampler fields: filter, address u/v/w, bias, anisotropy, comparison, border, min/max LOD.
    pub statics: Vec<[u32; 10]>,
    /// root CBVs, SRVs, UAVs and root-indexed views the oracle binds outside the heap
    pub root_views: usize,
    /// `[WaveSize]` of the oracle (0: none)
    pub wave: u32,
}

fn oracle(dir: &Path, hash: &str) -> Result<Oracle, String> {
    let dxil = std::fs::read(dir.join(format!("{hash}.dxil"))).map_err(|_| "no oracle".to_string())?;
    let b = std::fs::read(dir.join(format!("{hash}.vsh"))).map_err(|_| "no oracle entry".to_string())?;
    if b.get(..4) != Some(b"VSH1") {
        return Err("oracle entry is no VSH1".into());
    }
    let word = |at: &mut usize| -> Result<u32, String> {
        let v = b.get(*at..*at + 4).ok_or("oracle entry truncated")?;
        *at += 4;
        Ok(u32::from_le_bytes(v.try_into().unwrap()))
    };
    let mut at = 4usize;
    let _stage = word(&mut at)?;
    // the DXIL (the same bytes as <hash>.dxil; its length need not be a multiple of 4)
    at += word(&mut at)? as usize;
    // user data (4 words each), console sampler words (4 each)
    at += 16 * word(&mut at)? as usize;
    at += 16 * word(&mut at)? as usize;
    let mut statics = Vec::new();
    for _ in 0..word(&mut at)? {
        let mut s = [0u32; 10];
        for v in &mut s {
            *v = word(&mut at)?;
        }
        statics.push(s);
    }
    // root CBVs, SRVs, UAVs (byte lists), then root-indexed views
    let mut root_views = 0;
    for _ in 0..3 {
        let n = word(&mut at)? as usize;
        root_views += n;
        at += n;
    }
    root_views += word(&mut at)? as usize;
    // the wave size the oracle demands ([WaveSize]: PSV0 runtime info, minimum expected lane count)
    let wave = rebuild_shader::Container::parse(&dxil)
        .ok()
        .and_then(|c| c.part(rebuild_shader::FourCC::PSV0).and_then(|p| p.data.get(20..24).map(|w| u32::from_le_bytes(w.try_into().unwrap()))))
        .unwrap_or(0);
    Ok(Oracle { dxil, statics, root_views, wave })
}

/// The result of one program.
pub struct Row {
    pub hash: String,
    pub stage: &'static str,
    pub single: bool,
    pub verdict: &'static str,
    pub reason: String,
    pub detail: String,
}

impl Row {
    fn csv(&self) -> String {
        let clean = |s: &str| s.replace([',', '\n', '\r'], ";");
        format!("{},{},{},{},{},{}", self.hash, self.stage, if self.single { "v0" } else { "v1" }, self.verdict, clean(&self.reason), clean(&self.detail))
    }
}

fn lead_agents() -> PathBuf {
    PathBuf::from(std::env::var("REBUILD_LEAD_DIR").unwrap_or_else(|_| r"D:\Developer\triumph-rebuild".into())).join("agents")
}

/// The real card's exclusive lock (`rebuild_pipelines::d3d::CardLock`, `agents\CARD` created new): waits while
/// another lane holds it or the lead is measuring. Dropped when the worker is done.
fn card_take() -> rebuild_pipelines::d3d::CardLock {
    let mut said = false;
    loop {
        match rebuild_pipelines::d3d::CardLock::take() {
            Ok(l) => return l,
            Err(e) => {
                if !said {
                    eprintln!("execcheck: waiting for the card: {e}");
                    said = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    }
}

/// A worker that died holding the card leaves its lock behind (no drop ran): removed when it names that worker
/// (a later `take` would also find it stale; this frees it at once).
fn card_release_dead(pid: u32) {
    rebuild_cardlock::CardLock::release_dead(&rebuild_cardlock::agents_dir(), pid);
}

fn measuring_wait() {
    let lock = lead_agents().join("MEASURING");
    let mut said = false;
    while lock.exists() {
        if !said {
            eprintln!("execcheck: {} exists, waiting", lock.display());
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

fn main() {
    let Ok(path) = std::env::var("REBUILD_SHADERS") else {
        eprintln!("execcheck: set REBUILD_SHADERS to a copy of shaders.rpf (never the game folder)");
        std::process::exit(2);
    };
    let out = PathBuf::from(arg("--out").unwrap_or_else(|| r"D:\victory-data\exec".into()));
    std::fs::create_dir_all(&out).expect("output folder");
    let odir = PathBuf::from(std::env::var("REBUILD_ORACLE_DIR").unwrap_or_else(|_| r"D:\victory-data\rebuild-oracle".into()));
    let stages: Vec<&'static str> = match arg("--stage").as_deref() {
        Some("ps") => vec!["ps"],
        Some("vs") => vec!["vs"],
        Some("all") => vec!["cs", "ps", "vs"],
        _ => vec!["cs"],
    };
    let only: Option<BTreeSet<String>> = arg("--only").map(|v| match v.strip_prefix('@') {
        Some(f) => std::fs::read_to_string(f).unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
        None => v.split(',').map(str::to_string).collect(),
    });
    let csv = out.join("exec.csv");

    let data = std::fs::read(&path).expect("read the shaders.rpf copy");
    let walk = census::walk(&data, workers());
    let (mut progs, skips) = prepare(&walk, &stages, &out, only.as_ref());
    drop(walk);
    drop(data);
    if flag("--card") {
        // the real GPU: straight-line programs only (no loops), at most 20 unless --limit says fewer
        progs.retain(|p| p.single);
        progs.truncate(20);
    }
    if let Some(n) = arg("--limit").and_then(|v| v.parse().ok()) {
        progs.truncate(n);
    }

    if flag("--worker") {
        // run the programs not yet in the CSV, appending rows
        let done: BTreeSet<String> = std::fs::read_to_string(&csv)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split(',').next().map(str::to_string))
            .collect();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&csv).expect("csv");
        eprintln!("execcheck: running on {}", if flag("--card") { "the card (smoke set)" } else { "WARP" });
        let card = flag("--card").then(card_take);
        let mut g = gpu::Gpu::new(flag("--card"), flag("--debug")).unwrap_or_else(|e| {
            eprintln!("execcheck: {e}");
            std::process::exit(2);
        });
        // input sets that never finished, by program (the supervisor's record): skipped
        let mut hung: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for l in std::fs::read_to_string(out.join("hung.txt")).unwrap_or_default().lines() {
            let mut w = l.split(' ');
            if let (Some(h), Some(k)) = (w.next(), w.next()) {
                hung.entry(h.to_string()).or_default().extend(k.trim().parse::<u32>().ok());
            }
        }
        for p in progs.iter().filter(|p| !done.contains(&p.hash)) {
            measuring_wait();
            std::fs::write(out.join("current"), &p.hash).unwrap();
            let row = match oracle(&odir, &p.hash) {
                Err(e) => Row { hash: p.hash.clone(), stage: p.stage, single: p.single, verdict: "untested", reason: e, detail: String::new() },
                Ok(o) => {
                    let skip = hung.get(&p.hash).cloned().unwrap_or_default();
                    let r = run::program(&mut g, p, &o, &out, &skip);
                    if g.lost() {
                        // the device went with this input set: no row; the supervisor records the set (`current`)
                        // and runs the program again without it
                        eprintln!("execcheck: device lost on {}: {}", std::fs::read_to_string(out.join("current")).unwrap_or_default(), r.detail);
                        drop(card);
                        rebuild_cardlock::exit(3);
                    }
                    r
                }
            };
            writeln!(f, "{}", row.csv()).unwrap();
            f.flush().unwrap();
        }
        let _ = std::fs::remove_file(out.join("current"));
        drop(g);
        drop(card);
        return;
    }

    // supervisor: the skips first, then workers until every program has a row
    if !flag("--resume") {
        let _ = std::fs::remove_file(&csv);
        let _ = std::fs::remove_file(out.join("hung.txt"));
        let _ = std::fs::remove_file(out.join("current"));
    }
    {
        let have: BTreeSet<String> =
            std::fs::read_to_string(&csv).unwrap_or_default().lines().filter_map(|l| l.split(',').next().map(str::to_string)).collect();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&csv).expect("csv");
        for s in skips.iter().filter(|s| !have.contains(&s.hash)) {
            let r = Row { hash: s.hash.clone(), stage: s.stage, single: s.single, verdict: "untested", reason: s.reason.clone(), detail: String::new() };
            writeln!(f, "{}", r.csv()).unwrap();
        }
    }
    let exe = std::env::current_exe().unwrap();
    let mut passthrough: Vec<String> = std::env::args().skip(1).filter(|a| a != "--resume").collect();
    passthrough.push("--worker".into());
    for _attempt in 0..1000 {
        let mut child = std::process::Command::new(&exe).args(&passthrough).spawn().expect("worker");
        let st = child.wait().expect("worker");
        card_release_dead(child.id());
        if st.success() {
            break;
        }
        // the worker died on a program: name it and go on
        let cur_set = std::fs::read_to_string(out.join("current")).unwrap_or_default();
        let (cur, set) = match cur_set.split_once(' ') {
            Some((h, k)) => (h.to_string(), k.trim().parse::<u32>().ok()),
            None => (cur_set.trim().to_string(), None),
        };
        if cur.is_empty() {
            eprintln!("execcheck: worker failed with no program in hand ({st})");
            std::process::exit(1);
        }
        // an input set that never finished (a loop whose trip count comes from the data): skipped next time,
        // unless every set has
        // an input set that never finished (a loop whose trip count comes from the data) or lost the device (an
        // index from the data past every view): skipped next time, unless every set has
        if let (Some(3 | 4), Some(k)) = (st.code(), set) {
            let path = out.join("hung.txt");
            let mut h = std::fs::read_to_string(&path).unwrap_or_default();
            h.push_str(&format!("{cur} {k} {}\n", if st.code() == Some(4) { "never finished" } else { "lost the device" }));
            std::fs::write(&path, &h).unwrap();
            if h.lines().filter(|l| l.starts_with(&cur)).count() < run::SETS as usize {
                let _ = std::fs::remove_file(out.join("current"));
                continue;
            }
        }
        let have = std::fs::read_to_string(&csv).unwrap_or_default();
        if !have.lines().any(|l| l.starts_with(&cur)) {
            let p = progs.iter().find(|p| p.hash == cur);
            let r = Row {
                hash: cur.clone(),
                stage: p.map_or("?", |p| p.stage),
                single: p.is_some_and(|p| p.single),
                verdict: "crash",
                reason: if set.is_some() { "no input set completes (loops or device loss)".into() } else { "device lost".into() },
                detail: format!("{st}"),
            };
            let mut f = std::fs::OpenOptions::new().append(true).open(&csv).unwrap();
            writeln!(f, "{}", r.csv()).unwrap();
        }
        let _ = std::fs::remove_file(out.join("current"));
    }
    report(&csv);
}

/// Counts per stage and verdict, and the reasons by size.
fn report(csv: &Path) {
    let text = std::fs::read_to_string(csv).unwrap_or_default();
    let mut by: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut reasons: BTreeMap<(String, String, String), (usize, Vec<String>)> = BTreeMap::new();
    for l in text.lines() {
        let f: Vec<&str> = l.split(',').collect();
        if f.len() < 5 {
            continue;
        }
        *by.entry((f[1].into(), f[3].into())).or_default() += 1;
        if f[3] != "match" {
            let e = reasons.entry((f[1].into(), f[3].into(), f[4].into())).or_default();
            e.0 += 1;
            if e.1.len() < 3 {
                e.1.push(f[0].into());
            }
        }
    }
    println!("execcheck — ours against the oracle on the card");
    let stages: BTreeSet<&String> = by.keys().map(|k| &k.0).collect();
    for s in stages {
        let n: usize = by.iter().filter(|(k, _)| &k.0 == s).map(|(_, v)| v).sum();
        let row: Vec<String> = by.iter().filter(|(k, _)| &k.0 == s).map(|(k, v)| format!("{} {v}", k.1)).collect();
        println!("  {s}  {n} programs: {}", row.join(" · "));
    }
    let mut v: Vec<_> = reasons.into_iter().collect();
    v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    println!("\n  stage  verdict    programs  reason (examples)");
    for ((s, verdict, r), (n, ex)) in v.iter().take(60) {
        println!("  {s:<5}  {verdict:<9}  {n:>8}  {r}  ({})", ex.join(" "));
    }
}
