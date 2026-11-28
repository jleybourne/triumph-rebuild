//! Textures the title's CPU writes: their host copies kept current.
//!
//! On the console a texture is its memory: the title (or a library on its threads: Bink's video decoder) writes
//! texels with the CPU and the GPU samples them. Here a texture is a host texture of its own (`d3d12x`), and nothing
//! carried the CPU's writes into it. The title's start screen went black for seconds at a time: its attract video
//! (Bink 2, three sets of Y 3840x2160 + Cr/Cb 1920x1080 R8 planes, console linear layout, in the title's physical
//! pages) was drawn full screen from textures that stayed zero.
//!
//! ```text
//! d3d12x makes a host texture        Memory::texture_made -> registered here when its console layout is linear (rows
//!                                                            a host copy takes as they lie) and nothing on the GPU
//!                                                            writes it (no target, depth or UAV flag)
//! marks: watched memory              the ring's backer    -> written(pages): textures over the pages it sends
//!        other memory (physical)     Bink's frame calls   -> frame_decoded(): textures whose sampled bytes changed
//! the title's next submission        submit::execute      -> refresh(queue, value): one host list ahead of the
//!                                                            title's: each marked texture copied from the card's
//!                                                            copy of its memory (watched: after the queue's ring
//!                                                            wait), the title's memory in place (opened heaps), or
//!                                                            its CPU bytes through an upload buffer
//! ```
//!
//! The ring copy reads what a pass at or before the one the submission waits for put on the card, so it never reads
//! ahead of the upload; a mark seen late (its pass runs after this submission) is served by the next one. Each copy
//! takes its texture from the layout it lives in (the title's initial one: no barrier of the title's names a
//! CPU-written texture) to copy-dest and back, with enhanced barriers.
//!
//! **Cost.** Nothing marked: one atomic load per submission (and one per Bink frame call). A marked texture: a GPU copy
//! of its bytes. A Bink frame: 513 eight-byte reads per registered texture outside watched memory, to skip the sets
//! the frame did not write.
//!
//! **Swizzled layouts** (and linear rows a host copy cannot take as they lie) go through the detile pass
//! (`crate::detile`): the console bytes are read on the card where they already are, detiled into staging rows, and
//! copied by footprint. Streamed textures over GPU pages the queue maps (0xf8, `crate::pagemap`) are marked when
//! their pages are mapped or unmapped. A census of these textures (memory, mode, format, how they are written, and
//! how many ended stale) is in the run report.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

/// 64 KiB: the ring's page (what the backer names).
const PAGE: u64 = 64 << 10;

/// Textures registered (of them outside watched memory), refused (not linear or no layout, rows the host cannot copy),
/// textures copied, bytes copied, refreshes put off for want of a free list, copies skipped (memory unwritten or not
/// readable), Bink frames seen.
pub static REGISTERED: AtomicU64 = AtomicU64::new(0);
pub static UNWATCHED: AtomicU64 = AtomicU64::new(0);
pub static NOT_LINEAR: AtomicU64 = AtomicU64::new(0);
pub static NOT_COPYABLE: AtomicU64 = AtomicU64::new(0);
pub static REFRESHED: AtomicU64 = AtomicU64::new(0);
pub static BYTES: AtomicU64 = AtomicU64::new(0);
pub static BUSY: AtomicU64 = AtomicU64::new(0);
pub static UNBACKED: AtomicU64 = AtomicU64::new(0);
pub static FRAMES: AtomicU64 = AtomicU64::new(0);
/// Textures registered through the detile pass, skipped (the arena's GPU-only memory), detile copies and their
/// bytes, textures put off to a later submission (the batch's staging budget), the CPU time refreshes took (ns).
pub static DETILED: AtomicU64 = AtomicU64::new(0);
pub static GPU_ONLY: AtomicU64 = AtomicU64::new(0);
pub static DETILE_COPIES: AtomicU64 = AtomicU64::new(0);
pub static DETILE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static DEFERRED: AtomicU64 = AtomicU64::new(0);
pub static REFRESH_NS: AtomicU64 = AtomicU64::new(0);
/// Refresh CPU time by phase (ns): finding marked textures (with Bink's sampling), choosing sources and plans, making
/// buffers, staging (the CPU copies) and recording.
pub static PHASE_NS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn lap(t: &mut std::time::Instant, phase: usize) {
    let now = std::time::Instant::now();
    PHASE_NS[phase].fetch_add((now - *t).as_nanos() as u64, Relaxed);
    *t = now;
}

/// Textures marked because the queue remapped their GPU pages (0xf8).
pub static REMAPPED: AtomicU64 = AtomicU64::new(0);

/// Staging a refresh's detiles may take (more waits for the next submission; one texture always goes).
const STAGING_BUDGET: u64 = 64 << 20;

/// One subresource's rows in console memory, as a host copy footprint names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rows {
    pub subresource: u32,
    /// from the texture's console address
    pub offset: u64,
    pub width: u32,
    pub height: u32,
    /// rows of elements (texel rows over the block height)
    pub rows: u32,
    /// bytes from one row of elements to the next
    pub pitch: u32,
}

/// The copies that rebuild a texture from its console bytes, and the bytes they read: every subresource of a linear
/// layout whose rows the host can copy (pitch a multiple of 256, start a multiple of 512).
pub fn linear_rows(console: &D3D12_RESOURCE_DESC) -> Result<(Vec<Rows>, u64), Refusal> {
    let c = rebuild_xg::Computer::new(&rebuild_d3d12x::format::xg_desc(console)).map_err(|_| Refusal::NoLayout)?;
    let plane = c.texture.planes.first().ok_or(Refusal::NoLayout)?;
    if c.texture.planes.len() != 1 {
        return Err(Refusal::NotLinear); // depth with stencil: rendered, never CPU-written
    }
    let l = &plane.layout;
    if l.desc.swizzle != rebuild_xg::Swizzle::LINEAR {
        return Err(Refusal::NotLinear);
    }
    if console.Dimension.0 == rebuild_d3d12x::format::DIMENSION_TEXTURE3D {
        return Err(Refusal::NotCopyable); // linear 3D keeps a chain per depth slice: no host footprint
    }
    let bpe = u64::from(l.desc.element_bytes);
    let mips = l.mips.len() as u32;
    let mut out = Vec::with_capacity((mips * l.slices) as usize);
    for s in 0..l.slices {
        for (m, mip) in l.mips.iter().enumerate() {
            let pitch = u64::from(mip.pitch) * bpe;
            let offset = plane.offset + u64::from(s) * l.slice_size + mip.offset;
            if pitch % 256 != 0 || offset % 512 != 0 || pitch > u64::from(u32::MAX) {
                return Err(Refusal::NotCopyable);
            }
            // a block-compressed mip copies in whole blocks: the footprint is in texels
            let b = l.desc.block;
            out.push(Rows { subresource: m as u32 + s * mips, offset, width: mip.width * b, height: mip.height * b, rows: mip.height, pitch: pitch as u32 });
        }
    }
    Ok((out, l.size))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NoLayout,
    NotLinear,
    NotCopyable,
}

