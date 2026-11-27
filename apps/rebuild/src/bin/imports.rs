//! Build 3, lane loader: the import census (L3), the title's own DLLs (L4), and the parser's sweep
//! over a folder of PE files (L1).
//!
//! cargo run --release --bin imports                       # census over $REBUILD_GAME_DIR
//! cargo run --release --bin imports -- --exports          # also print every export of the title's DLLs
//! cargo run --release --bin imports -- --scan <dir> [--recursive]
//!
//! The census parses every PE image in the title's folder, maps each one at its preferred base
//! (nothing executes), and binds its imports through two resolvers: one that knows nothing yet,
//! and one that knows only the exports of the DLLs the title ships. What the second cannot bind is
//! the GDK discovery list for Build 3. The table goes to `agents/loader-imports.csv`: names only.
//!
//! The game folder is read-only: every file in it is listed (path, size, modified time) and every
//! image opened is hashed, before and after; the run fails if anything changed.

use rebuild_loader::map::{map_into, MapOptions, NullResolver, Resolver};
use rebuild_loader::pe::{Image, PeError, Symbol};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

fn walk(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut ents: Vec<_> = rd.flatten().collect();
    ents.sort_by_key(|e| e.path());
    for e in ents {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if recursive {
                walk(&e.path(), true, out);
            }
        } else if ft.is_file() {
            out.push(e.path());
        }
    }
}

fn starts_with_mz(p: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 2];
    std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut b)).is_ok() && &b == b"MZ"
}

fn fnv(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn listing(dir: &Path) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut files = Vec::new();
    walk(dir, true, &mut files);
    files
        .into_iter()
        .map(|p| {
            let m = std::fs::metadata(&p).ok();
            let len = m.as_ref().map(|m| m.len()).unwrap_or(0);
            let t = m.and_then(|m| m.modified().ok());
            (p, len, t)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// L1: parse every PE in a folder

fn scan(dir: &Path, recursive: bool) {
    let t0 = Instant::now();
    let mut files = Vec::new();
    walk(dir, recursive, &mut files);
    let (mut pe, mut ok, mut not_plus, mut unreadable) = (0usize, 0usize, 0usize, 0usize);
    let mut errors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut relocs, mut imports, mut delay, mut exports, mut tls_cb, mut pdata, mut bytes) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0u64);
    let mut mapped = 0usize;
    let mut map_err: BTreeMap<String, usize> = BTreeMap::new();
    for p in &files {
        if !starts_with_mz(p) {
            continue;
        }
        let Ok(data) = std::fs::read(p) else {
            unreadable += 1;
            continue;
        };
        pe += 1;
        bytes += data.len() as u64;
        match Image::parse(&data) {
            Ok(img) => {
                ok += 1;
                relocs += img.relocs.len();
                imports += img.imports.iter().map(|m| m.entries.len()).sum::<usize>();
                delay += img.delay_imports.iter().map(|m| m.entries.len()).sum::<usize>();
                exports += img.exports.len();
                tls_cb += img.tls.as_ref().map(|t| t.callbacks.len()).unwrap_or(0);
                pdata += img.pdata.len();
                // L2 on every image: map it 64 KiB-aligned away from its preferred base
                if (img.size_of_image as usize) <= 512 << 20 {
                    let mut mem = vec![0u8; img.size_of_image as usize];
                    let base = (img.image_base ^ 0x7f00_0000_0000) & !0xffff;
                    match map_into(&img, &data, base, &mut mem, &mut NullResolver, MapOptions::default()) {
                        Ok(_) => mapped += 1,
                        Err(e) => *map_err.entry(format!("{e:?}")).or_default() += 1,
                    }
                }
            }
            Err(PeError::NotPe32Plus(_)) => not_plus += 1,
            Err(e) => errors.entry(e.to_string()).or_default().push(p.display().to_string()),
        }
    }
    let nerr: usize = errors.values().map(|v| v.len()).sum();
    println!("imports --scan {} ({})", dir.display(), if recursive { "recursive" } else { "top level" });
    println!("files {}  ·  PE (MZ) {pe}  ·  {:.1} MB read  ·  {:.2} s", files.len(), bytes as f64 / 1e6, t0.elapsed().as_secs_f64());
    println!("parsed as PE32+   {ok}");
    println!("PE32 / other      {not_plus}   (refused by design: not PE32+)");
    println!("errors            {nerr}");
    for (e, ps) in &errors {
        println!("  {e}: {}  e.g. {}", ps.len(), ps[0]);
    }
    println!("unreadable        {unreadable}");
    println!("mapped (moved)    {mapped}");
    for (e, n) in &map_err {
        println!("  map refused {e}: {n}");
    }
    println!("totals            relocations {relocs} · imports {imports} · delay-load {delay} · exports {exports} · TLS callbacks {tls_cb} · .pdata entries {pdata}");
    println!("RESULT {ok} PE32+ images parsed, {nerr} errors, 0 panics");
}

// ---------------------------------------------------------------------------------------------
// L3 / L4: the census

struct Loaded {
    file: String,
    img: Image,
    data_hash: u64,
    data: Vec<u8>,
}

/// Resolves only against the exports of the title's own DLLs.
struct TitleDlls<'a> {
    by_name: HashMap<String, &'a Loaded>,
    hits: BTreeMap<(String, String), String>,
}

