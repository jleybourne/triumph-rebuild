//! The title's pipelines: device 0x58 `CreateComputePipelineState(*desc, iid, *out)` and 0x1f0
//! `CreateComputePipelineStateX(*desc, ext count, *exts, iid, *out)` (graphics, 0x50 and 0x2b0, next).
//!
//! A console compute program is known by its hash (the first 8 bytes of the BLAKE3 of its DXBC container, as
//! the census and the program library name it). Creation returns at once with a console object: the pool in
//! `crates/pipelines` builds the host pipeline on a worker (`agents/pipelines.md`, "For the lead"). A program
//! the library does not hold (the BVH builder's own, until translation runs on demand) still gets a valid
//! object with no host pipeline, so the title's creation path completes; the lists drop its dispatches
//! (`ListStats::skipped`) and the run's report names the missing programs.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use rebuild_pipelines::library::Library;
use rebuild_pipelines::pool::{cpu_core_limit, default_threads, Handle, Pool};
use rebuild_pipelines::pso::{PsoCache, PsoCreator, Programs};
use rebuild_pipelines::state::{Desc, Key};
use rebuild_xlist::console::{HostPipeline, Raw};
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D12::{ID3D12DeviceChild, ID3D12Object, ID3D12PipelineState};

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
const DXGI_ERROR_NOT_FOUND: i32 = 0x887A_0002u32 as i32;

static POOL: OnceLock<Option<Pool<PsoCreator>>> = OnceLock::new();
static CREATED: AtomicU64 = AtomicU64::new(0);
/// programs the library does not hold: hash -> creations
static MISSING: Mutex<BTreeMap<u64, u32>> = Mutex::new(BTreeMap::new());

/// The program library: `REBUILD_PIPELINES_LIB`, else `library.bin` in `REBUILD_PIPELINES_OUT` (the
/// `pipelines build` output, default `D:\victory-data\pipelines`).
fn library_path() -> PathBuf {
    if let Some(p) = std::env::var_os("REBUILD_PIPELINES_LIB") {
        return p.into();
    }
    std::env::var_os("REBUILD_PIPELINES_OUT").map_or_else(|| PathBuf::from(r"D:\victory-data\pipelines"), PathBuf::from).join("library.bin")
}

/// The pool, made at device creation on the host device. Without a library every pipeline is missing.
pub fn start(device: &windows::Win32::Graphics::Direct3D12::ID3D12Device) {
    POOL.get_or_init(|| {
        let path = library_path();
        let lib = match Library::load(&path, 4, true) {
            Ok((lib, _)) => lib,
            Err(e) => {
                eprintln!("pipelines: no program library at {}: {e}", path.display());
                return None;
            }
        };
        let cache = match PsoCache::open(device, &rebuild_pipelines::pso::default_cache_path()) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("pipelines: no pipeline cache: {e}");
                None
            }
        };
        let threads = default_threads(cpu_core_limit());
        Some(Pool::new(PsoCreator::new(device, Programs::new(device, lib, None), cache), threads))
    });
}

fn pool() -> Option<&'static Pool<PsoCreator>> {
    POOL.get()?.as_ref()
}

/// The console pipeline object: header as every console object (+0 table, +8 bits, +0xC references), then ours.
#[repr(C)]
struct Pipeline {
    table: *const usize,
    bits: u32,
    refs: AtomicU32,
    key: Key,
    handle: Option<Handle>,
    /// the host root signature (borrowed from the program library's cache), once looked up
    root: AtomicUsize,
}

const SLOTS: usize = 16;

fn table() -> &'static [usize; SLOTS] {
    static T: OnceLock<[usize; SLOTS]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [trap as *const () as usize; SLOTS];
        t[0] = query as *const () as usize;
        t[1] = add_ref as *const () as usize;
        t[2] = release as *const () as usize;
        t[3] = get_private_data as *const () as usize;
        t[4] = accept as *const () as usize; // SetPrivateData
        t[5] = accept as *const () as usize; // SetPrivateDataInterface
        t[6] = accept as *const () as usize; // SetName
        t[7] = get_device as *const () as usize;
        t
    })
}