/// Detiled textures whose footprints do not fit their host texture (left alone, never copied).
pub static MISFIT: AtomicU64 = AtomicU64::new(0);

/// Each footprint names a subresource of `host` and covers exactly its mip (rounded up to whole `block`s): a copy of
/// it is one the host takes.
fn fits(host: &D3D12_RESOURCE_DESC, footprints: &[crate::detile::Footprint], block: u32) -> bool {
    let three_d = host.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
    let mips = u32::from(host.MipLevels.max(1));
    let subresources = mips * if three_d { 1 } else { u32::from(host.DepthOrArraySize.max(1)) };
    footprints.len() as u32 == subresources
        && footprints.iter().all(|f| {
            let m = f.subresource % mips;
            let size = |n: u64| ((n >> m).max(1) as u32).next_multiple_of(block);
            let depth = if three_d { (u32::from(host.DepthOrArraySize) >> m).max(1) } else { 1 };
            f.subresource < subresources && f.width == size(host.Width) && f.height == size(u64::from(host.Height)) && f.depth == depth
        })
}

/// Where a texture's console memory is: who can write it, and what marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mem {
    /// write-watched (the title's graphics memory, the arena's CPU pieces): the ring sends what the CPU writes
    Watched,
    /// the arena's GPU-only memory: only the GPU writes it (the title's copies and draws reach the host texture)
    GpuOnly,
    /// GPU pages the title mapped pool pages at (0xf8): the CPU writes the pool's view
    PageMapped,
    /// other committed memory the CPU reads (physical-page views, the stand-in's memory)
    Cpu,
    /// none of those (yet): nothing to copy from
    Unreadable,
}

fn memory_of(address: u64, bytes: u64) -> Mem {
    if crate::memory::watched(address, bytes) {
        Mem::Watched
    } else if crate::memory::gpu_only(address) {
        Mem::GpuOnly
    } else if crate::pagemap::source(address, bytes).is_some() {
        Mem::PageMapped
    } else if readable(address, bytes) {
        Mem::Cpu
    } else {
        Mem::Unreadable
    }
}

/// One class of the census of textures that are not linear: what they are and where they live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Class {
    mem: Mem,
    swizzle: u32,
    format: i32,
    dimension: i32,
    mips: u32,
    arrays: u32,
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    textures: u64,
    bytes: u64,
    /// some byte of the first 64 KiB non-zero when the texture was made
    data_at_creation: u64,
    /// textures the ring marked after creation, and the marks
    marked: u64,
    marks: u64,
    /// textures whose sampled bytes differed when they went (or at the report) from their creation
    changed: u64,
    /// textures whose CPU bytes then differed from their last copy (or were never copied): left behind
    stale: u64,
    smallest: (u32, u32),
    largest: (u32, u32),
}

/// The census of non-linear textures (`report`, and `REBUILD_TRACE_CPU_TEXTURES` prints every class).
static CENSUS: Mutex<BTreeMap<Class, Tally>> = Mutex::new(BTreeMap::new());

fn census(class: Class, f: impl FnOnce(&mut Tally)) {
    let mut c = lock(&CENSUS);
    f(c.entry(class).or_default());
}

struct Texture {
    address: u64,
    /// console bytes the copies read
    bytes: u64,
    host: ID3D12Resource,
    format: DXGI_FORMAT,
    /// the host barrier layout it lives in between the title's lists
    layout: D3D12_BARRIER_LAYOUT,
    copy: Copy,
    /// in a region the ring watches: marked by the pages it sends; else by Bink's frames
    watched: bool,
    dirty: AtomicBool,
    /// outside watched memory: a hash of sampled bytes at the last copy
    sampled: AtomicU64,
    /// textures through the detile pass: their census class, a hash of their bytes when made (or 0: unreadable), the
    /// ring's marks since
    class: Option<Class>,
    born: u64,
    marks: AtomicU64,
    /// a detiled texture: a hash of its CPU bytes at its last copy (0: none yet, or unreadable)
    copied: AtomicU64,
}

/// How a texture's console bytes become its host subresources.
enum Copy {
    /// linear rows a host copy takes as they lie
    Rows(Vec<Rows>),
    /// through the detile pass (`crate::detile`): swizzled layouts, and linear rows off the host's copy grid. The plan
    /// (tables) is made at the first copy.
    Detile(Box<rebuild_xg::Layout>, std::sync::OnceLock<Option<Arc<crate::detile::Plan>>>),
}

impl Texture {
    fn plan(&self) -> Option<Arc<crate::detile::Plan>> {
        match &self.copy {
            Copy::Rows(_) => None,
            Copy::Detile(l, p) => p
                .get_or_init(|| {
                    let plan = crate::detile::plan(l)?;
                    let host = unsafe { self.host.GetDesc() };
                    if !fits(&host, &plan.footprints, l.desc.block) {
                        if MISFIT.fetch_add(1, Relaxed) < 4 {
                            eprintln!(
                                "cputex    texture at {:#x}: its console layout does not match its host texture ({}x{}x{}, {} mips): left alone",
                                self.address, host.Width, host.Height, host.DepthOrArraySize, host.MipLevels
                            );
                        }
                        return None;
                    }
                    Some(Arc::new(plan))
                })
                .clone(),
        }
    }

    fn subresources(&self) -> usize {
        match &self.copy {
            Copy::Rows(r) => r.len(),
            Copy::Detile(l, _) => l.mips.len() * if l.desc.dimension == rebuild_xg::Dimension::Tex3d { 1 } else { l.slices as usize },
        }
    }

    /// Where the CPU sees its bytes: its own address, or for a GPU address the title mapped pool pages at (queue
    /// 0xf8, `crate::pagemap`: Bink's frame buffers), the pool's CPU view of them.
    fn cpu(&self) -> u64 {
        if self.watched {
            return self.address;
        }
        crate::pagemap::source(self.address, self.bytes).unwrap_or(self.address)
    }
}

// SAFETY: D3D12 resources are free-threaded; the rest is plain data and atomics.
unsafe impl Send for Texture {}
unsafe impl Sync for Texture {}

#[derive(Default)]
struct Registry {
    /// by console address (several textures may share one: views of one memory in different formats)
    by_address: BTreeMap<u64, Vec<Arc<Texture>>>,
    /// the largest `bytes` registered: how far back a page lookup looks
    longest: u64,
}

static REGISTRY: RwLock<Option<Registry>> = RwLock::new(None);
/// Some texture is marked: the submission path's only check.
static DIRTY: AtomicBool = AtomicBool::new(false);
/// Bink frames already looked at by a refresh.
static FRAMES_SEEN: AtomicU64 = AtomicU64::new(0);

