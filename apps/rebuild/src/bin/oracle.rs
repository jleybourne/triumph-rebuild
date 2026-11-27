//! Lane shader, S5: the oracle set — the fork's translation of every program, by hash.
//!
//! cargo run --release --bin oracle -- --cache <warm cache> [--verify N]
//!
//! The fork files a translation under a key that mixes in its own build fingerprint, so a
//! cache entry cannot be named from the container alone. So the oracle is made by the fork's
//! own offline command, one thread, into an empty cache:
//!
//!   VICTORY_CACHE_DIR=<warm cache> victory translate --warm all --jobs 1 <game>\shaders.rpf
//!
//! which translates the containers one after another in file order. This binary walks the
//! archive the same way, lists the entries by creation time, and pairs them in order. The
//! pairing is accepted only if the entry count equals the containers expected and every
//! entry's stage is its container's; `--verify N` re-translates N programs one at a time, each
//! into its own cache (`REBUILD_FORK_EXE`), and compares the bytes.
//!
//! Output, outside git (game-derived bytes): `<REBUILD_ORACLE_DIR>\<hash>.dxil` (the DXIL) and
//! `<hash>.vsh` (the whole entry: DXIL plus the fork's root bindings). In git: the index
//! `agents/shader-oracle.csv` (hash, stage, present, sizes).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rebuild_shader::census;
use rebuild_shader::HwStage;

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1)).cloned()
}

/// The fork's entry stage codes (`shader_cache.rs` STAGES).
const STAGES: [HwStage; 8] =
    [HwStage::Ls, HwStage::Hs, HwStage::Es, HwStage::Gs, HwStage::Vs, HwStage::Ps, HwStage::Ms, HwStage::Cs];

/// Stage and DXIL of a `VSH1` entry.
fn entry(bytes: &[u8]) -> Option<(HwStage, &[u8])> {
    let word = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    if bytes.get(..4)? != b"VSH1" {
        return None;
    }
    let stage = *STAGES.get(word(4)?)?;
    let len = word(8)?;
    Some((stage, bytes.get(12..12 + len)?))
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".tmp")) {
            out.push(p);
        }
    }
}

/// Entries under a cache root, oldest first.
fn entries_by_creation(root: &Path) -> Vec<PathBuf> {
    let mut list = Vec::new();
    files(root, &mut list);
    let mut timed: Vec<_> = list
        .into_iter()
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.created()).expect("creation time"), p))
        .collect();
    timed.sort();
    timed.into_iter().map(|t| t.1).collect()
}

fn stage_name(s: HwStage) -> &'static str {
    match s {
        HwStage::Vs => "vs",
        HwStage::Ps => "ps",
        HwStage::Ms => "ms",
        HwStage::Cs => "cs",
        _ => "other",
    }
}

