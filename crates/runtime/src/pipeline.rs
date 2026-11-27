//! The title's pipelines: device 0x58 `CreateComputePipelineState(*desc, iid, *out)`, 0x1f0
//! `CreateComputePipelineStateX(*desc, ext count, *exts, iid, *out)`, 0x50 `CreateGraphicsPipelineState(*desc,
//! iid, *out)` and 0x2b0 `CreatePipelineState(*{size, *stream}, iid, *out)`. Graphics descriptions and streams
//! become a `Desc` in `rebuild_pipelines::stream` (console subobject numbering).
//!
//! A console compute program is known by its hash (the first 8 bytes of the BLAKE3 of its DXBC container, as
//! the census and the program library name it). Creation returns at once with a console object: the pool in
//! `crates/pipelines` builds the host pipeline on a worker (`agents/pipelines.md`, "For the lead"). A program
//! the library does not hold (the BVH builder's own, until translation runs on demand) still gets a valid
//! object with no host pipeline, so the title's creation path completes; the lists drop its dispatches
//! (`ListStats::skipped`) and the run's report names the missing programs. The same holds for a description
//! the stream reader refuses (mesh shaders, a console subobject of unknown size, ...): a valid object, counted
//! by reason in the report.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use rebuild_pipelines::library::Library;
use rebuild_pipelines::pool::{cpu_core_limit, default_threads, Handle, Pool};
use rebuild_pipelines::pso::{PsoCache, PsoCreator, Programs};
use rebuild_pipelines::state::{Desc, Key};
use rebuild_pipelines::stream::{self, Numbering, Parsed, Process, Refusal};
use rebuild_pipelines::Stage;
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
/// graphics pipelines created (0x50 and 0x2b0), and mesh-shader ones (no host pipeline yet)
static GRAPHICS: AtomicU64 = AtomicU64::new(0);
static MESH: AtomicU64 = AtomicU64::new(0);
/// descriptions with no host pipeline: reason -> creations
static REFUSED: Mutex<BTreeMap<&'static str, u32>> = Mutex::new(BTreeMap::new());
/// console stream subobjects skipped (known size, not read): first type -> count; each type is named once
static SKIPPED: Mutex<BTreeMap<u32, u32>> = Mutex::new(BTreeMap::new());

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
    stream::program_hash(bytecode)
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
    unsafe { hand_out(key, handle, iid, out) }
}

/// The console object for a pipeline (with its pool handle, or none), answered as `iid` into `out`.
unsafe fn hand_out(key: Key, handle: Option<Handle>, iid: *const GUID, out: *mut Raw) -> i32 {
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

/// Console subobject sizes for types from 25, as `type:bytes` pairs (the whole subobject, a multiple of 8) in
/// `REBUILD_STREAM_SUBOBJECTS` (e.g. `25:16,27:24`), until a captured stream settles them: those types are
/// skipped and counted. No sizes are known yet, so by default any console type stops the parse.
fn console_sizes() -> &'static [(u32, usize)] {
    static S: OnceLock<Vec<(u32, usize)>> = OnceLock::new();
    S.get_or_init(|| {
        let v = std::env::var("REBUILD_STREAM_SUBOBJECTS").unwrap_or_default();
        v.split(',')
            .filter_map(|p| {
                let (t, n) = p.trim().split_once(':')?;
                Some((t.trim().parse().ok()?, n.trim().parse().ok()?))
            })
            .collect()
    })
}

/// A graphics or compute description read: requested from the pool when its programs are in the library.
fn request(parsed: Result<Parsed, Refusal>) -> (Key, Option<Handle>) {
    let p = match parsed {
        Ok(p) => p,
        Err(r) => {
            if matches!(r, Refusal::Stage("mesh" | "amplification")) {
                MESH.fetch_add(1, Ordering::Relaxed);
            }
            let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
            let n = refused.entry(r.reason()).or_insert(0);
            if *n == 0 {
                eprintln!("pipeline  no host pipeline: {r} (counted from now on)");
            }
            *n += 1;
            return (Key { compute: false, shaders: [0, 0], state: 0 }, None);
        }
    };
    if p.skipped > 0 {
        let mut skipped = SKIPPED.lock().unwrap_or_else(|e| e.into_inner());
        let n = skipped.entry(p.first_skipped).or_insert(0);
        if *n == 0 {
            eprintln!("pipeline  console stream subobject {} skipped (size from REBUILD_STREAM_SUBOBJECTS)", p.first_skipped);
        }
        *n += p.skipped;
    }
    let key = Key::of(&p.desc);
    let wanted: [(u64, Stage); 2] = match &p.desc {
        Desc::Compute { cs } => [(*cs, Stage::Cs), (0, Stage::Ps)],
        Desc::Graphics { vs, ps, .. } => {
            GRAPHICS.fetch_add(1, Ordering::Relaxed);
            [(*vs, Stage::Vs), (ps.unwrap_or(0), Stage::Ps)]
        }
    };
    let pool = pool();
    let mut all = pool.is_some();
    for (h, stage) in wanted.into_iter().filter(|w| w.0 != 0) {
        if pool.and_then(|p| p.creator().programs().library().get(h, stage)).is_none() {
            all = false;
            *MISSING.lock().unwrap_or_else(|e| e.into_inner()).entry(h).or_insert(0) += 1;
        }
    }
    (key, pool.filter(|_| all).map(|pool| pool.request(key, p.desc)))
}

