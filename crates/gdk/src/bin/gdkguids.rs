//! G1 evidence: the runtime GUID tables in the title's images. Every image in `$REBUILD_GAME_DIR`
//! (top level) is searched for every GUID `known` names; where two or more sit together, the table
//! they form is walked at its stride in both directions while entries look like runtime GUIDs
//! (version 4), so a GUID we cannot name shows up as UNKNOWN. Prints GUIDs and offsets only. The
//! game folder is read-only: every top-level file (the only ones this tool opens) has its (size,
//! mtime) compared before and after.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rebuild_gdk::guid::Guid;
use rebuild_gdk::known::{name, KNOWN};

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.is_file() {
            out.insert(e.path(), (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
        }
    }
    out
}

fn guid_at(data: &[u8], o: usize) -> Option<[u8; 16]> {
    data.get(o..o + 16).map(|s| s.try_into().unwrap())
}

fn main() {
    let Some(dir) = std::env::var_os("REBUILD_GAME_DIR").map(PathBuf::from) else {
        eprintln!("set REBUILD_GAME_DIR to the game folder");
        std::process::exit(2);
    };
    let before = snapshot(&dir);
    let mut images: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read the game folder")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe") || x.eq_ignore_ascii_case("dll")))
        .collect();
    images.sort();
    let mut totals = (0usize, 0usize, 0usize);
    for path in &images {
        let data = std::fs::read(path).expect("read an image");
        let mut hits: Vec<usize> = Vec::new();
        for (g, ..) in KNOWN {
            let needle = g.to_bytes();
            let mut from = 0;
            while let Some(p) = data[from..].windows(16).position(|w| w == needle) {
                hits.push(from + p);
                from += p + 1;
            }
        }
        if hits.len() < 2 {
            continue;
        }
        hits.sort();
        // The table's stride: the most common gap between neighbouring hits.
        let mut gaps: BTreeMap<usize, usize> = BTreeMap::new();
        for w in hits.windows(2) {
            *gaps.entry(w[1] - w[0]).or_default() += 1;
        }
        let stride = gaps.iter().filter(|(g, _)| **g >= 16).max_by_key(|(_, n)| **n).map_or(16, |(g, _)| *g);
        let is_entry = |o: usize| guid_at(&data, o).is_some_and(|b| Guid::is_v4(&b));
        let mut start = hits[0];
        while start >= stride && is_entry(start - stride) {
            start -= stride;
        }
        let mut end = *hits.last().unwrap();
        while is_entry(end + stride) {
            end += stride;
        }
        let file = path.file_name().unwrap().to_string_lossy();
        println!("== {file}: table at {start:#x}..{:#x}, stride {stride:#x}", end + 16);
        let mut o = start;
        while o <= end {
            if let Some(b) = guid_at(&data, o) {
                if Guid::is_v4(&b) {
                    let g = Guid::from_bytes(&b);
                    match name(&g) {
                        Some((area, role, served)) => {
                            totals.0 += 1;
                            if !served {
                                totals.2 += 1;
                            }
                            println!("  {o:#x} {g} {area} ({role}){}", if served { "" } else { " [not served]" });
                        }
                        None => {
                            totals.1 += 1;
                            println!("  {o:#x} {g} UNKNOWN");
                        }
                    }
                } else {
                    println!("  {o:#x} (not a GUID)");
                }
            }
            o += stride;
        }
    }
    let after = snapshot(&dir);
    println!(
        "known entries {}, unknown {}, known but not served {}; game folder top level: {} files, {}",
        totals.0,
        totals.1,
        totals.2,
        after.len(),
        if before == after { "unchanged" } else { "CHANGED" }
    );
    if before != after {
        let changed: Vec<_> = after.iter().filter(|(k, v)| before.get(*k) != Some(*v)).map(|(k, _)| k.file_name()).collect();
        eprintln!("changed: {changed:?}");
        std::process::exit(1);
    }
}