fn main() {
    let cache = PathBuf::from(arg("--cache").expect("--cache <the warm run's cache folder>"));
    let index = arg("--index").unwrap_or_else(|| "agents/shader-oracle.csv".into());
    let out = PathBuf::from(std::env::var("REBUILD_ORACLE_DIR").expect("set REBUILD_ORACLE_DIR (outside git)"));
    let game = std::env::var("REBUILD_GAME_DIR").expect("set REBUILD_GAME_DIR");
    let rpf = Path::new(&game).join("shaders.rpf");
    let data = std::fs::read(&rpf).expect("read shaders.rpf");
    let before = rebuild_shader::blake3::hash(&data);
    let walk = census::walk(&data, 4);

    // The container the fork translates: the first VS/PS/MS/CS program it carries.
    let mut stage_of: HashMap<&str, HwStage> = HashMap::new();
    for p in &walk.programs {
        if matches!(p.native.stage, HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs) {
            stage_of.entry(p.code_hash.as_str()).or_insert(p.native.stage);
        }
    }
    // What the warm run translates, in order: each container hash once, if it has such a program.
    let mut seen = HashSet::new();
    let expected: Vec<(&str, HwStage)> = walk
        .order
        .iter()
        .filter(|(h, _)| seen.insert(h.as_str()))
        .filter_map(|(h, c)| stage_of.get(c.as_str()).map(|&s| (h.as_str(), s)))
        .collect();
    let entries = entries_by_creation(&cache);
    println!("oracle — lane shader S5");
    println!("expected     {} containers translated in file order; {} cache entries", expected.len(), entries.len());

    // Pair in order; every stage must agree.
    let mut paired: HashMap<&str, PathBuf> = HashMap::new();
    let mut stage_wrong = 0usize;
    let mut unreadable = 0usize;
    for ((hash, stage), path) in expected.iter().zip(&entries) {
        let bytes = std::fs::read(path).expect("read entry");
        match entry(&bytes) {
            Some((s, _)) if s == *stage => {
                paired.insert(hash, path.clone());
            }
            Some(_) => stage_wrong += 1,
            None => unreadable += 1,
        }
    }
    let aligned = entries.len() == expected.len() && stage_wrong == 0 && unreadable == 0;
    println!(
        "pairing      {} paired · {stage_wrong} stage disagreements · {unreadable} unreadable entries — {}",
        paired.len(),
        if aligned { "accepted" } else { "REFUSED" }
    );
    if !aligned {
        std::process::exit(1);
    }

    // Spot check: N programs re-translated alone, each into its own cache.
    let verify: usize = arg("--verify").and_then(|v| v.parse().ok()).unwrap_or(0);
    if verify > 0 {
        let exe = std::env::var("REBUILD_FORK_EXE").expect("--verify needs REBUILD_FORK_EXE");
        let scratch = cache.with_file_name("verify");
        let (mut same, mut checked) = (0, 0);
        let step = (expected.len() / verify).max(1);
        for (k, (hash, _)) in expected.iter().step_by(step).take(verify).enumerate() {
            let dir = scratch.join(k.to_string());
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let list = dir.join("hash.txt");
            std::fs::write(&list, hash).unwrap();
            let status = std::process::Command::new(&exe)
                .args(["translate", "--warm"])
                .arg(&list)
                .args(["--jobs", "1"])
                .arg(&rpf)
                .env("VICTORY_CACHE_DIR", dir.join("cache"))
                .stdout(std::process::Stdio::null())
                .status()
                .expect("run the fork");
            let alone = entries_by_creation(&dir.join("cache"));
            checked += 1;
            if status.success()
                && alone.len() == 1
                && std::fs::read(&alone[0]).ok() == std::fs::read(&paired[hash]).ok()
            {
                same += 1;
            } else {
                println!("  verify     {hash}: differs");
            }
        }
        println!("verify       {same} of {checked} programs re-translated alone are byte-identical to their pairing");
    }

    // The oracle files and the index, for the 25,694 VS/PS/MS/CS programs.
    std::fs::create_dir_all(&out).expect("create the oracle folder");
    let mut csv = String::from("hash,stage,present,dxil_bytes,entry_bytes\n");
    let (mut present, mut missing) = (0usize, Vec::new());
    for p in &walk.programs {
        if !matches!(p.native.stage, HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs) {
            continue;
        }
        match paired.get(p.hash.as_str()) {
            Some(path) => {
                let bytes = std::fs::read(path).unwrap();
                let (_, dxil) = entry(&bytes).unwrap();
                std::fs::write(out.join(format!("{}.dxil", p.hash)), dxil).unwrap();
                std::fs::write(out.join(format!("{}.vsh", p.hash)), &bytes).unwrap();
                csv.push_str(&format!("{},{},1,{},{}\n", p.hash, stage_name(p.native.stage), dxil.len(), bytes.len()));
                present += 1;
            }
            None => {
                csv.push_str(&format!("{},{},0,0,0\n", p.hash, stage_name(p.native.stage)));
                missing.push(p.hash.clone());
            }
        }
    }
    std::fs::write(&index, csv).expect("write the index");
    println!("oracle       {present} present · {} missing of {}", missing.len(), present + missing.len());
    for h in &missing {
        println!("  missing    {h}");
    }
    let after = rebuild_shader::blake3::hash(&std::fs::read(&rpf).unwrap());
    println!(
        "read-only    shaders.rpf {}",
        if before == after { "unchanged" } else { "CHANGED" }
    );
    println!("wrote        {index}; oracle files in {}", out.display());
}