/// `d3d12x` made `host` for a console texture at `address` (`console`: the title's description), living in host
/// barrier layout `layout`. Registered when its layout copies as it lies and the GPU never writes it.
pub fn texture_made(address: u64, console: &D3D12_RESOURCE_DESC, host: &ID3D12Resource, layout: D3D12_BARRIER_LAYOUT) {
    if !on() || address == 0 {
        return;
    }
    // render targets, depth and UAV textures are written by the GPU, never the CPU
    if console.Flags.0 & (D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0) != 0 {
        return;
    }
    let (copy, bytes, class) = match linear_rows(console) {
        Ok((rows, bytes)) => (Copy::Rows(rows), bytes, None),
        Err(Refusal::NoLayout) => {
            NOT_LINEAR.fetch_add(1, Relaxed);
            return;
        }
        Err(why) => {
            let l = crate::detile::layout_of(console);
            let (swizzle, size) = match &l {
                Ok(l) => (l.desc.swizzle.0, l.size),
                Err(_) => (console.Layout.0 as u32 & 0xff, rebuild_d3d12x::format::footprint(console).map_or(0, |f| f.size)),
            };
            let class = Class {
                mem: memory_of(address, size.max(1)),
                swizzle,
                format: console.Format.0,
                dimension: console.Dimension.0,
                mips: rebuild_d3d12x::format::mip_count(console),
                arrays: if console.Dimension.0 == rebuild_d3d12x::format::DIMENSION_TEXTURE3D { 1 } else { u32::from(console.DepthOrArraySize) },
            };
            let size2 = (console.Width as u32, console.Height);
            census(class, |t| {
                t.textures += 1;
                t.bytes += size;
                if t.textures == 1 || size2 < t.smallest {
                    t.smallest = size2;
                }
                if size2 > t.largest {
                    t.largest = size2;
                }
            });
            match l {
                Ok(l) if detile_mode() != Detile::Off => (Copy::Detile(Box::new(l), std::sync::OnceLock::new()), size, Some(class)),
                _ => {
                    if why == Refusal::NotCopyable {
                        NOT_COPYABLE.fetch_add(1, Relaxed);
                    } else {
                        NOT_LINEAR.fetch_add(1, Relaxed);
                    }
                    return;
                }
            }
        }
    };
    let watched = crate::memory::watched(address, bytes);
    // the arena's GPU-only memory: the CPU never writes it (the title's copies and draws reach the host texture)
    if !watched && crate::memory::gpu_only(address) {
        GPU_ONLY.fetch_add(1, Relaxed);
        return;
    }
    if !watched {
        UNWATCHED.fetch_add(1, Relaxed);
    }
    let format = unsafe { host.GetDesc() }.Format;
    let mut t = Texture { address, bytes, host: host.clone(), format, layout, copy, watched, dirty: AtomicBool::new(true), sampled: AtomicU64::new(0), class, born: 0, marks: AtomicU64::new(0), copied: AtomicU64::new(0) };
    if let Some(c) = class {
        DETILED.fetch_add(1, Relaxed);
        let cpu = t.cpu();
        if readable(cpu, bytes) {
            t.born = sample(cpu, bytes) | 1;
            // SAFETY: readable (checked)
            let head = unsafe { std::slice::from_raw_parts(cpu as *const u8, bytes.min(64 << 10) as usize) };
            if head.iter().any(|&b| b != 0) {
                census(c, |t| t.data_at_creation += 1);
            }
        }
    }
    let t = Arc::new(t);
    let n = REGISTERED.fetch_add(1, Relaxed);
    if trace() || n < 4 || (!watched && UNWATCHED.load(Relaxed) <= 16) {
        eprintln!(
            "cputex    texture at {address:#x} ({}x{}x{} format {} layout {:#x}, host layout {}, {} subresources, {bytes:#x} bytes, {}{}) is CPU-written: kept current",
            console.Width,
            console.Height,
            console.DepthOrArraySize,
            console.Format.0,
            console.Layout.0,
            layout.0,
            t.subresources(),
            if watched { "watched by the ring" } else { "not watched: Bink's frames mark it" },
            if t.class.is_some() { ", detiled" } else { "" }
        );
    }
    let mut g = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    let r = g.get_or_insert_with(Registry::default);
    r.longest = r.longest.max(bytes);
    r.by_address.entry(address).or_default().push(t);
    DIRTY.store(true, Relaxed);
}

/// The texture `host` at `address` is gone.
pub fn texture_gone(address: u64, host: &ID3D12Resource) {
    let mut g = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    let Some(r) = g.as_mut() else { return };
    if let Some(v) = r.by_address.get_mut(&address) {
        v.retain(|t| {
            let gone = t.host.as_raw() == host.as_raw();
            if gone {
                settle(t);
            }
            !gone
        });
        if v.is_empty() {
            r.by_address.remove(&address);
        }
    }
}

/// A detiled texture's census at its end (or the report's): the ring's marks, and whether its bytes changed since
/// it was made.
fn settle(t: &Texture) {
    let Some(c) = t.class else { return };
    let marks = t.marks.swap(0, Relaxed);
    let cpu = t.cpu();
    let now = if readable(cpu, t.bytes) { sample(cpu, t.bytes) | 1 } else { 0 };
    let changed = t.born != 0 && now != 0 && now != t.born;
    let copied = t.copied.load(Relaxed);
    let stale = now != 0 && (copied == 0 || copied != now);
    census(c, |s| {
        s.marks += marks;
        s.marked += u64::from(marks > 0);
        s.changed += u64::from(changed);
        s.stale += u64::from(stale);
    });
}

/// The ring is about to send these 64 KiB pages (console addresses, sorted): mark the textures over them.
pub fn written(pages: &[u64]) {
    mark(pages, true);
}

/// The queue mapped or unmapped these GPU pages (0xf8, `crate::pagemap`; sorted): the textures over them read other
/// bytes now (a streamed texture made before its pages were mapped, or a slot the title streams into again).
pub fn remapped(pages: &[u64]) {
    let n = mark(pages, false);
    REMAPPED.fetch_add(n, Relaxed);
}

/// Marks the textures over `pages` (by the ring: counted for the census); how many were not marked yet.
fn mark(pages: &[u64], ring: bool) -> u64 {
    let g = REGISTRY.read().unwrap_or_else(|e| e.into_inner());
    let Some(r) = g.as_ref() else { return 0 };
    if r.by_address.is_empty() {
        return 0;
    }
    let mut n = 0;
    for &p in pages {
        for t in over(&r.by_address, r.longest, p, |t| (t.address, t.bytes)) {
            if !t.dirty.swap(true, Relaxed) {
                n += 1;
                // a detiled texture written again after a copy (the census counts these)
                if ring && t.class.is_some() {
                    t.marks.fetch_add(1, Relaxed);
                }
            }
        }
    }
    if n > 0 {
        DIRTY.store(true, Relaxed);
    }
    n
}

/// The entries of `map` (by start address; `extent` gives start and bytes, none longer than `longest`) that overlap
/// the 64 KiB page at `page`.
fn over<'a, T>(map: &'a BTreeMap<u64, Vec<T>>, longest: u64, page: u64, extent: impl Fn(&T) -> (u64, u64) + 'a) -> impl Iterator<Item = &'a T> + 'a {
    map.range(page.saturating_sub(longest)..page + PAGE).flat_map(|(_, v)| v.iter()).filter(move |t| {
        let (a, n) = extent(t);
        a < page + PAGE && page < a + n
    })
}