fn of<'a>(p: Raw) -> Option<&'a Pipeline> {
    // SAFETY: a non-null object the title holds; our table marks ours
    (!p.is_null() && unsafe { *(p as *const *const usize) } == table().as_ptr()).then(|| unsafe { &*(p as *const Pipeline) })
}

static TRAPS: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn trap(_this: Raw) -> i32 {
    if TRAPS.fetch_add(1, Ordering::Relaxed) == 0 {
        eprintln!("pipeline  a pipeline method we do not serve was called (GetCachedBlob or a console one)");
    }
    E_NOTIMPL
}

unsafe extern "system" fn query(this: Raw, iid: *const GUID, out: *mut Raw) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    const UNKNOWN: GUID = windows::core::IUnknown::IID;
    let ok = unsafe { iid.as_ref() }.is_some_and(|g| [ID3D12PipelineState::IID, ID3D12DeviceChild::IID, ID3D12Object::IID, UNKNOWN].contains(g));
    if !ok {
        unsafe { out.write(std::ptr::null_mut()) };
        return E_NOINTERFACE;
    }
    unsafe { add_ref(this) };
    unsafe { out.write(this) };
    S_OK
}

unsafe extern "system" fn add_ref(this: Raw) -> u32 {
    unsafe { &*(this as *const Pipeline) }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: Raw) -> u32 {
    let left = unsafe { &*(this as *const Pipeline) }.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: made by Box::into_raw in `create`; this was the last reference. The pool keeps the host
        // pipeline (shared by key).
        drop(unsafe { Box::from_raw(this as *mut Pipeline) });
    }
    left
}

unsafe extern "system" fn get_private_data(_this: Raw, _guid: *const GUID, size: *mut u32, _data: *mut c_void) -> i32 {
    if !size.is_null() {
        unsafe { size.write(0) };
    }
    DXGI_ERROR_NOT_FOUND
}

unsafe extern "system" fn accept(_this: Raw) -> i32 {
    S_OK
}

unsafe extern "system" fn get_device(_this: Raw, iid: *const GUID, out: *mut Raw) -> i32 {
    let Some(device) = crate::device::title_object() else { return E_INVALIDARG };
    // SAFETY: the device's slot 0 is QueryInterface
    unsafe {
        let qi: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> i32 = std::mem::transmute(*(*(device as *const *const usize)));
        qi(device, iid, out)
    }
}

/// The program hash of a console shader: the first 8 bytes (big-endian) of the BLAKE3 of its container.
pub fn program_hash(bytecode: &[u8]) -> Option<u64> {
    if bytecode.len() < 0x20 || &bytecode[..4] != b"DXBC" {
        return None;
    }
    let total = u32::from_le_bytes(bytecode[0x18..0x1c].try_into().ok()?) as usize;
    let bytes = bytecode.get(..total)?;
    Some(u64::from_be_bytes(rebuild_shader::blake3::hash(bytes).as_bytes()[..8].try_into().ok()?))
}

/// `D3D12_COMPUTE_PIPELINE_STATE_DESC`: root signature, CS bytecode, node mask, cached PSO, flags.
#[repr(C)]
struct ComputeDesc {
    root: Raw,
    cs: *const u8,
    cs_bytes: usize,
    node_mask: u32,
    cached: *const u8,
    cached_bytes: usize,
    flags: u32,
}

