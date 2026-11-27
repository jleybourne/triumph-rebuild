//! Build 3: map the title's images and bind every import through the real resolver, without running
//! a single instruction of title code.
//!
//! REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin maptitle
//!
//! Every image is parsed, placed at its preferred base (or the next free 64 KiB slot high in the
//! address space), registered as a title module, then mapped: sections copied, relocations applied,
//! imports bound — the title's own DLLs first, then our runtime, then the host. The report is the
//! measured state of Build 3: how much of the title binds, and the discovery list for the rest. The
//! game folder is only read.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rebuild_loader::map::MapOptions;
use rebuild_loader::region::Region;
use rebuild_loader::{map_into, Image, Resolver, Symbol};
use rebuild_runtime::host::{Host, HostResolver};
use rebuild_runtime::modules::register_title;
use rebuild_runtime::Tier;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if matches!(p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("exe" | "dll")) {
                out.push(p);
            }
        }
    }
}

struct Title {
    name: String,
    base: u64,
    names: HashMap<String, u32>,
    ordinals: HashMap<u16, u32>,
}

/// The title's own DLLs first; then ours and the host's (`HostResolver`).
struct Combined<'a> {
    title: &'a HashMap<String, Title>,
    host: HostResolver,
    from_title: usize,
    from_host_or_ours: usize,
}

impl Resolver for Combined<'_> {
    fn resolve(&mut self, dll: &str, symbol: &Symbol) -> Option<u64> {
        if let Some(t) = self.title.get(&dll.to_ascii_lowercase()) {
            let rva = match symbol {
                Symbol::Name { name, .. } => t.names.get(name).copied(),
                Symbol::Ordinal(o) => t.ordinals.get(o).copied(),
            };
            if let Some(rva) = rva {
                self.from_title += 1;
                return Some(t.base + rva as u64);
            }
        }
        let r = self.host.resolve(dll, symbol);
        if r.is_some() {
            self.from_host_or_ours += 1;
        }
        r
    }
}

fn main() {
    let Some(dir) = std::env::var_os("REBUILD_GAME_DIR").map(PathBuf::from) else {
        println!("FAILED    set REBUILD_GAME_DIR to the title's folder (it is only read)");
        std::process::exit(1);
    };
    let mut files = Vec::new();
    walk(&dir, &mut files);
    files.sort();

    // parse everything first
    let mut parsed: Vec<(String, Vec<u8>, Image)> = Vec::new();
    for f in &files {
        let Ok(data) = std::fs::read(f) else { continue };
        if let Ok(img) = Image::parse(&data) {
            let name = f.file_name().unwrap().to_string_lossy().to_ascii_lowercase();
            parsed.push((name, data, img));
        }
    }

    // place: preferred base if free, else high free slots
    let mut regions: Vec<Region> = Vec::new();
    let mut title: HashMap<String, Title> = HashMap::new();
    let mut next = 0x7E00_0000_0000u64;
    let mut moved = 0;
    for (name, _, img) in &parsed {
        let size = (img.size_of_image as usize).div_ceil(65536) * 65536;
        let region = Region::at(img.image_base, size).or_else(|| {
            moved += 1;
            loop {
                let r = Region::at(next, size);
                next += size as u64 + 0x10_0000;
                if r.is_some() {
                    break r;
                }
            }
        });
        let region = region.expect("a place for the image");
        let base = region.base();
        regions.push(region);
        let mut names = HashMap::new();
        let mut ordinals = HashMap::new();
        for e in &img.exports {
            if let Some(rva) = e.rva {
                ordinals.insert(e.ordinal as u16, rva);
                if let Some(n) = &e.name {
                    names.insert(n.clone(), rva);
                }
            }
        }
        register_title(name, base, &format!("G:\\{name}"), names.clone(), ordinals.clone(), name.ends_with(".exe"));
        title.insert(name.clone(), Title { name: name.clone(), base, names, ordinals });
    }

    // map and bind
    let mut res = Combined { title: &title, host: HostResolver { host: Host::default(), missing: Vec::new() }, from_title: 0, from_host_or_ours: 0 };
    let (mut relocs, mut bound, mut missing_total) = (0usize, 0usize, 0usize);
    for ((name, data, img), region) in parsed.iter().zip(regions.iter_mut()) {
        let base = region.base();
        match map_into(img, data, base, region.bytes(), &mut res, MapOptions::default()) {
            Ok(r) => {
                relocs += r.relocs_applied;
                bound += r.imports_bound + r.delay_bound;
                missing_total += r.missing.len();
            }
            Err(e) => println!("FAILED    {name}: {e:?}"),
        }
        let _ = &title[name].name;
    }

    let mut by_tier: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for (dll, _, t) in &res.host.missing {
        let tier = match t {
            Tier::Override => "override not built yet",
            Tier::ConsoleDll => "console DLL not built yet",
            Tier::ConsoleFunction => "console function not built yet",
            Tier::Host => "host",
        };
        *by_tier.entry(tier.into()).or_default().entry(dll.to_ascii_lowercase()).or_default() += 1;
    }
    let total = bound + missing_total;
    println!("maptitle — Build 3: the title's images mapped and bound, no title code run");
    println!("images    {} mapped ({} at their preferred base, {moved} moved); {relocs} relocations applied", parsed.len(), parsed.len() - moved);
    println!(
        "imports   {bound} of {total} bound ({:.1} %): {} from the title's own DLLs, {} from our runtime and the host",
        100.0 * bound as f64 / total.max(1) as f64,
        res.from_title,
        res.from_host_or_ours
    );
    let tls: Vec<String> = parsed.iter().filter(|(_, _, i)| i.tls.is_some()).map(|(n, _, i)| format!("{n} ({} callbacks)", i.tls.as_ref().map(|t| t.callbacks.len()).unwrap_or(0))).collect();
    println!("tls       {} images with static TLS: {}", tls.len(), tls.join(", "));
    println!("missing   {missing_total} (the discovery list; each is a function still to build):");
    for (tier, dlls) in &by_tier {
        let n: usize = dlls.values().sum();
        println!("  {n:>4}  {tier}: {}", dlls.iter().map(|(d, c)| format!("{d} {c}")).collect::<Vec<_>>().join(" · "));
    }
}