// ---- Bink: its frames land in memory the ring does not watch (the title's physical pages) ----

/// Bink finished writing a frame into its buffers (one of its frame calls returned, `crate::bink`): the next refresh
/// looks at the textures outside watched memory.
pub fn frame_decoded() {
    FRAMES.fetch_add(1, Relaxed);
}

/// A hash of 513 eight-byte words spread over `bytes` at `address` (readable memory): changes when a video frame
/// lands in it, the same when the frame went to another buffer set.
fn sample(address: u64, bytes: u64) -> u64 {
    let words = bytes / 8;
    if words == 0 {
        return 0;
    }
    let step = (words / 512).max(1);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < words {
        // SAFETY: inside the readable range the caller checked; aligned (textures are 512-byte aligned)
        let w = unsafe { std::ptr::read_volatile((address as *const u64).add(i as usize)) };
        h = (h ^ w).wrapping_mul(0x1000_0000_01b3);
        i += step;
    }
    let last = unsafe { std::ptr::read_volatile((address as *const u64).add(words as usize - 1)) };
    (h ^ last).wrapping_mul(0x1000_0000_01b3)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Off,
    /// watched memory copies from the card's copy the ring keeps (no CPU work)
    Ring,
    /// every copy takes the CPU bytes through an upload buffer (WARP: its copies from the reserved sub-window buffers
    /// fault)
    Upload,
}

/// `REBUILD_CPU_TEXTURES`: `0` off, `ring`, `upload`; default the ring on a card, upload on WARP.
fn mode() -> Mode {
    static M: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *M.get_or_init(|| match std::env::var("REBUILD_CPU_TEXTURES").as_deref().map(str::trim) {
        Ok("0") => Mode::Off,
        Ok("ring") => Mode::Ring,
        Ok("upload") => Mode::Upload,
        _ if std::env::var_os("REBUILD_WARP").is_some() => Mode::Upload,
        _ => Mode::Ring,
    })
}

fn on() -> bool {
    mode() != Mode::Off
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Detile {
    /// swizzled textures are counted, not copied
    Off,
    /// on the card (`crate::detile`): the bytes read where the card has them
    Gpu,
    /// on the CPU into the upload buffer (the reference path, and for measuring)
    Cpu,
}

/// `REBUILD_DETILE`: `0` off, `cpu`, else on the card.
fn detile_mode() -> Detile {
    static M: std::sync::OnceLock<Detile> = std::sync::OnceLock::new();
    *M.get_or_init(|| match std::env::var("REBUILD_DETILE").as_deref().map(str::trim) {
        Ok("0") => Detile::Off,
        Ok("cpu") => Detile::Cpu,
        _ => Detile::Gpu,
    })
}

/// The pass, made on first use (`None`: it failed; the CPU path stands in).
fn detiler(device: &ID3D12Device) -> Option<&'static crate::detile::Detiler> {
    static D: std::sync::OnceLock<Option<crate::detile::Detiler>> = std::sync::OnceLock::new();
    D.get_or_init(|| match crate::detile::Detiler::new(device) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("cputex    the detile pass failed ({e}): swizzled textures detile on the CPU");
            None
        }
    })
    .as_ref()
}

/// `REBUILD_TRACE_CPU_TEXTURES=1`: every registration and refresh.
fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REBUILD_TRACE_CPU_TEXTURES").is_some())
}

struct Slot {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    list7: Option<ID3D12GraphicsCommandList7>,
    /// staging for copies of CPU bytes and the detile tables (mapped for the run)
    upload: Option<ID3D12Resource>,
    upload_ptr: *mut u8,
    upload_bytes: u64,
    /// the detile pass's rows (a default-heap buffer with unordered access)
    staging: Option<ID3D12Resource>,
    staging_bytes: u64,
    /// the queue's progress value it was last submitted under
    value: u64,
}

// SAFETY: D3D12 objects are free-threaded; the mapped pointer is used under the pool's lock only.
unsafe impl Send for Slot {}

/// Lists per queue, reused once the queue's progress passes their value.
static POOLS: Mutex<Vec<(usize, Vec<Slot>)>> = Mutex::new(Vec::new());
const SLOTS_PER_QUEUE: usize = 8;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where one copy reads.
#[derive(Clone, Copy)]
enum Source {
    /// a host buffer and the offset of the texture's byte 0 in it
    Buffer(*mut c_void, u64),
    /// the CPU bytes, through the slot's upload buffer
    Cpu,
}

/// One texture of a refresh: where its bytes are, and how they become subresources.
struct Work<'a> {
    t: &'a Arc<Texture>,
    source: Source,
    /// a detile, and whether the CPU does it (into the upload buffer)
    plan: Option<Arc<crate::detile::Plan>>,
    cpu_detile: bool,
    /// once staged: the buffer the copies read and the offset their footprints count from
    copy_from: (*mut c_void, u64),
}

/// Before a submission's lists on `q` (after its ring wait): one host list that copies every marked texture, marked
/// with the submission's progress `value`. `None`: nothing marked (the common case), or not a direct queue (the marks
/// wait for one).
pub fn refresh(q: &crate::queue::Queue, value: u64) -> Option<*mut c_void> {
    if q.desc.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
        return None;
    }
    let frames = FRAMES.load(Relaxed);
    let new_frames = frames != FRAMES_SEEN.load(Relaxed);
    if !DIRTY.load(Relaxed) && !new_frames {
        return None;
    }
    let started = std::time::Instant::now();
    let r = refresh_marked(q, value, frames, new_frames);
    REFRESH_NS.fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
    r
}

