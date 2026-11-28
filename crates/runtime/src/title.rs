//! Loading the title: every image prepared (`rebuild_loader::prepare`), loaded by the Windows loader
//! (mapping, relocation, static TLS, unwind tables), registered as a title module, and its imports bound
//! into its IAT: the title's own DLLs first, then our runtime, then the host. An import nothing provides
//! is bound to a generated **trap thunk** that records the call in the discovery list, instead of a null
//! that would crash. No title code runs here.

#![cfg(windows)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rebuild_loader::{Image, Resolver, Symbol};
use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_FLAGS};
use windows::Win32::System::Memory::*;

use crate::host::{Host, HostResolver};
use crate::modules::register_title;

/// One loaded image.
pub struct Loaded {
    pub name: String,
    pub base: u64,
    pub image: Image,
}

pub struct LoadedTitle {
    pub images: Vec<Loaded>,
    pub bound: usize,
    pub from_title: usize,
    pub trapped: Vec<(String, String)>,
}

struct Exports {
    base: u64,
    names: HashMap<String, u32>,
    ordinals: HashMap<u16, u32>,
}

/// The title's own DLLs first; then ours and the host's.
pub struct Combined<'a> {
    title: &'a HashMap<String, Exports>,
    pub host: HostResolver,
    pub from_title: usize,
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
                // Bink's calls: ours first, so textures over its buffers learn of each frame (crate::bink, crate::cputex)
                if let Symbol::Name { name, .. } = symbol {
                    if let Some(ours) = crate::bink::wrap_title_export(dll, name, t.base + rva as u64) {
                        return Some(ours);
                    }
                }
                return Some(t.base + rva as u64);
            }
        }
        self.host.resolve(dll, symbol)
    }
}

// ------------------------------------------------------------------------------- trap thunks

/// What each trap index stands for.
static TRAPS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// Trap calls, in first-call order (index, count).
static HITS: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());

/// Every unbuilt import the title called, in first-call order, with call counts.
pub fn trap_hits() -> Vec<(String, String, u64)> {
    let traps = TRAPS.lock().unwrap();
    HITS.lock().unwrap().iter().map(|(i, n)| (traps[*i].0.clone(), traps[*i].1.clone(), *n)).collect()
}

/// The target of every trap thunk: rcx carries the trap index (the call's first argument is lost,
/// which is fine for a function that does nothing but report).
extern "system" fn trap(index: u64) -> u64 {
    let mut hits = HITS.lock().unwrap();
    match hits.iter_mut().find(|(i, _)| *i == index as usize) {
        Some((_, n)) => *n += 1,
        None => {
            hits.push((index as usize, 1));
            if let Some((dll, name)) = TRAPS.lock().unwrap().get(index as usize) {
                eprintln!("discovery: the title called {dll}!{name}, which is not built yet");
            }
        }
    }
    0
}

/// Generated code, one 22-byte thunk per trap: `mov rcx, index; mov rax, trap; jmp rax`.
fn make_thunks(n: usize) -> u64 {
    let size = (n * 22).max(1).div_ceil(4096) * 4096;
    let base = unsafe { VirtualAlloc(None, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as *mut u8;
    assert!(!base.is_null(), "thunk memory");
    let target = trap as *const () as usize as u64;
    for i in 0..n {
        let mut code = Vec::with_capacity(22);
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&(i as u64).to_le_bytes());
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&target.to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xE0]);
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), base.add(i * 22), 22) };
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    unsafe { VirtualProtect(base as *const c_void, size, PAGE_EXECUTE_READ, &mut old) }.expect("thunks executable");
    base as u64
}

// ------------------------------------------------------------------------------------ loading

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