/// `D3D12_PIPELINE_STATE_STREAM_DESC`.
#[repr(C)]
struct StreamDesc {
    size: usize,
    stream: *const u8,
}

/// The largest stream read (the title's template is 0x2b0 bytes).
const MAX_STREAM: usize = 64 << 10;

/// 0x50 CreateGraphicsPipelineState: a `D3D12_GRAPHICS_PIPELINE_STATE_DESC`.
pub unsafe extern "system" fn CreateGraphicsPipelineState(_this: Raw, desc: *const c_void, iid: *const GUID, out: *mut Raw) -> i32 {
    if desc.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes a whole description, and what it points at is live for the call
    let d = unsafe { std::slice::from_raw_parts(desc as *const u8, stream::graphics_desc::SIZE) };
    let (key, handle) = request(stream::parse_graphics(d, &unsafe { Process::new() }));
    unsafe { hand_out(key, handle, iid, out) }
}

/// 0x2b0 CreatePipelineState: a pipeline state stream, console subobject numbering.
pub unsafe extern "system" fn CreatePipelineState(_this: Raw, desc: *const c_void, iid: *const GUID, out: *mut Raw) -> i32 {
    let Some(d) = (unsafe { (desc as *const StreamDesc).as_ref() }) else { return E_INVALIDARG };
    if d.stream.is_null() || d.size == 0 || d.size > MAX_STREAM {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `size` bytes of stream, and what it points at is live for the call
    let s = unsafe { std::slice::from_raw_parts(d.stream, d.size) };
    let (key, handle) = request(stream::parse_stream(s, &unsafe { Process::new() }, Numbering::Console, console_sizes()));
    unsafe { hand_out(key, handle, iid, out) }
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
        let programs = pool.creator().programs();
        let r = if p.key.compute {
            programs.compute(p.key.shaders[0]).ok()?.1
        } else {
            let ps = p.key.shaders[1];
            programs.graphics(p.key.shaders[0], (ps != 0).then_some(ps)).ok()?.2
        };
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
    let list = |v: Vec<String>| if v.is_empty() { "none".to_string() } else { v.join(", ") };
    let refused = list(REFUSED.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(r, n)| format!("{r} {n}")).collect());
    let skipped = list(SKIPPED.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(t, n)| format!("type {t} x{n}")).collect());
    format!(
        "pipelines {} created, {} graphics, {} mesh ({} programs not in the library: {}{}); no host pipeline: {refused}; console subobjects skipped: {skipped}; library {}",
        CREATED.load(Ordering::Relaxed),
        GRAPHICS.load(Ordering::Relaxed),
        MESH.load(Ordering::Relaxed),
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
        assert!(missing(program_hash(&c).unwrap()), "{}", report());
        assert_eq!(unsafe { release(out) }, 0);
    }

    /// Whether the run counts this program as not in the library (the report names the first few).
    fn missing(h: u64) -> bool {
        MISSING.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&h)
    }

    /// A DXBC-shaped container of `n` bytes.
    fn container(marker: u8, n: usize) -> Vec<u8> {
        let mut c = vec![marker; n];
        c[..4].copy_from_slice(b"DXBC");
        c[0x18..0x1c].copy_from_slice(&(n as u32).to_le_bytes());
        c
    }

    /// One stream subobject: the type, the payload at `at`, 8-byte aligned.
    fn sub(s: &mut Vec<u8>, t: u32, at: usize, payload: &[u8]) {
        let start = s.len();
        s.extend(t.to_le_bytes());
        s.resize(start + at, 0);
        s.extend(payload);
        s.resize((s.len() + 7) & !7, 0);
    }

    /// `D3D12_SHADER_BYTECODE` of bytes in this process.
    fn code(c: &[u8]) -> Vec<u8> {
        (c.as_ptr() as u64).to_le_bytes().into_iter().chain((c.len() as u64).to_le_bytes()).collect()
    }

    fn stream_create(s: &[u8]) -> (i32, Raw) {
        let d = StreamDesc { size: s.len(), stream: s.as_ptr() };
        let mut out: Raw = std::ptr::null_mut();
        let hr = unsafe { CreatePipelineState(std::ptr::null_mut(), (&d as *const StreamDesc).cast(), &ID3D12PipelineState::IID, &mut out) };
        (hr, out)
    }

    #[test]
    fn streams_give_the_title_pipelines_with_or_without_a_host_one() {
        // VS + PS with an input layout, in this process's memory as the title passes it
        let (vs, ps) = (container(0x51, 0x40), container(0x52, 0x40));
        let name = b"POSITION\0";
        let mut element = (name.as_ptr() as u64).to_le_bytes().to_vec();
        for v in [0u32, 6, 0, 0, 0, 0] {
            element.extend(v.to_le_bytes());
        }
        let mut input = (element.as_ptr() as u64).to_le_bytes().to_vec();
        input.extend(1u64.to_le_bytes());
        let mut s = Vec::new();
        sub(&mut s, 0, 8, &0x1234u64.to_le_bytes());
        sub(&mut s, 1, 8, &code(&vs));
        sub(&mut s, 2, 8, &code(&ps));
        sub(&mut s, 12, 8, &input);
        sub(&mut s, 14, 4, &3u32.to_le_bytes());
        let (hr, out) = stream_create(&s);
        assert_eq!(hr, S_OK);
        let p = of(out).expect("our pipeline object");
        assert_eq!((p.key.compute, p.key.shaders), (false, [program_hash(&vs).unwrap(), program_hash(&ps).unwrap()]));
        assert!(host(out).is_none(), "programs not in the library: no host pipeline");
        assert!(missing(p.key.shaders[0]) && missing(p.key.shaders[1]));
        assert_eq!(unsafe { release(out) }, 0);

        // a console mesh shader (24): a valid object, counted
        let mesh = MESH.load(Ordering::Relaxed);
        let mut m = Vec::new();
        sub(&mut m, 24, 8, &code(&vs));
        sub(&mut m, 2, 8, &code(&ps));
        let (hr, out) = stream_create(&m);
        assert_eq!(hr, S_OK);
        assert!(of(out).is_some() && host(out).is_none());
        assert!(MESH.load(Ordering::Relaxed) > mesh);
        assert_eq!(unsafe { release(out) }, 0);

        // a console type of unknown size: refused cleanly, still an object
        let mut u = Vec::new();
        sub(&mut u, 1, 8, &code(&vs));
        sub(&mut u, 40, 4, &[0; 12]);
        let (hr, out) = stream_create(&u);
        assert_eq!(hr, S_OK);
        assert!(of(out).is_some() && host(out).is_none());
        assert!(report().contains("subobject of unknown size"), "{}", report());
        assert_eq!(unsafe { release(out) }, 0);

        // a compute stream is a compute pipeline
        let cs = container(0x53, 0x20);
        let mut c = Vec::new();
        sub(&mut c, 6, 8, &code(&cs));
        let (hr, out) = stream_create(&c);
        assert_eq!(hr, S_OK);
        assert_eq!(of(out).unwrap().key, Key::of(&Desc::Compute { cs: program_hash(&cs).unwrap() }));
        assert_eq!(unsafe { release(out) }, 0);

        // no stream: refused at the call
        let d = StreamDesc { size: 0, stream: std::ptr::null() };
        let mut out: Raw = std::ptr::null_mut();
        assert_eq!(unsafe { CreatePipelineState(std::ptr::null_mut(), (&d as *const StreamDesc).cast(), &ID3D12PipelineState::IID, &mut out) }, E_INVALIDARG);
    }

    #[test]
    fn a_graphics_description_gives_the_same_key_as_its_stream() {
        use stream::graphics_desc as g;
        let (vs, ps) = (container(0x61, 0x40), container(0x62, 0x40));
        let mut d = vec![0u8; g::SIZE];
        let mut put = |at: usize, b: &[u8]| d[at..at + b.len()].copy_from_slice(b);
        put(g::VS, &code(&vs));
        put(g::PS, &code(&ps));
        // what a stream with only VS and PS gets by D3D12's defaults: one blend for all, full mask, back faces
        // culled, depth clip, triangles, one sample
        let words = |w: &[u32]| w.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let mut blend = vec![0u32, 0];
        for _ in 0..8 {
            blend.extend([0, 0, 2, 1, 1, 2, 1, 1, 4, 0xf]);
        }
        put(g::BLEND, &words(&blend));
        put(g::SAMPLE_MASK, &words(&[u32::MAX]));
        put(g::RASTER, &words(&[3, 3, 0, 0, 0, 0, 1, 0, 0, 0, 0]));
        put(g::TOPOLOGY, &words(&[3]));
        put(g::SAMPLE_DESC, &words(&[1, 0]));
        let mut out: Raw = std::ptr::null_mut();
        assert_eq!(unsafe { CreateGraphicsPipelineState(std::ptr::null_mut(), d.as_ptr().cast(), &ID3D12PipelineState::IID, &mut out) }, S_OK);
        let mut s = Vec::new();
        sub(&mut s, 1, 8, &code(&vs));
        sub(&mut s, 2, 8, &code(&ps));
        let (_, from_stream) = stream_create(&s);
        assert_eq!(of(out).unwrap().key, of(from_stream).unwrap().key);
        assert!(!of(out).unwrap().key.compute);
        assert_eq!(unsafe { release(out) }, 0);
        assert_eq!(unsafe { release(from_stream) }, 0);
        assert_eq!(unsafe { CreateGraphicsPipelineState(std::ptr::null_mut(), std::ptr::null(), &ID3D12PipelineState::IID, &mut out) }, E_INVALIDARG);
    }
}