fn refresh_marked(q: &crate::queue::Queue, value: u64, frames: u64, new_frames: bool) -> Option<*mut c_void> {
    let mut clock = std::time::Instant::now();
    DIRTY.store(false, Relaxed);
    FRAMES_SEEN.store(frames, Relaxed);
    let dirty: Vec<Arc<Texture>> = {
        let g = REGISTRY.read().unwrap_or_else(|e| e.into_inner());
        let r = g.as_ref()?;
        if new_frames {
            // a Bink frame: the linear textures outside watched memory whose bytes changed (one of the buffer sets)
            for t in r.by_address.values().flatten().filter(|t| !t.watched && t.class.is_none()) {
                let opened = in_place(t).is_some();
                let ok = opened || readable(t.cpu(), t.bytes);
                let changed = ok && {
                    let h = sample(t.cpu(), t.bytes);
                    t.sampled.swap(h, Relaxed) != h
                };
                if changed {
                    t.dirty.store(true, Relaxed);
                }
                if trace() && frames < 4 {
                    eprintln!("cputex    Bink frame {frames}: texture at {:#x} (CPU bytes at {:#x}): opened in place {opened}, readable {ok}, changed {changed}", t.address, t.cpu());
                }
            }
        }
        r.by_address.values().flatten().filter(|t| t.dirty.swap(false, Relaxed)).cloned().collect()
    };
    lap(&mut clock, 0);
    if dirty.is_empty() {
        return None;
    }
    let host = crate::device::list_host()?;
    let device = crate::device::get()?;
    let me = q as *const crate::queue::Queue as usize;
    let completed = unsafe { q.progress.GetCompletedValue() };
    let mut pools = lock(&POOLS);
    let k = match pools.iter().position(|p| p.0 == me) {
        Some(k) => k,
        None => {
            pools.push((me, Vec::new()));
            pools.len() - 1
        }
    };
    let pool = &mut pools[k].1;
    let free = match pool.iter().position(|s| s.value <= completed) {
        Some(i) => i,
        None if pool.len() < SLOTS_PER_QUEUE => unsafe {
            let allocator: ID3D12CommandAllocator = device.host.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
            let list: ID3D12GraphicsCommandList = device.host.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).ok()?;
            let _ = list.Close();
            let list7 = list.cast::<ID3D12GraphicsCommandList7>().ok();
            pool.push(Slot { allocator, list, list7, upload: None, upload_ptr: std::ptr::null_mut(), upload_bytes: 0, staging: None, staging_bytes: 0, value: 0 });
            pool.len() - 1
        },
        None => {
            // every list still running: the marks go to the next submission
            BUSY.fetch_add(1, Relaxed);
            for t in &dirty {
                t.dirty.store(true, Relaxed);
            }
            DIRTY.store(true, Relaxed);
            return None;
        }
    };
    let slot = &mut pool[free];
    let upload_all = mode() == Mode::Upload;
    let pass = if detile_mode() == Detile::Gpu { detiler(&device.host) } else { None };
    // each texture's source: watched memory from the card's copy the ring keeps; other memory from the title's
    // memory opened in place, else its CPU bytes; under the upload mode always the CPU bytes. A detile reads its
    // source on the card (the pass), or the CPU bytes (no pass, or REBUILD_DETILE=cpu).
    let mut work: Vec<Work> = Vec::with_capacity(dirty.len());
    let (mut need, mut staging) = (0u64, 0u64);
    let mut deferred = 0u64;
    for t in &dirty {
        let plan = match &t.copy {
            Copy::Rows(_) => None,
            Copy::Detile(..) => match t.plan() {
                Some(p) => Some(p),
                None => {
                    UNBACKED.fetch_add(1, Relaxed);
                    continue;
                }
            },
        };
        let cpu_detile = plan.is_some() && pass.is_none();
        let buffer = if upload_all || cpu_detile {
            None
        } else if t.watched {
            // pages nobody wrote yet have no card page: their tiles read zero (tiled resources tier 2, which the
            // model requires), as the CPU's fresh memory does (a glyph atlas the title fills a few glyphs at a time)
            host.memory(t.address).filter(|r| !r.resource.is_null() && r.console <= t.address && t.address + t.bytes <= r.console + r.bytes).map(|r| (r.resource, r.resource_offset + (t.address - r.console)))
        } else {
            in_place(t)
        };
        let source = match buffer {
            Some((r, at)) if at % 512 == 0 => Source::Buffer(r, at),
            _ if (upload_all || cpu_detile || !t.watched) && readable(t.cpu(), t.bytes) => Source::Cpu,
            _ => {
                UNBACKED.fetch_add(1, Relaxed);
                continue;
            }
        };
        if let Some(p) = &plan {
            // the batch's staging budget: the rest waits for the next submission (one texture always goes)
            if staging > 0 && staging.next_multiple_of(512) + p.staging > STAGING_BUDGET {
                t.dirty.store(true, Relaxed);
                deferred += 1;
                continue;
            }
            staging = staging.next_multiple_of(512) + p.staging;
            if cpu_detile {
                need = need.next_multiple_of(512) + p.staging;
            } else {
                need = need.next_multiple_of(256) + p.tables.len() as u64 * 4;
                if let Source::Cpu = source {
                    need = need.next_multiple_of(512) + t.bytes;
                }
            }
        } else if let Source::Cpu = source {
            need = need.next_multiple_of(512) + t.bytes;
        }
        work.push(Work { t, source, plan, cpu_detile, copy_from: (std::ptr::null_mut(), 0) });
    }
    if deferred > 0 {
        DEFERRED.fetch_add(deferred, Relaxed);
        DIRTY.store(true, Relaxed);
    }
    if work.is_empty() {
        return None;
    }
    lap(&mut clock, 1);
    let gpu_detiles = work.iter().filter(|w| w.plan.is_some() && !w.cpu_detile).count();
    if need > 0 && slot.upload_bytes < need {
        let bytes = need.next_power_of_two().max(1 << 20);
        let (buffer, ptr) = upload_buffer(&device.host, bytes)?;
        slot.upload = Some(buffer);
        slot.upload_ptr = ptr;
        slot.upload_bytes = bytes;
    }
    if gpu_detiles > 0 && slot.staging_bytes < staging {
        let bytes = staging.next_power_of_two().max(1 << 20);
        slot.staging = Some(staging_buffer(&device.host, bytes)?);
        slot.staging_bytes = bytes;
    }
    lap(&mut clock, 2);
    // the upload buffer's bytes, the staging offsets, and each detile's addresses for the pass
    let upload = slot.upload.as_ref().map_or(std::ptr::null_mut(), |u| u.as_raw());
    let upload_va = slot.upload.as_ref().map_or(0, |u| unsafe { u.GetGPUVirtualAddress() });
    let staging_raw = slot.staging.as_ref().map_or(std::ptr::null_mut(), |s| s.as_raw());
    let staging_va = slot.staging.as_ref().map_or(0, |s| unsafe { s.GetGPUVirtualAddress() });
    let mut passes: Vec<(Arc<crate::detile::Plan>, u64, u64, u64)> = Vec::with_capacity(gpu_detiles);
    let (mut at, mut st) = (0u64, 0u64);
    // CPU bytes into the upload buffer: (from, to, bytes), copied together after the loop
    let mut bulk: Vec<(usize, usize, usize)> = Vec::new();
    for w in &mut work {
        let t = w.t;
        // SAFETY (each copy below): readable title memory (checked) into our mapped upload buffer, inside its size
        match (&w.plan, w.cpu_detile, w.source) {
            (Some(p), true, _) => {
                at = at.next_multiple_of(512);
                let tiled = unsafe { std::slice::from_raw_parts(t.cpu() as *const u8, t.bytes as usize) };
                let rows = unsafe { std::slice::from_raw_parts_mut(slot.upload_ptr.add(at as usize), p.staging as usize) };
                crate::detile::reference(p, tiled, rows);
                w.copy_from = (upload, at);
                at += p.staging;
            }
            (Some(p), false, source) => {
                let bytes_va = match source {
                    Source::Buffer(r, o) => crate::detile::gpu_address(r)? + o,
                    Source::Cpu => {
                        at = at.next_multiple_of(512);
                        bulk.push((t.cpu() as usize, slot.upload_ptr as usize + at as usize, t.bytes as usize));
                        let va = upload_va + at;
                        at += t.bytes;
                        va
                    }
                };
                at = at.next_multiple_of(256);
                unsafe { std::ptr::copy_nonoverlapping(p.tables.as_ptr().cast::<u8>(), slot.upload_ptr.add(at as usize), p.tables.len() * 4) };
                let tables_va = upload_va + at;
                at += p.tables.len() as u64 * 4;
                st = st.next_multiple_of(512);
                passes.push((p.clone(), bytes_va, tables_va, staging_va + st));
                w.copy_from = (staging_raw, st);
                st += p.staging;
            }
            (None, _, Source::Cpu) => {
                at = at.next_multiple_of(512);
                bulk.push((t.cpu() as usize, slot.upload_ptr as usize + at as usize, t.bytes as usize));
                w.copy_from = (upload, at);
                at += t.bytes;
            }
            (None, _, Source::Buffer(r, o)) => w.copy_from = (r, o),
        }
    }
    // SAFETY: each is readable title memory (checked) into our mapped upload buffer, inside its size, not overlapping
    unsafe { copy_bulk(&bulk) };
    unsafe {
        slot.allocator.Reset().ok()?;
        slot.list.Reset(&slot.allocator, None).ok()?;
        let textures: Vec<&Arc<Texture>> = work.iter().map(|w| w.t).collect();
        if let (Some(d), false) = (pass, passes.is_empty()) {
            let s = slot.staging.as_ref()?;
            if slot.list7.is_none() {
                slot.list.ResourceBarrier(&[transition(s, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
            }
            d.bind(&slot.list);
            for (p, bytes, tables, rows) in &passes {
                d.record(&slot.list, p, *bytes, *tables, *rows);
            }
            staging_barrier(slot, s, true);
        }
        if !barriers(slot, &textures, true) {
            let _ = slot.list.Close();
            return None;
        }
        let mut bytes = 0u64;
        let mut detiled = 0u64;
        for w in &work {
            let t = w.t;
            let (src, base) = w.copy_from;
            let copy = |subresource: u32, offset: u64, width: u32, height: u32, depth: u32, pitch: u32| {
                let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: borrowed(&t.host), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: subresource } };
                let source = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&src),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: base + offset,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: t.format, Width: width, Height: height, Depth: depth, RowPitch: pitch },
                        },
                    },
                };
                slot.list.CopyTextureRegion(&dst, 0, 0, 0, &source, None);
            };
            match (&t.copy, &w.plan) {
                (Copy::Rows(rows), _) => {
                    for r in rows {
                        copy(r.subresource, r.offset, r.width, r.height, 1, r.pitch);
                        bytes += u64::from(r.pitch) * u64::from(r.rows);
                    }
                }
                (Copy::Detile(..), Some(p)) => {
                    for f in &p.footprints {
                        let n = u64::from(f.pitch) * u64::from(f.rows) * u64::from(f.depth);
                        copy(f.subresource, f.offset, f.width, f.height, f.depth, f.pitch);
                        bytes += n;
                        detiled += n;
                    }
                    DETILE_COPIES.fetch_add(1, Relaxed);
                    // what this copy took, for the census's staleness count
                    let cpu = t.cpu();
                    if readable(cpu, t.bytes) {
                        t.copied.store(sample(cpu, t.bytes) | 1, Relaxed);
                    }
                }
                (Copy::Detile(..), None) => {}
            }
        }
        barriers(slot, &textures, false);
        if let (Some(s), false) = (&slot.staging, passes.is_empty()) {
            staging_barrier(slot, s, false);
        }
        slot.list.Close().ok()?;
        slot.value = value;
        let n = REFRESHED.fetch_add(textures.len() as u64, Relaxed);
        BYTES.fetch_add(bytes, Relaxed);
        let before = DETILE_BYTES.fetch_add(detiled, Relaxed);
        if trace() || n == 0 || (detiled > 0 && before == 0) {
            eprintln!(
                "cputex    refresh: {} textures ({} through the upload buffer, {} detiled on the card, {} on the CPU), {bytes} bytes, first at {:#x}",
                textures.len(),
                work.iter().filter(|w| matches!(w.source, Source::Cpu)).count(),
                passes.len(),
                work.iter().filter(|w| w.cpu_detile).count(),
                textures[0].address
            );
        }
        lap(&mut clock, 3);
        Some(slot.list.as_raw())
    }
}