fn hash16(data: &[u8]) -> String {
    // FNV-1a, 64-bit: a stable name for a prepared copy of exactly these bytes
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h = (h ^ *b as u64).wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

/// Load every image in the title's folder (read only) through the Windows loader, register it, and bind
/// its imports. `cache` holds the prepared copies (outside the game folder).
pub fn load_title(dir: &Path, cache: &Path) -> Result<LoadedTitle, String> {
    std::fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let mut files = Vec::new();
    walk(dir, &mut files);
    files.sort();

    let mut images: Vec<Loaded> = Vec::new();
    let mut exports: HashMap<String, Exports> = HashMap::new();
    for f in &files {
        let Ok(data) = std::fs::read(f) else { continue };
        let Ok(img) = Image::parse(&data) else {
            eprintln!("skipped: {} is not a PE32+ image", f.file_name().unwrap().to_string_lossy());
            continue;
        };
        let name = f.file_name().unwrap().to_string_lossy().to_ascii_lowercase();
        if crate::exports::replaces_title_dll(&name) {
            eprintln!("not mapped: {name} (served by rebuild-ffx)");
            continue;
        }
        let (bytes, _) =rebuild_loader::prepare::prepare(&data, &img)?;
        let path = cache.join(format!("{}-{}.dll", name.trim_end_matches(".exe").trim_end_matches(".dll"), hash16(&data)));
        if !path.exists() {
            std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_FLAGS(0)) }
            .map_err(|e| format!("the Windows loader refused the prepared {name}: {e}"))?;
        let base = module.0 as u64;
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
        register_title(&name, base, &format!("G:\\{name}"), names.clone(), ordinals.clone(), name.ends_with(".exe"));
        exports.insert(name.clone(), Exports { base, names, ordinals });
        images.push(Loaded { name, base, image: img });
    }

    // bind every import slot; collect misses first so the thunks can be made in one block
    let mut res = Combined { title: &exports, host: HostResolver { host: Host::default(), missing: Vec::new() }, from_title: 0 };
    let mut writes: Vec<(u64, Option<u64>, String, String)> = Vec::new();
    for l in &images {
        for m in l.image.imports.iter().chain(l.image.delay_imports.iter()) {
            for imp in &m.entries {
                let slot = l.base + imp.iat_rva as u64;
                let addr = res.resolve(&m.dll, &imp.symbol);
                writes.push((slot, addr, m.dll.to_ascii_lowercase(), imp.symbol.to_string()));
            }
        }
    }
    let misses: Vec<usize> = writes.iter().enumerate().filter(|(_, w)| w.1.is_none()).map(|(i, _)| i).collect();
    let thunks = make_thunks(misses.len());
    {
        let mut traps = TRAPS.lock().unwrap();
        traps.clear();
        for (k, &i) in misses.iter().enumerate() {
            traps.push((writes[i].2.clone(), writes[i].3.clone()));
            writes[i].1 = Some(thunks + k as u64 * 22);
        }
    }
    let mut bound = 0;
    for (slot, addr, _, _) in &writes {
        let mut old = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            VirtualProtect(*slot as *const c_void, 8, PAGE_READWRITE, &mut old).map_err(|e| format!("IAT slot {slot:#x}: {e}"))?;
            (*slot as *mut u64).write(addr.unwrap());
            let mut ignored = PAGE_PROTECTION_FLAGS(0);
            let _ = VirtualProtect(*slot as *const c_void, 8, old, &mut ignored);
        }
        bound += 1;
    }
    let trapped = TRAPS.lock().unwrap().clone();
    Ok(LoadedTitle { images, bound: bound - trapped.len(), from_title: res.from_title, trapped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trap_thunk_reports_its_import_and_returns_zero() {
        {
            let mut t = TRAPS.lock().unwrap();
            t.clear();
            t.push(("nowhere.dll".into(), "Nothing".into()));
            t.push(("console.dll".into(), "Something".into()));
        }
        let thunks = make_thunks(2);
        let f: extern "system" fn(u64, u64) -> u64 = unsafe { std::mem::transmute((thunks + 22) as usize) };
        assert_eq!(f(123, 456), 0);
        assert_eq!(f(0, 0), 0);
        let hits = trap_hits();
        assert_eq!(hits, vec![("console.dll".to_string(), "Something".to_string(), 2)]);
    }
}