impl Resolver for TitleDlls<'_> {
    fn resolve(&mut self, dll: &str, s: &Symbol) -> Option<u64> {
        let l = self.by_name.get(&dll.to_ascii_lowercase())?;
        let e = match s {
            Symbol::Name { name, .. } => l.img.export(name),
            Symbol::Ordinal(o) => l.img.exports.iter().find(|e| e.ordinal == *o as u32),
        }?;
        self.hits.insert((dll.to_ascii_lowercase(), s.to_string()), l.file.clone());
        Some(l.img.image_base + e.rva.unwrap_or(0) as u64)
    }
}

#[derive(Default)]
struct Row {
    delay: bool,
    plain: bool,
    importers: BTreeSet<String>,
}

fn census(game: &Path, print_exports: bool) {
    let t0 = Instant::now();
    let before = listing(game);
    let mut paths = Vec::new();
    walk(game, true, &mut paths);
    let mut loaded = Vec::new();
    let mut not_parsed = Vec::new();
    for p in paths.iter().filter(|p| starts_with_mz(p)) {
        let data = std::fs::read(p).expect("read image");
        let file = p.strip_prefix(game).unwrap_or(p).display().to_string();
        match Image::parse(&data) {
            Ok(img) => loaded.push(Loaded { file, img, data_hash: fnv(&data), data }),
            Err(e) => not_parsed.push(format!("{file}: {e}")),
        }
    }

    // the census through a resolver that knows nothing yet
    let mut rows: BTreeMap<(String, String), Row> = BTreeMap::new();
    let mut null_missing = 0usize;
    let mut sizes = Vec::new();
    for l in &loaded {
        let mut mem = vec![0u8; l.img.size_of_image as usize];
        let r = map_into(&l.img, &l.data, l.img.image_base, &mut mem, &mut NullResolver, MapOptions::default())
            .unwrap_or_else(|e| panic!("{}: {e}", l.file));
        null_missing += r.missing.len();
        sizes.push(r.missing.len());
        for m in r.missing {
            let row = rows.entry((m.dll.to_ascii_lowercase(), m.symbol.to_string())).or_default();
            if m.delay {
                row.delay = true
            } else {
                row.plain = true
            }
            row.importers.insert(l.file.clone());
        }
    }

    // and through the title's own DLLs
    let mut td = TitleDlls {
        by_name: loaded
            .iter()
            .filter(|l| l.img.is_dll())
            .map(|l| (Path::new(&l.file).file_name().unwrap().to_string_lossy().to_ascii_lowercase(), l))
            .collect(),
        hits: BTreeMap::new(),
    };
    let mut after: Vec<(String, usize, usize)> = Vec::new();
    for l in &loaded {
        let mut mem = vec![0u8; l.img.size_of_image as usize];
        let r = map_into(&l.img, &l.data, l.img.image_base, &mut mem, &mut td, MapOptions::default()).unwrap();
        after.push((l.file.clone(), r.imports_bound + r.delay_bound, r.missing.len()));
    }

    // the table, grouped by DLL
    let mut by_dll: BTreeMap<&str, (usize, usize, BTreeSet<&str>)> = BTreeMap::new();
    for ((dll, _), row) in &rows {
        let k = &mut by_dll.entry(dll.as_str()).or_default();
        k.0 += 1;
        for i in &row.importers {
            k.2.insert(i.as_str());
        }
    }
    for ((dll, f), _) in &rows {
        if td.hits.contains_key(&(dll.clone(), f.clone())) {
            by_dll.get_mut(dll.as_str()).unwrap().1 += 1;
        }
    }

    println!("imports — Build 3 lane loader: the import census over the title's folder");
    println!();
    println!("images ({} parsed, {} not parsed)", loaded.len(), not_parsed.len());
    println!("  {:<38} {:>4} {:>10} {:>6} {:>8} {:>7} {:>6} {:>8} {:>5}", "file", "kind", "size", "dlls", "imports", "delay", "exports", "relocs", "tls");
    for l in &loaded {
        let i = &l.img;
        println!(
            "  {:<38} {:>4} {:>10} {:>6} {:>8} {:>7} {:>6} {:>8} {:>5}",
            l.file,
            if i.is_dll() { "dll" } else { "exe" },
            l.data.len(),
            i.imports.len() + i.delay_imports.len(),
            i.imports.iter().map(|m| m.entries.len()).sum::<usize>(),
            i.delay_imports.iter().map(|m| m.entries.len()).sum::<usize>(),
            i.exports.len(),
            i.relocs.len(),
            i.tls.as_ref().map(|t| t.callbacks.len()).unwrap_or(0),
        );
    }
    for n in &not_parsed {
        println!("  not parsed: {n}");
    }
    println!();
    println!("imported DLLs ({}), grouped", by_dll.len());
    println!("  {:<52} {:>9} {:>11} {:>8}  importers", "dll", "functions", "title DLLs", "missing");
    for (dll, (n, hit, imp)) in &by_dll {
        println!("  {:<52} {:>9} {:>11} {:>8}  {}", dll, n, hit, n - hit, imp.iter().cloned().collect::<Vec<_>>().join(" "));
    }
    let distinct = rows.len();
    let from_title = td.hits.len();
    let gdk = distinct - from_title;
    let gdk_dlls = by_dll.values().filter(|(n, hit, _)| n > hit).count();
    println!();
    println!("per image after binding to the title's DLLs (bound / missing)");
    for (f, b, m) in &after {
        println!("  {f:<38} {b:>6} / {m}");
    }

    // L4: the title's own DLLs and their exports
    println!();
    println!("the title's own DLLs: exports (named / ordinal-only / forwarded), and how many of them the title's images import statically");
    for l in loaded.iter().filter(|l| l.img.is_dll()) {
        let name = Path::new(&l.file).file_name().unwrap().to_string_lossy().to_ascii_lowercase();
        let used = td.hits.keys().filter(|(d, _)| *d == name).count();
        let named = l.img.exports.iter().filter(|e| e.name.is_some()).count();
        let fwd = l.img.exports.iter().filter(|e| e.forward.is_some()).count();
        println!(
            "  {:<38} {:>6} exports ({named} named, {} by ordinal only, {fwd} forwarded); {used} imported statically",
            l.file,
            l.img.exports.len(),
            l.img.exports.len() - named
        );
        if print_exports {
            for e in &l.img.exports {
                let n = e.name.clone().unwrap_or_else(|| format!("#{}", e.ordinal));
                let imp = if td.hits.contains_key(&(name.clone(), n.clone())) { "  (imported)" } else { "" };
                match &e.forward {
                    Some(f) => println!("      {n} -> {f}{imp}"),
                    None => println!("      {n}{imp}"),
                }
            }
        }
    }

    // the CSV: names only
    let csv_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/loader-imports.csv");
    let mut csv = String::from("dll,function,kind,importers,resolved_by\n");
    for ((dll, f), row) in &rows {
        let kind = match (row.plain, row.delay) {
            (true, true) => "both",
            (false, true) => "delay",
            _ => "import",
        };
        let by = td.hits.get(&(dll.clone(), f.clone())).map(|s| format!("title:{s}")).unwrap_or_else(|| "missing".into());
        csv.push_str(&format!("{dll},{f},{kind},{},{by}\n", row.importers.iter().cloned().collect::<Vec<_>>().join(";")));
    }
    std::fs::write(&csv_path, csv).expect("write csv");

    // the game folder is unchanged
    let after_listing = listing(game);
    let mut rehash_ok = 0;
    for l in &loaded {
        if fnv(&std::fs::read(game.join(&l.file)).expect("reread")) == l.data_hash {
            rehash_ok += 1;
        }
    }
    let unchanged = before == after_listing && rehash_ok == loaded.len();
    println!();
    println!(
        "game folder: {} files listed before and after (path, size, modified) {}; {rehash_ok}/{} images rehashed equal",
        before.len(),
        if before == after_listing { "identical" } else { "DIFFERENT" },
        loaded.len()
    );
    println!("wrote {}", csv_path.display());
    println!("{:.2} s", t0.elapsed().as_secs_f64());
    println!();
    println!(
        "RESULT {} images · {} imported DLLs · {distinct} distinct imported functions ({null_missing} import slots) missing from a resolver that knows nothing; \
         {from_title} come from the title's own DLLs; GDK discovery list: {gdk} functions in {gdk_dlls} DLLs",
        loaded.len(),
        by_dll.len()
    );
    if !unchanged {
        println!("FAILED the game folder changed during the run");
        std::process::exit(1);
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if let Some(i) = a.iter().position(|x| x == "--scan") {
        let dir = a.get(i + 1).expect("--scan <dir>");
        scan(Path::new(dir), a.iter().any(|x| x == "--recursive"));
        return;
    }
    let Ok(game) = std::env::var("REBUILD_GAME_DIR") else {
        println!("set REBUILD_GAME_DIR to the title's folder (read-only; nothing is written there)");
        std::process::exit(2);
    };
    census(Path::new(&game), a.iter().any(|x| x == "--exports"));
}