/// CPU bytes copied into upload buffers, and the time it took (ns).
pub static BULK_BYTES: AtomicU64 = AtomicU64::new(0);
pub static BULK_NS: AtomicU64 = AtomicU64::new(0);

/// Adds a batch's bytes and time to the totals when it ends.
struct Count(std::time::Instant, usize);

impl Drop for Count {
    fn drop(&mut self) {
        BULK_BYTES.fetch_add(self.1 as u64, Relaxed);
        BULK_NS.fetch_add(self.0.elapsed().as_nanos() as u64, Relaxed);
    }
}

/// Copies `(from, to, bytes)` each, on this thread. Measured on the card (a Bink frame, 12.6 MB): 11.1 GB/s on one
/// thread, 12.2 GB/s split over four; the split saved 0.1 ms a video frame and was dropped.
///
/// # Safety
/// Every `from` is readable and every `to` writable for its bytes; no two overlap.
unsafe fn copy_bulk(bulk: &[(usize, usize, usize)]) {
    let total: usize = bulk.iter().map(|b| b.2).sum();
    let _count = Count(std::time::Instant::now(), total);
    for &(from, to, n) in bulk {
        unsafe { std::ptr::copy_nonoverlapping(from as *const u8, to as *mut u8, n) };
    }
}

/// The staging buffer from the pass to the copies (`to_copy`), or back to where the next list's pass finds it
/// (legacy barriers only: enhanced ones keep no state for buffers).
unsafe fn staging_barrier(slot: &Slot, s: &ID3D12Resource, to_copy: bool) {
    if let Some(l7) = &slot.list7 {
        if to_copy {
            let b = [D3D12_BUFFER_BARRIER {
                SyncBefore: D3D12_BARRIER_SYNC_COMPUTE_SHADING,
                SyncAfter: D3D12_BARRIER_SYNC_COPY,
                AccessBefore: D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
                AccessAfter: D3D12_BARRIER_ACCESS_COPY_SOURCE,
                pResource: borrowed(s),
                Offset: 0,
                Size: u64::MAX,
            }];
            let group = D3D12_BARRIER_GROUP { Type: D3D12_BARRIER_TYPE_BUFFER, NumBarriers: 1, Anonymous: D3D12_BARRIER_GROUP_0 { pBufferBarriers: b.as_ptr() } };
            unsafe { l7.Barrier(&[group]) };
        }
        return;
    }
    let (from, to) = if to_copy { (D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE) } else { (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON) };
    unsafe { slot.list.ResourceBarrier(&[transition(s, from, to)]) };
}

fn staging_buffer(device: &ID3D12Device, bytes: u64) -> Option<ID3D12Resource> {
    let mut r: Option<ID3D12Resource> = None;
    unsafe {
        device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &rebuild_d3d12x::resource::buffer_desc(bytes, true), D3D12_RESOURCE_STATE_COMMON, None, &mut r)
            .ok()?;
    }
    r
}