unsafe fn create_compute(desc: *const ComputeDesc, iid: *const GUID, out: *mut Raw) -> i32 {
    let Some(d) = (unsafe { desc.as_ref() }) else { return E_INVALIDARG };
    if d.cs.is_null() || d.cs_bytes == 0 {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `cs_bytes` of bytecode
    let Some(cs) = program_hash(unsafe { std::slice::from_raw_parts(d.cs, d.cs_bytes) }) else { return E_INVALIDARG };
    let desc = Desc::Compute { cs };
    let key = Key::of(&desc);
    let handle = match pool() {
        Some(p) if p.creator().programs().library().get(cs, rebuild_pipelines::Stage::Cs).is_some() => Some(p.request(key, desc)),
        _ => {
            *MISSING.lock().unwrap_or_else(|e| e.into_inner()).entry(cs).or_insert(0) += 1;
            None
        }
    };
    CREATED.fetch_add(1, Ordering::Relaxed);
    let p = Box::into_raw(Box::new(Pipeline { table: table().as_ptr(), bits: 0, refs: AtomicU32::new(1), key, handle, root: AtomicUsize::new(0) }));
    if out.is_null() {
        unsafe { release(p as Raw) };
        return 1; // S_FALSE: creation would succeed
    }
    let hr = unsafe { query(p as Raw, if iid.is_null() { &ID3D12PipelineState::IID } else { iid }, out) };
    unsafe { release(p as Raw) };
    hr
}

/// 0x58 CreateComputePipelineState.
pub unsafe extern "system" fn CreateComputePipelineState(_this: Raw, desc: *const c_void, iid: *const GUID, out: *mut Raw) -> i32 {
    unsafe { create_compute(desc.cast(), iid, out) }
}

/// 0x1f0 CreateComputePipelineStateX: the extensions (console options) are not needed to build the program.
pub unsafe extern "system" fn CreateComputePipelineStateX(_this: Raw, desc: *const c_void, _ext_count: u32, _exts: *const c_void, iid: *const GUID, out: *mut Raw) -> i32 {
    unsafe { create_compute(desc.cast(), iid, out) }
}

/// The host pipeline of a console one, for the lists. Waits for the pool at a pipeline's first use.
pub fn host(console: Raw) -> Option<HostPipeline> {
    let p = of(console)?;
    let h = p.handle?;
    let pool = pool()?;
    let pso = match pool.get(h) {
        Some(pso) => pso,
        None => pool.wait(h).ok()?,
    };
    let mut root = p.root.load(Ordering::Relaxed);
    if root == 0 {
        let r = pool.creator().programs().compute(p.key.shaders[0]).ok()?.1;
        // the library's cache keeps its own reference for the run
        root = r.as_raw() as usize;
        p.root.store(root, Ordering::Relaxed);
    }
    Some(HostPipeline { pso: pso.as_raw(), root_signature: root as Raw, compute: p.key.compute })
}

/// One line for the run report.
pub fn report() -> String {
    let missing = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let first: Vec<String> = missing.keys().take(6).map(|h| format!("{h:016x}")).collect();
    format!(
        "pipelines {} created ({} programs not in the library: {}{}); library {}",
        CREATED.load(Ordering::Relaxed),
        missing.len(),
        first.join(" "),
        if missing.len() > first.len() { " ..." } else { "" },
        if pool().is_some() { "loaded" } else { "absent" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_known_by_the_hash_of_its_container_only() {
        let mut c = vec![0u8; 0x40];
        c[..4].copy_from_slice(b"DXBC");
        c[0x18..0x1c].copy_from_slice(&0x20u32.to_le_bytes());
        let want = u64::from_be_bytes(rebuild_shader::blake3::hash(&c[..0x20]).as_bytes()[..8].try_into().unwrap());
        assert_eq!(program_hash(&c), Some(want), "bytes past the container's size do not count");
        assert_eq!(program_hash(b"DXIL"), None);
    }

    #[test]
    fn a_program_outside_the_library_still_gives_the_title_a_pipeline() {
        let mut c = vec![0u8; 0x20];
        c[..4].copy_from_slice(b"DXBC");
        c[0x18..0x1c].copy_from_slice(&0x20u32.to_le_bytes());
        let d = ComputeDesc { root: std::ptr::null_mut(), cs: c.as_ptr(), cs_bytes: c.len(), node_mask: 0, cached: std::ptr::null(), cached_bytes: 0, flags: 0 };
        let mut out: Raw = std::ptr::null_mut();
        assert_eq!(unsafe { CreateComputePipelineStateX(std::ptr::null_mut(), (&d as *const ComputeDesc).cast(), 0, std::ptr::null(), &ID3D12PipelineState::IID, &mut out) }, S_OK);
        assert!(of(out).is_some());
        assert!(host(out).is_none(), "no host pipeline: the lists drop its dispatches");
        assert!(report().contains(&format!("{:016x}", program_hash(&c).unwrap())));
        assert_eq!(unsafe { release(out) }, 0);
    }
}