/// Each texture from its layout to copy-dest (`to_copy`), or back. Enhanced barriers (the layout a texture lives in
/// may be any read layout the title gave it); a host without them: legacy barriers, for textures in the common layout
/// only (false when some texture cannot be moved, nothing recorded then).
unsafe fn barriers(slot: &Slot, textures: &[&Arc<Texture>], to_copy: bool) -> bool {
    let copy = D3D12_BARRIER_LAYOUT_COPY_DEST;
    if let Some(l7) = &slot.list7 {
        let b: Vec<D3D12_TEXTURE_BARRIER> = textures
            .iter()
            .map(|t| D3D12_TEXTURE_BARRIER {
                SyncBefore: if to_copy { D3D12_BARRIER_SYNC_ALL } else { D3D12_BARRIER_SYNC_COPY },
                SyncAfter: if to_copy { D3D12_BARRIER_SYNC_COPY } else { D3D12_BARRIER_SYNC_ALL },
                AccessBefore: if to_copy { D3D12_BARRIER_ACCESS_COMMON } else { D3D12_BARRIER_ACCESS_COPY_DEST },
                AccessAfter: if to_copy { D3D12_BARRIER_ACCESS_COPY_DEST } else { D3D12_BARRIER_ACCESS_COMMON },
                LayoutBefore: if to_copy { t.layout } else { copy },
                LayoutAfter: if to_copy { copy } else { t.layout },
                pResource: borrowed(&t.host),
                Subresources: D3D12_BARRIER_SUBRESOURCE_RANGE { IndexOrFirstMipLevel: 0xffff_ffff, ..Default::default() },
                Flags: D3D12_TEXTURE_BARRIER_FLAG_NONE,
            })
            .collect();
        let group = D3D12_BARRIER_GROUP { Type: D3D12_BARRIER_TYPE_TEXTURE, NumBarriers: b.len() as u32, Anonymous: D3D12_BARRIER_GROUP_0 { pTextureBarriers: b.as_ptr() } };
        unsafe { l7.Barrier(&[group]) };
        return true;
    }
    if textures.iter().any(|t| t.layout != D3D12_BARRIER_LAYOUT_COMMON) {
        return false;
    }
    let (from, to) = if to_copy { (D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST) } else { (D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON) };
    let b: Vec<D3D12_RESOURCE_BARRIER> = textures.iter().map(|t| transition(&t.host, from, to)).collect();
    unsafe { slot.list.ResourceBarrier(&b) };
    true
}

/// A texture outside watched memory, as the title's memory opened in place for the GPU (`crate::titlemem`: physical
/// pages mapped into views, allocations the GPU reads where they are): the buffer and the offset of its byte 0.
/// Memory opened there is committed for as long as it is open, so the CPU may read it too.
fn in_place(t: &Texture) -> Option<(*mut c_void, u64)> {
    let c = t.cpu();
    let r = crate::device::list_host()?.memory(c)?;
    (!r.resource.is_null() && r.console <= c && c + t.bytes <= r.console + r.bytes).then(|| (r.resource, r.resource_offset + (c - r.console)))
}

/// `bytes` at `address` are committed memory the CPU can read: every region over them (physical pages map one view
/// per 64 KiB page, so a texture spans many).
fn readable(address: u64, bytes: u64) -> bool {
    use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS};
    let end = address + bytes;
    let mut at = address;
    while at < end {
        let mut m = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: a query, nothing is read
        if unsafe { VirtualQuery(Some(at as *const c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
            return false;
        }
        if m.State != MEM_COMMIT || m.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0) != 0 || m.RegionSize == 0 {
            return false;
        }
        at = m.BaseAddress as u64 + m.RegionSize as u64;
    }
    true
}

fn upload_buffer(device: &ID3D12Device, bytes: u64) -> Option<(ID3D12Resource, *mut u8)> {
    let mut r: Option<ID3D12Resource> = None;
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &rebuild_d3d12x::resource::buffer_desc(bytes, false),
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut r,
            )
            .ok()?;
        let r = r?;
        let mut p: *mut c_void = std::ptr::null_mut();
        r.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p)).ok()?;
        Some((r, p.cast()))
    }
}

/// The run's totals, for the end-of-run report.
pub fn report() -> String {
    // the census of the textures still alive (their marks and changes so far)
    if let Some(r) = REGISTRY.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        r.by_address.values().flatten().for_each(|t| settle(t));
    }
    let mut s = format!(
        "cpu textures: {} kept current ({} outside watched memory; {} Bink frames; {} through the detile pass, {} on GPU-only memory left alone), {} copies ({} detiles), {} MiB copied ({} MiB detiled), {} put off for a list, {} for the staging budget, {} skipped (unwritten or unreadable), {} marked by page remaps, {} detiles not fitting their host texture; refresh CPU {:.1} ms (finding {:.1}, sources and plans {:.1}, buffers {:.1}, staging and recording {:.1}), of it {:.1} ms copying {} MiB of CPU bytes ({:.1} GB/s); refused: {} not linear (no addressing: pipe or bank XOR, multisampled, two planes, no layout), {} rows the host cannot copy",
        REGISTERED.load(Relaxed),
        UNWATCHED.load(Relaxed),
        FRAMES.load(Relaxed),
        DETILED.load(Relaxed),
        GPU_ONLY.load(Relaxed),
        REFRESHED.load(Relaxed),
        DETILE_COPIES.load(Relaxed),
        BYTES.load(Relaxed) >> 20,
        DETILE_BYTES.load(Relaxed) >> 20,
        BUSY.load(Relaxed),
        DEFERRED.load(Relaxed),
        UNBACKED.load(Relaxed),
        REMAPPED.load(Relaxed),
        MISFIT.load(Relaxed),
        REFRESH_NS.load(Relaxed) as f64 / 1e6,
        PHASE_NS[0].load(Relaxed) as f64 / 1e6,
        PHASE_NS[1].load(Relaxed) as f64 / 1e6,
        PHASE_NS[2].load(Relaxed) as f64 / 1e6,
        PHASE_NS[3].load(Relaxed) as f64 / 1e6,
        BULK_NS.load(Relaxed) as f64 / 1e6,
        BULK_BYTES.load(Relaxed) >> 20,
        BULK_BYTES.load(Relaxed) as f64 / BULK_NS.load(Relaxed).max(1) as f64,
        NOT_LINEAR.load(Relaxed),
        NOT_COPYABLE.load(Relaxed)
    );
    let census = lock(&CENSUS);
    let mut classes: Vec<(&Class, &Tally)> = census.iter().collect();
    classes.sort_by(|a, b| b.1.textures.cmp(&a.1.textures).then(a.0.cmp(b.0)));
    // totals by where the memory is, and by memory and mode
    let mut by_mode: BTreeMap<(Mem, u32), (u64, u64)> = BTreeMap::new();
    for (c, t) in &classes {
        let e = by_mode.entry((c.mem, c.swizzle)).or_default();
        e.0 += t.textures;
        e.1 += t.bytes;
    }
    s += "\ncputex    census by memory and mode:";
    for ((m, mode), (n, b)) in &by_mode {
        s += &format!(" {m:?}/{mode} {n} ({:.1} MiB);", *b as f64 / (1 << 20) as f64);
    }
    let mut by_mem: BTreeMap<Mem, Tally> = BTreeMap::new();
    for (c, t) in &classes {
        let m = by_mem.entry(c.mem).or_default();
        m.textures += t.textures;
        m.bytes += t.bytes;
        m.data_at_creation += t.data_at_creation;
        m.marked += t.marked;
        m.changed += t.changed;
        m.stale += t.stale;
    }
    for (m, t) in &by_mem {
        s += &format!(
            "\ncputex    census {m:?}: {} textures, {:.1} MiB, {} with data when made, {} written again, {} changed, {} stale",
            t.textures,
            t.bytes as f64 / (1 << 20) as f64,
            t.data_at_creation,
            t.marked,
            t.changed,
            t.stale
        );
    }
    let shown = if trace() { classes.len() } else { classes.len().min(30) };
    for (c, t) in &classes[..shown] {
        s += &format!(
            "\ncputex    census {:?} mode {} format {} dim {} mips {} arrays {}: {} textures, {:.1} MiB, {} with data when made, {} written again ({} times), {} changed, {} stale; {}x{} .. {}x{}",
            c.mem, c.swizzle, c.format, c.dimension, c.mips, c.arrays, t.textures, t.bytes as f64 / (1 << 20) as f64, t.data_at_creation, t.marked, t.marks, t.changed, t.stale, t.smallest.0, t.smallest.1, t.largest.0, t.largest.1
        );
    }
    if shown < classes.len() {
        s += &format!("\ncputex    census: {} more classes (REBUILD_TRACE_CPU_TEXTURES=1 lists them)", classes.len() - shown);
    }
    s
}

fn borrowed(r: &ID3D12Resource) -> std::mem::ManuallyDrop<Option<ID3D12Resource>> {
    // SAFETY: a borrowed pointer for the call's lifetime (no reference taken)
    unsafe { std::mem::transmute_copy(r) }
}

fn transition(r: &ID3D12Resource, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: borrowed(r),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: from,
                StateAfter: to,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    fn tex(w: u64, h: u32, format: i32, mips: u16, layout: i32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: mips,
            Format: DXGI_FORMAT(format),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT(layout),
            ..Default::default()
        }
    }

    #[test]
    fn a_linear_video_plane_copies_as_it_lies() {
        // a 1920x1080 R8 luma plane, console linear (0x100): rows of 1920 bytes padded to 256 -> 2048
        let (rows, bytes) = linear_rows(&tex(1920, 1080, 61, 1, 0x100)).unwrap();
        assert_eq!(rows, vec![Rows { subresource: 0, offset: 0, width: 1920, height: 1080, rows: 1080, pitch: 2048 }]);
        assert_eq!(bytes, 2048 * 1080);
        // row major (1) is linear too
        assert_eq!(linear_rows(&tex(960, 540, 61, 1, 1)).unwrap().0[0].pitch, 1024);
    }

    #[test]
    fn swizzled_and_uncopyable_layouts_are_refused() {
        // "the library picks" for a sampled 2D texture: SW_4KB_S
        assert_eq!(linear_rows(&tex(1920, 1080, 61, 1, 0)), Err(Refusal::NotLinear));
        assert_eq!(linear_rows(&tex(1920, 1080, 61, 1, 0x105)), Err(Refusal::NotLinear));
        // a linear mip chain packs mips smallest first: the small ones start off the 512-byte grid
        assert_eq!(linear_rows(&tex(256, 256, 28, 0, 0x100)), Err(Refusal::NotCopyable));
    }

    #[test]
    fn detile_footprints_fit_the_host_texture_they_copy_into() {
        let layout = |d: &D3D12_RESOURCE_DESC| crate::detile::layout_of(d).unwrap();
        // a BC7 520x260 chain, a 256x128 R8 SW_4KB_S texture, a 3D LUT, a 6-slice array
        for d in [tex(520, 260, 98, 0, 0x105), tex(256, 128, 61, 1, 0x105), D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D, DepthOrArraySize: 17, ..tex(17, 17, 10, 1, 0x105) }, D3D12_RESOURCE_DESC { DepthOrArraySize: 6, ..tex(512, 512, 77, 0, 0x105) }] {
            let l = layout(&d);
            let p = crate::detile::plan(&l).unwrap();
            let mut host = d;
            host.MipLevels = l.mips.len() as u16;
            assert!(fits(&host, &p.footprints, l.desc.block), "{}x{} format {}", d.Width, d.Height, d.Format.0);
            // a host texture with one mip fewer, or another size, takes none of them
            host.MipLevels -= u16::from(host.MipLevels > 1);
            let fewer = l.mips.len() > 1;
            assert_eq!(fits(&host, &p.footprints, l.desc.block), !fewer);
            host.MipLevels = l.mips.len() as u16;
            host.Width += 4;
            assert!(!fits(&host, &p.footprints, l.desc.block));
        }
    }

    #[test]
    fn pages_find_the_textures_they_cover() {
        let mut m: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
        for t in [(0x10_0000, 0x30_0000), (0x50_0000, 0x1000), (0x50_0000, 0x2_0000), (0x60_8000, 0x100)] {
            m.entry(t.0).or_default().push(t);
        }
        let longest = 0x30_0000;
        let hits = |p: u64| over(&m, longest, p, |t| *t).copied().collect::<Vec<_>>();
        assert_eq!(hits(0x10_0000), vec![(0x10_0000, 0x30_0000)]);
        assert_eq!(hits(0x3f_0000), vec![(0x10_0000, 0x30_0000)], "the last page of a long texture");
        assert!(hits(0x40_0000).is_empty(), "the page just past it");
        assert!(hits(0x0f_0000).is_empty(), "the page just before it");
        assert_eq!(hits(0x50_0000).len(), 2, "two textures over one memory");
        assert_eq!(hits(0x51_0000), vec![(0x50_0000, 0x2_0000)]);
        assert_eq!(hits(0x60_0000), vec![(0x60_8000, 0x100)], "a texture starting mid-page");
    }

    #[test]
    fn a_frame_in_another_buffer_set_leaves_the_sample_alone() {
        let mut plane = vec![0u64; 3840 * 2160 / 8];
        let at = |p: &Vec<u64>| p.as_ptr() as u64;
        let bytes = (plane.len() * 8) as u64;
        let a = sample(at(&plane), bytes);
        assert_eq!(sample(at(&plane), bytes), a, "the same bytes");
        // a new frame writes everywhere: every sampled word moves
        for (i, w) in plane.iter_mut().enumerate() {
            *w = i as u64 * 0x9e37_79b9;
        }
        let b = sample(at(&plane), bytes);
        assert_ne!(a, b);
        // the last word is sampled too
        *plane.last_mut().unwrap() ^= 1;
        assert_ne!(sample(at(&plane), bytes), b);
    }
}
