//! Build 5: plan decision 1 in the title run (`docs/design/memory-run.md`).
//!
//! The title's CPU-written graphics memory (every write-watched XMem reservation) gets **card pages at its own
//! addresses**: each 512 MiB sub-window it touches is one reserved buffer (`uma::space`), its views placed in the
//! view heap at `WINDOW_HEAP_BASE + k` (SRV) and `WINDOW_UAV_BASE + k` (UAV) for translated shaders' window
//! addressing. What the CPU changes reaches those pages through the Build 2 ring: each submission asks for a flush
//! and its host queue waits for the ring's fence (the GPU waits, the title's thread does not copy).
//!
//! **Card pages are placed lazily**: a 64 KiB page gets one when the ring first finds CPU bytes to send into it,
//! or when a GPU resource covers it (the arena's, or a buffer the title places in its own memory). A page nobody
//! wrote reads zero through an unmapped tile (tiled resources tier 2), exactly what the CPU's fresh memory holds.
//! The title commits ~5 GiB of graphics memory and writes far less of it; placing it all at commit filled the card
//! and textures failed (E_OUTOFMEMORY, the title's assert at `exe+0x131a2ad`). `REBUILD_MEMORY=eager` places at
//! commit (for bisecting), `REBUILD_MEMORY=standin` keeps the old path (no tiles, no ring, no window views; the
//! title's memory opened in place by `titlemem`).
//!
//! d3d12x resources get console addresses from the [`Arena`]: GPU-only memory on card pages only, CPU-visible
//! memory committed and watched (the ring), READBACK memory committed and read by the GPU in place.
//!
//! **Locks**, always in this order: the XMem state lock (`exports`), then `maps`, then the ring's region lock, then
//! `tiles` (a leaf: nothing is waited for while holding it). The ring's coordinator holds the region lock for a
//! pass and takes `tiles` to place pages; a free takes `maps`, then the region lock (waiting for the pass), then
//! `tiles`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Memory::*;

use rebuild_uma::ring::{Backer, Ring, WindowVa, SLOT_BYTES};
use rebuild_uma::space::{Space, SUB, SUBS};
use rebuild_uma::{Backed, PlacementId, Uma, PAGE, WINDOW};
use rebuild_xlist::console::MemoryRange;

const P: u64 = PAGE as u64;

fn mode() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| std::env::var("REBUILD_MEMORY").unwrap_or_default().to_ascii_lowercase())
}

/// `REBUILD_MEMORY=standin`: the old path.
pub fn standin() -> bool {
    mode() == "standin"
}

pub struct Model {
    ring: Ring,
    /// sub-window k's GPU address (0: not made yet), read by the ring's coordinator without a lock
    vas: Arc<Vec<AtomicU64>>,
    /// sub-window k's reserved buffer (borrowed: the space keeps it for the run)
    subs: Vec<AtomicPtr<c_void>>,
    maps: Mutex<Maps>,
    tiles: Mutex<Tiles>,
    resources: OnceLock<rebuild_d3d12x::Resources>,
    /// card pages placed at commit (REBUILD_MEMORY=eager), not on first write
    eager: bool,
}

// SAFETY: D3D12 objects are free-threaded; the state is locked.
unsafe impl Send for Model {}
unsafe impl Sync for Model {}

/// What memory there is.
struct Maps {
    /// watched reservations: base -> size (what the ring reads)
    watched: BTreeMap<u64, u64>,
    /// every span of console addresses that may have card pages: watched reservations and the arena's pieces
    spans: BTreeMap<u64, u64>,
    arena: ArenaState,
}

/// Which pages have card pages (a leaf lock).
struct Tiles {
    uma: Uma,
    space: Space,
    /// 64 KiB pages with a card page: page address -> its placement
    pages: HashMap<u64, PlacementId>,
    /// sub-windows whose views wait for the resources (made before `set_resources`)
    views_pending: Vec<u32>,
    /// key pages (`crate::mapped`) mapped in their sub-window: only those given a reused card page, to zero it
    keys_mapped: std::collections::HashSet<u64>,
}

static MODEL: OnceLock<Model> = OnceLock::new();

/// Counters for the run report.
pub static PAGES_PLACED: AtomicU64 = AtomicU64::new(0);
pub static PAGES_ON_WRITE: AtomicU64 = AtomicU64::new(0);
pub static PAGES_RETIRED: AtomicU64 = AtomicU64::new(0);
pub static PAGES_ZEROED: AtomicU64 = AtomicU64::new(0);
pub static PAGES_REFUSED: AtomicU64 = AtomicU64::new(0);
pub static REGIONS_ADDED: AtomicU64 = AtomicU64::new(0);
pub static REGIONS_REMOVED: AtomicU64 = AtomicU64::new(0);
pub static MAP_FAILED: AtomicU64 = AtomicU64::new(0);

pub fn get() -> Option<&'static Model> {
    MODEL.get()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start the model on the host device (once, at device creation), then take over the watched memory the title
/// made before there was a device. `None` under `REBUILD_MEMORY=standin` or when the device cannot (tiled
/// resources tier 2 is required): the run keeps the stand-in.
pub fn start(device: &ID3D12Device, warp: bool) -> Option<&'static Model> {
    if standin() {
        eprintln!("memory    REBUILD_MEMORY=standin: the title's memory is opened in place (no tiles, no ring, no window views)");
        return None;
    }
    if let Some(m) = MODEL.get() {
        return Some(m);
    }
    let vas: Arc<Vec<AtomicU64>> = Arc::new((0..SUBS).map(|_| AtomicU64::new(0)).collect());
    let v2 = vas.clone();
    let va: WindowVa = Arc::new(move |k| v2.get(k as usize).map(|v| v.load(Relaxed)).filter(|&v| v != 0));
    let ring = match Ring::dynamic(device, SUB, va, 4, SLOT_BYTES) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("memory    the upload ring failed ({e}): the stand-in stays");
            return None;
        }
    };
    let space = match Space::new(device, &ring.queue) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("memory    no tiles ({e}): the stand-in stays");
            return None;
        }
    };
    // card memory the tiles may take: REBUILD_TILE_MIB, else 6 GiB on a card and 2 GiB on WARP (WARP's "card"
    // memory is the process's own, under the run's job limit)
    let mib = std::env::var("REBUILD_TILE_MIB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(if warp { 2048 } else { 6144 });
    let tiles = Tiles { uma: Uma::new(SUBS * SUB as usize / WINDOW, mib << 20), space, pages: HashMap::new(), views_pending: Vec::new(), keys_mapped: Default::default() };
    let maps = Maps { watched: BTreeMap::new(), spans: BTreeMap::new(), arena: ArenaState::default() };
    let model = Model {
        ring,
        vas,
        subs: (0..SUBS).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
        maps: Mutex::new(maps),
        tiles: Mutex::new(tiles),
        resources: OnceLock::new(),
        eager: mode() == "eager",
    };
    if MODEL.set(model).is_err() {
        return MODEL.get();
    }
    let m = MODEL.get()?;
    // the ring asks for card pages under what it is about to send (lazy placement)
    let backer: Backer = Arc::new(|pages: &[u64]| {
        if let Some(m) = MODEL.get() {
            let mut t = lock(&m.tiles);
            let before = PAGES_PLACED.load(Relaxed);
            m.place_pages(&mut t, pages.iter().copied());
            PAGES_ON_WRITE.fetch_add(PAGES_PLACED.load(Relaxed) - before, Relaxed);
        }
        // textures over these pages take the new bytes at the next submission (crate::cputex)
        crate::cputex::written(pages);
    });
    m.ring.set_backer(backer);
    // quiet slices go cold when the I/O syscalls are guarded (`install_cold`, before the title ran)
    m.ring.set_cold(rebuild_uma::cold::enabled());
    // what the title made before the device: with no allocation or free running meanwhile
    crate::exports::with_watched(|regions| {
        for &(base, size) in regions {
            reserved(base, size);
            if !m.eager {
                continue;
            }
            let mut at = base;
            while at < base + size {
                let mut mbi = MEMORY_BASIC_INFORMATION::default();
                if unsafe { VirtualQuery(Some(at as *const c_void), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
                    break;
                }
                let end = (mbi.BaseAddress as u64 + mbi.RegionSize as u64).min(base + size);
                if mbi.State == MEM_COMMIT {
                    committed(mbi.BaseAddress as u64, end - mbi.BaseAddress as u64);
                }
                at = end;
            }
        }
        eprintln!(
            "memory    tiles and the upload ring are on ({}): {} watched regions ({} MiB) taken over, {mib} MiB of card pages allowed",
            if m.eager { "card pages at commit" } else { "card pages on first write" },
            regions.len(),
            regions.iter().map(|r| r.1).sum::<u64>() >> 20
        );
    });
    Some(m)
}

/// The resources (d3d12x) the window views go into; views of sub-windows made before are placed now.
pub fn set_resources(r: &rebuild_d3d12x::Resources) {
    let Some(m) = MODEL.get() else { return };
    let _ = m.resources.set(r.clone());
    let pending = std::mem::take(&mut lock(&m.tiles).views_pending);
    for k in pending {
        m.place_views(k);
    }
}

/// A write-watched reservation was made at `base`.
pub fn reserved(base: u64, size: u64) {
    let Some(m) = MODEL.get() else { return };
    let mut maps = lock(&m.maps);
    if maps.watched.contains_key(&base) {
        return;
    }
    // SAFETY: write-watched memory, reserved until `freeing` removes it from the ring
    match unsafe { m.ring.add(base as *mut u8, size as usize, base) } {
        Ok(()) => {
            maps.watched.insert(base, size);
            maps.spans.insert(base, size);
            REGIONS_ADDED.fetch_add(1, Relaxed);
        }
        Err(e) => eprintln!("memory    watched region {base:#x} (+{size:#x}) not added: {e}"),
    }
}

/// Cold slices (`rebuild_uma::cold`): watched memory no write touched for a while is made read-only and no
/// longer scanned; a write faults it hot again. On unless `REBUILD_COLD=0`, and only once the I/O syscalls are
/// guarded (`kwrite`): a kernel write into a read-only page would fail. Call before the title runs.
pub fn install_cold() {
    if std::env::var("REBUILD_COLD").is_ok_and(|v| v == "0") || standin() {
        eprintln!("memory    cold slices off (REBUILD_COLD=0 or the stand-in): every pass reads all watched memory");
        return;
    }
    if crate::kwrite::install() {
        rebuild_uma::cold::enable();
        // what asks the protection of the title's memory sees what the title made, not our read-only marks
        rebuild_d3d12x::vmem::set_tracked(rebuild_uma::cold::original);
    }
}

/// `size` bytes at `at` were committed. Under `REBUILD_MEMORY=eager`, watched pages get card pages now; else on
/// their first write (the ring) or under a resource.
pub fn committed(at: u64, size: u64) {
    // fresh pages in a cold slice: it turns hot (scanned) before the title can write them
    rebuild_uma::cold::touch(at, size);
    let Some(m) = MODEL.get() else { return };
    if !m.eager {
        return;
    }
    let (first, end) = {
        let maps = lock(&m.maps);
        let Some((&base, &len)) = maps.watched.range(..=at).next_back() else { return };
        if at >= base + len {
            return;
        }
        (at / P * P, (at + size).min(base + len).div_ceil(P) * P)
    };
    let mut t = lock(&m.tiles);
    m.place_pages(&mut t, (first..end).step_by(PAGE));
}

/// Free (`VirtualFree(base, size, kind)` as `free` does it), after the model stops reading what goes.
pub fn freeing(base: u64, size: u64, kind: u32, free: impl FnOnce() -> windows::core::Result<()>) -> windows::core::Result<()> {
    let Some(m) = MODEL.get() else { return free() };
    let release = kind & MEM_RELEASE.0 != 0 && kind & MEM_PRESERVE_PLACEHOLDER.0 == 0;
    let decommit = kind & MEM_DECOMMIT.0 != 0;
    if !release && !decommit {
        return free();
    }
    let mut maps = lock(&m.maps);
    let Some((&wbase, &wlen)) = maps.watched.range(..=base).next_back() else { return free() };
    if base >= wbase + wlen {
        return free();
    }
    if release {
        // the ring lets go first (waits for a pass reading it), then the memory and its card pages go
        m.ring.remove(wbase);
        maps.watched.remove(&wbase);
        maps.spans.remove(&wbase);
        REGIONS_REMOVED.fetch_add(1, Relaxed);
        let r = free();
        m.retire(&mut lock(&m.tiles), wbase, wbase + wlen);
        r
    } else {
        let len = if size == 0 { wbase + wlen - base } else { size };
        let r = m.ring.quiet(base, len, free);
        // only whole 64 KiB pages: a page partly committed still has CPU bytes the GPU reads
        m.retire(&mut lock(&m.tiles), base.div_ceil(P) * P, (base + len) / P * P);
        r
    }
}

/// `bytes` at `address` lie in one watched region (the ring keeps the card's copy of them current).
pub fn watched(address: u64, bytes: u64) -> bool {
    let Some(m) = MODEL.get() else { return false };
    let maps = lock(&m.maps);
    maps.watched.range(..=address).next_back().is_some_and(|(&b, &l)| address + bytes <= b + l)
}

/// `address` lies in the arena's GPU-only memory: card pages only, never committed, the CPU never writes it.
pub fn gpu_only(address: u64) -> bool {
    let Some(m) = MODEL.get() else { return false };
    let maps = lock(&m.maps);
    let in_span = maps.spans.range(..=address).next_back().is_some_and(|(&b, &l)| address < b + l);
    in_span && !maps.watched.range(..=address).next_back().is_some_and(|(&b, &l)| address < b + l)
}

/// Before a submission's lists: everything the CPU changed goes up, and `queue` waits for it (on the GPU).
#[inline]
pub fn flush_wait(queue: &ID3D12CommandQueue) {
    if let Some(m) = MODEL.get() {
        let v = m.ring.flush();
        let _ = unsafe { queue.Wait(&m.ring.fence, v) };
    }
    // and for the reserved textures' tile mappings so far (their own queue; crate::tilequeue)
    crate::tilequeue::wait(queue);
}

/// The card's copy of watched memory or the arena holding `va`: its span inside one sub-window.
pub fn range(va: u64) -> Option<MemoryRange> {
    let m = MODEL.get()?;
    let (base, len) = {
        let maps = lock(&m.maps);
        let (&b, &l) = maps.spans.range(..=va).next_back()?;
        (b, l)
    };
    if va >= base + len {
        return None;
    }
    let k = (va / SUB) as usize;
    let sub = k as u64 * SUB;
    let resource = match m.subs.get(k)?.load(Relaxed) {
        // a sub-window nothing was placed in yet: make it now (lists cache this range for good)
        r if r.is_null() => {
            let mut t = lock(&m.tiles);
            m.ensure_sub(&mut t, k)?;
            m.subs[k].load(Relaxed)
        }
        r => r,
    };
    let (start, end) = (base.max(sub), (base + len).min(sub + SUB));
    Some(MemoryRange { console: start, bytes: end - start, host_va: m.vas[k].load(Relaxed) + (start - sub), resource, resource_offset: start - sub })
}

/// Upload totals for the frame line: (bytes sent, coordinator µs, passes, flushes asked).
pub fn upload_totals() -> Option<(u64, u64, u64, u64)> {
    let t = MODEL.get()?.ring.totals();
    Some((t.bytes.load(Relaxed), t.busy_us.load(Relaxed), t.passes.load(Relaxed), t.flushes.load(Relaxed)))
}

/// Card pages in use by what they hold: (the title's watched memory, the arena's GPU-only pieces, the arena's
/// CPU-visible pieces), in pages.
pub fn pages_by_region() -> Option<[u64; 3]> {
    let m = MODEL.get()?;
    let maps = lock(&m.maps);
    let pages: Vec<u64> = lock(&m.tiles).pages.keys().copied().collect();
    let mut n = [0u64; 3];
    for p in pages {
        let i = match maps.arena.pieces.range(..=p).next_back() {
            Some((&b, &(l, k))) if p < b + l => {
                if k == Kind::Gpu {
                    1
                } else {
                    2
                }
            }
            _ => 0,
        };
        n[i] += 1;
    }
    Some(n)
}

/// Card memory the host reports for this process: (local usage, local budget, non-local usage), bytes.
pub fn video_memory() -> Option<(u64, u64, u64)> {
    use windows::Win32::Graphics::Dxgi::*;
    let d = crate::device::get()?;
    // SAFETY: plain DXGI calls on the device's own adapter
    unsafe {
        let luid = d.host.GetAdapterLuid();
        let f: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let a: IDXGIAdapter3 = f.EnumAdapterByLuid(luid).ok()?;
        let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut other = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local).ok()?;
        a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut other).ok()?;
        Some((local.CurrentUsage, local.Budget, other.CurrentUsage))
    }
}

/// One line: what card memory holds, by kind (`REBUILD_VRAM_LOG=<seconds>` prints it while the title runs).
pub fn vram_line() -> String {
    vram_line_with(true)
}

fn vram_line_with(census: bool) -> String {
    let mib = |b: u64| b >> 20;
    let mut s = String::from("vram      ");
    if let Some((used, budget, other)) = video_memory() {
        s += &format!(
            "local {} / {} MiB budget{}, non-local {} MiB; ",
            mib(used),
            mib(budget),
            if rebuild_d3d12x::vram::PRESSURE.load(Relaxed) { " (at the budget)" } else { "" },
            mib(other)
        );
    }
    if let (Some(m), Some(r)) = (MODEL.get(), pages_by_region()) {
        let heaps = lock(&m.tiles).space.heap_bytes();
        s += &format!(
            "tiles {} MiB of heaps, pages: title memory {} MiB, arena GPU-only {} MiB, arena CPU {} MiB; ",
            mib(heaps),
            mib(r[0] * P),
            mib(r[1] * P),
            mib(r[2] * P)
        );
    }
    if let Some((used, allowed)) = card_pages() {
        s += &format!("card pages {} / {} MiB allowed; ", used >> 20, allowed >> 20);
    }
    s += &format!("host textures {}", rebuild_d3d12x::vram::report());
    s += &format!("
vram      mapped textures: {}", crate::mapped::report());
    if crate::mapped::enabled() {
        s += &format!("
vram      {}; {} presents", crate::pagemap::report(), crate::present::PRESENT_CALLS.load(Relaxed));
    }
    if census {
        s += &census_line();
    }
    s
}

/// The live host textures by (origin, GPU-written, where their console memory is): count, host MiB, console MiB,
/// console MiB covered (the union: what the console pays), distinct start addresses.
fn census_line() -> String {
    use rebuild_d3d12x::vram::{live, union, Origin};
    let all = live();
    if all.is_empty() {
        return String::new();
    }
    let region = |a: u64| -> usize {
        if watched(a, 1) {
            0
        } else if crate::pagemap::source(a, 1).is_some() {
            2
        } else if MODEL.get().is_some_and(|m| lock(&m.maps).spans.range(..=a).next_back().is_some_and(|(&b, &l)| a < b + l)) {
            1
        } else {
            3
        }
    };
    const REGIONS: [&str; 4] = ["watched", "arena", "page-mapped", "other"];
    let mut groups: BTreeMap<(u8, bool, usize), Vec<rebuild_d3d12x::vram::Live>> = BTreeMap::new();
    for l in all {
        groups.entry((l.origin as u8, l.gpu_written, region(l.address))).or_default().push(l);
    }
    let name = |o: u8| match o {
        x if x == Origin::Committed as u8 => "committed",
        x if x == Origin::Placed as u8 => "placed",
        x if x == Origin::AtAddress as u8 => "at-address",
        x if x == Origin::PlacedView as u8 => "placed-view",
        _ => "twin",
    };
    let (gpu_pages, pool_pages) = crate::pagemap::mapped_now();
    let mut s = format!(
        "
vram      page-mapped now: {} MiB of GPU addresses over {} MiB of pool pages
vram      census (origin, written by: count, host MiB, console MiB, covered MiB, addresses, pool MiB under them):",
        (gpu_pages as u64 * P) >> 20,
        (pool_pages as u64 * P) >> 20
    );
    for ((o, gw, r), v) in groups {
        let host: u64 = v.iter().map(|l| l.bytes).sum();
        let console: u64 = v.iter().map(|l| l.console).sum();
        let covered = union(v.iter().map(|l| (l.address, l.address + l.console)).collect());
        let mut starts: Vec<u64> = v.iter().map(|l| l.address).collect();
        starts.sort_unstable();
        starts.dedup();
        let mut pool = std::collections::HashSet::new();
        if r == 2 {
            for l in &v {
                crate::pagemap::sources(l.address, l.console, &mut pool);
            }
        }
        s += &format!(
            "
          {} {} in {}: {}, {}, {}, {}, {}, {}",
            name(o),
            if gw { "GPU" } else { "CPU/copy" },
            REGIONS[r],
            v.len(),
            host >> 20,
            console >> 20,
            covered >> 20,
            starts.len(),
            (pool.len() as u64 * P) >> 20
        );
    }
    s
}

/// Card memory in use and the budget (bytes) at the watcher's last look.
pub static VRAM_USED: AtomicU64 = AtomicU64::new(0);
pub static VRAM_BUDGET: AtomicU64 = AtomicU64::new(0);
/// Times the card reached its budget (pressure on), over the run.
pub static PRESSURE_ON: AtomicU64 = AtomicU64::new(0);

/// Headroom kept under the budget: past `budget - ON`, pressure; back under `budget - OFF`, none.
const PRESSURE_ON_MARGIN: u64 = 384 << 20;
const PRESSURE_OFF_MARGIN: u64 = 768 << 20;

/// Pressure from usage and budget, with hysteresis (`was`: the state before).
pub fn pressure(used: u64, budget: u64, was: bool) -> bool {
    if budget == 0 {
        return false;
    }
    if was {
        used + PRESSURE_OFF_MARGIN > budget
    } else {
        used + PRESSURE_ON_MARGIN > budget
    }
}

/// Watch the card's budget (`QueryVideoMemoryInfo`, four times a second, from a thread of its own): near it, new
/// textures nothing renders to go to system memory (`rebuild_d3d12x::vram::PRESSURE`) and textures over page-mapped
/// memory take no new card pages (`crate::mapped`), so what the title uses stays on the card instead of the driver
/// paging it out (frames stall at 0 when that happens). `REBUILD_VRAM_BUDGET=0` turns it off.
pub fn start_budget_watch() {
    if std::env::var("REBUILD_VRAM_BUDGET").is_ok_and(|v| v.trim() == "0") {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("vram budget".into()).spawn(|| loop {
            if let Some((used, budget, _)) = video_memory() {
                VRAM_USED.store(used, Relaxed);
                VRAM_BUDGET.store(budget, Relaxed);
                let flag = &rebuild_d3d12x::vram::PRESSURE;
                let was = flag.load(Relaxed);
                let now = pressure(used, budget, was);
                if now != was {
                    flag.store(now, Relaxed);
                    if now && PRESSURE_ON.fetch_add(1, Relaxed) < 8 {
                        eprintln!("vram      at the budget: {} of {} MiB in use; new textures go to system memory, streamed ones take no new card pages", used >> 20, budget >> 20);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        });
    });
}

/// Print [`vram_line`] every `REBUILD_VRAM_LOG` seconds (off without it), from a thread of its own.
pub fn start_vram_log() {
    let Some(secs) = std::env::var("REBUILD_VRAM_LOG").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|s| *s > 0.0) else { return };
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("vram log".into()).spawn(move || {
            let started = std::time::Instant::now();
            for n in 1u64.. {
                std::thread::sleep(std::time::Duration::from_secs_f64(secs));
                // the census (a walk over every live texture) every tenth line
                eprintln!("[{:.1} s] {}", started.elapsed().as_secs_f64(), vram_line_with(n % 10 == 0));
            }
        });
    });
}

/// The run report's lines.
pub fn report() -> String {
    let Some(m) = MODEL.get() else {
        return if standin() { "memory    stand-in (REBUILD_MEMORY=standin)".into() } else { "memory    stand-in (the model did not start)".into() };
    };
    let t = m.ring.totals();
    let (n, bytes) = m.ring.regions();
    let (used, heaps) = {
        let tl = lock(&m.tiles);
        (tl.uma.card_pages_used(), tl.space.heap_bytes())
    };
    let passes = t.passes.load(Relaxed).max(1);
    let per = |a: &AtomicU64| a.load(Relaxed) as f64 / passes as f64 / 1000.0;
    let cold = rebuild_uma::cold::counters();
    format!(
        "memory    tiles: {n} watched regions ({} MiB), {used} card pages in use ({} MiB of heaps), {} sub-windows, {} views placed; pages placed {} ({} on first write), retired {}, zeroed {}, refused {}, map failures {}; regions added {}, removed {}\n\
         arena     {} MiB of resources at console addresses, {} refused; {} buffers sharing card pages by address; {} allocations, {} frees\n\
         upload    {} passes for {} flushes, {} MiB sent, {:.2} ms a pass, flush -> signalled {:.2} ms; carried {} pages, {} chunks with no sub-window, {} failed passes\n\
         upload    a pass, ms: region lock {:.2}, slot wait {:.2}, watch {:.2}, diff {:.2}, gather {:.2}, record {:.2}; after it, making slices cold {:.2}
         cold      {}: write-watch read {} MiB a pass, {} MiB of cold slices not read; slices made cold {} ({:.2} ms each), refused {}; write faults {} ({:.1} us each), kernel writes guarded {}, commits {}; check: {} pages found written in cold slices",
        bytes >> 20,
        heaps >> 20,
        m.subs.iter().filter(|s| !s.load(Relaxed).is_null()).count(),
        m.resources.get().map_or(0, |r| r.windows_placed()),
        PAGES_PLACED.load(Relaxed),
        PAGES_ON_WRITE.load(Relaxed),
        PAGES_RETIRED.load(Relaxed),
        PAGES_ZEROED.load(Relaxed),
        PAGES_REFUSED.load(Relaxed),
        MAP_FAILED.load(Relaxed),
        REGIONS_ADDED.load(Relaxed),
        REGIONS_REMOVED.load(Relaxed),
        ARENA_BYTES.load(Relaxed) >> 20,
        ARENA_REFUSED.load(Relaxed),
        ALIASED.load(Relaxed),
        ARENA_ALLOCS.load(Relaxed),
        ARENA_FREES.load(Relaxed),
        t.passes.load(Relaxed),
        t.flushes.load(Relaxed),
        t.bytes.load(Relaxed) >> 20,
        per(&t.busy_us),
        per(&t.latency_us),
        t.carried.load(Relaxed),
        t.homeless.load(Relaxed),
        t.failed.load(Relaxed),
        per(&t.lock_us),
        per(&t.slot_wait_us),
        per(&t.watch_us),
        per(&t.diff_us),
        per(&t.gather_us),
        per(&t.record_us),
        per(&t.cool_us),
        if rebuild_uma::cold::enabled() { "on" } else { "off" },
        t.watched_bytes.load(Relaxed) / passes >> 20,
        t.cold_bytes.load(Relaxed) / passes >> 20,
        cold.went_cold.load(Relaxed),
        cold.cold_us.load(Relaxed) as f64 / cold.went_cold.load(Relaxed).max(1) as f64 / 1000.0,
        cold.refused.load(Relaxed),
        cold.faults.load(Relaxed),
        cold.fault_us.load(Relaxed) as f64 / cold.faults.load(Relaxed).max(1) as f64,
        cold.kernel_writes.load(Relaxed),
        cold.commits.load(Relaxed),
        t.cold_dirty.load(Relaxed),
    )
}

impl Model {
    /// Sub-window `k`'s buffer, made (and its views placed) if it is not yet.
    fn ensure_sub(&self, t: &mut Tiles, k: usize) -> Option<()> {
        if !self.subs.get(k)?.load(Relaxed).is_null() {
            return Some(());
        }
        if let Err(e) = t.space.ensure(k) {
            if MAP_FAILED.fetch_add(1, Relaxed) < 4 {
                eprintln!("memory    sub-window {k}: {e}");
            }
            return None;
        }
        self.publish(t);
        Some(())
    }

    /// Record and give views to the sub-windows the space made since the last call.
    fn publish(&self, t: &mut Tiles) {
        for k in t.space.take_created() {
            let r = t.space.sub(k as usize).expect("made").clone();
            self.vas[k as usize].store(unsafe { r.GetGPUVirtualAddress() }, Relaxed);
            self.subs[k as usize].store(r.as_raw(), Relaxed);
            if self.resources.get().is_some() {
                self.place_views(k);
            } else {
                t.views_pending.push(k);
            }
        }
    }

    /// Card pages for the 64 KiB pages at `pages` (those without one yet).
    fn place_pages(&self, t: &mut Tiles, pages: impl Iterator<Item = u64>) {
        let before = t.uma.card_pages_reached();
        let mut mapped: Vec<Backed> = Vec::new();
        for page in pages {
            if t.pages.contains_key(&page) {
                continue;
            }
            match t.uma.place(page, PAGE) {
                Ok((id, m)) => {
                    t.pages.insert(page, id);
                    mapped.extend(m);
                }
                Err(e) => {
                    if PAGES_REFUSED.fetch_add(1, Relaxed) < 4 {
                        eprintln!("memory    page {page:#x} gets no card page: {e:?}");
                    }
                }
            }
        }
        if mapped.is_empty() {
            return;
        }
        if let Err(e) = t.space.map(&mapped) {
            if MAP_FAILED.fetch_add(1, Relaxed) < 4 {
                eprintln!("memory    mapping {} pages from {:#x} failed: {e}", mapped.len(), mapped[0].page as u64 * P);
            }
            return;
        }
        PAGES_PLACED.fetch_add(mapped.len() as u64, Relaxed);
        self.publish(t);
        // a reused card page holds what its last user left: zeroed (the CPU's new memory is zero) before any use
        for b in mapped.iter().filter(|b| b.card < before) {
            let address = b.page as u64 * P;
            let k = (address / SUB) as usize;
            self.ring.zero(self.vas[k].load(Relaxed) + address % SUB, PAGE as u32);
            PAGES_ZEROED.fetch_add(1, Relaxed);
        }
    }

    /// Give back the card pages of pages in `first..end` (64 KiB aligned).
    fn retire(&self, t: &mut Tiles, first: u64, end: u64) {
        let mut released = Vec::new();
        let mut page = first;
        while page < end {
            if let Some(id) = t.pages.remove(&page) {
                if let Ok(r) = t.uma.retire(id) {
                    released.extend(r);
                }
            }
            page += P;
        }
        PAGES_RETIRED.fetch_add(released.len() as u64, Relaxed);
        if !released.is_empty() {
            if let Err(e) = t.space.unmap(&released) {
                if MAP_FAILED.fetch_add(1, Relaxed) < 4 {
                    eprintln!("memory    unmapping {} pages from {first:#x} failed: {e}", released.len());
                }
            }
        }
    }

    fn place_views(&self, k: u32) {
        let (Some(res), Some(r)) = (self.resources.get(), self.subs.get(k as usize)) else { return };
        let raw = r.load(Relaxed);
        if raw.is_null() {
            return;
        }
        // SAFETY: the space holds this buffer for the run; borrowed here for the call
        if let Some(buffer) = unsafe { ID3D12Resource::from_raw_borrowed(&raw) } {
            res.place_window(k, buffer);
        }
    }
}

// ------------------------------------------------------------------------------------- the arena

/// Where d3d12x resources get console addresses (`docs/design/memory-run.md`): pieces of our own reservations from
/// 1 TiB, above the title's graphics memory, sub-window aligned. GPU-only memory is reserved and never committed
/// (card pages only); CPU-visible memory is committed in write-watched pieces the ring reads; READBACK memory is
/// its own committed allocation, which the GPU writes in place (`titlemem` opens it), with no card copy.
pub struct Arena;

const ARENA_LO: u64 = 1 << 40;
const ARENA_HI: u64 = (1 << 44) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Gpu = 0,
    Cpu = 1,
    Readback = 2,
}

#[derive(Default)]
struct ArenaState {
    /// free ranges of the GPU-only and the CPU-visible pieces: start -> length
    free: [BTreeMap<u64, u64>; 2],
    /// live allocations: address -> (bytes, kind)
    allocs: HashMap<u64, (u64, Kind)>,
    /// the arena's pieces: base -> (bytes, kind)
    pieces: BTreeMap<u64, (u64, Kind)>,
}

pub static ARENA_BYTES: AtomicU64 = AtomicU64::new(0);
pub static ARENA_REFUSED: AtomicU64 = AtomicU64::new(0);
pub static ALIASED: AtomicU64 = AtomicU64::new(0);
pub static ARENA_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static ARENA_FREES: AtomicU64 = AtomicU64::new(0);

fn kind_of(heap: &rebuild_d3d12x::format::HeapProperties) -> Kind {
    const UPLOAD: u32 = 2;
    const READBACK: u32 = 3;
    const CUSTOM: u32 = 4;
    match heap.kind {
        UPLOAD => Kind::Cpu,
        READBACK => Kind::Readback,
        // custom: CPU pages not available (1) is GPU memory; write-combine (2) and write-back (3) the CPU writes
        CUSTOM if heap.cpu_pages >= 2 => Kind::Cpu,
        _ => Kind::Gpu,
    }
}

fn reserve_at(size: u64, kind: VIRTUAL_ALLOCATION_TYPE, protect: PAGE_PROTECTION_FLAGS, align: u64) -> Option<u64> {
    for _ in 0..4 {
        let gap = crate::xmem::host::free_gap(ARENA_LO, ARENA_HI, size + align)?;
        let at = gap.next_multiple_of(align);
        let p = unsafe { VirtualAlloc2(None, Some(at as *const c_void), size as usize, kind, protect.0, None) };
        if !p.is_null() {
            return Some(p as u64);
        }
    }
    None
}

impl Model {
    /// A first-fit range of `bytes` at `align` in the pieces of `kind`, making a piece when none fits.
    fn arena_take(&self, maps: &mut Maps, kind: Kind, bytes: u64, align: u64) -> Option<u64> {
        let k = kind as usize;
        for round in 0..2 {
            let fit = maps.arena.free[k].iter().find_map(|(&start, &len)| {
                let at = start.next_multiple_of(align);
                (at + bytes <= start + len).then_some((start, len, at))
            });
            if let Some((start, len, at)) = fit {
                let free = &mut maps.arena.free[k];
                free.remove(&start);
                if at > start {
                    free.insert(start, at - start);
                }
                if at + bytes < start + len {
                    free.insert(at + bytes, start + len - at - bytes);
                }
                return Some(at);
            }
            if round == 1 {
                break;
            }
            // a new piece: whole sub-windows, so a piece never shares a window view with the title's memory
            let size = (bytes + align).next_multiple_of(SUB);
            let base = match kind {
                Kind::Gpu => reserve_at(size, MEM_RESERVE, PAGE_NOACCESS, SUB)?,
                _ => reserve_at(size, MEM_RESERVE | rebuild_uma::ring::MEM_WRITE_WATCH, PAGE_READWRITE, SUB)?,
            };
            if kind == Kind::Cpu {
                // SAFETY: write-watched and reserved for the run
                if let Err(e) = unsafe { self.ring.add(base as *mut u8, size as usize, base) } {
                    eprintln!("memory    arena piece {base:#x} not watched: {e}");
                    return None;
                }
                maps.watched.insert(base, size);
                REGIONS_ADDED.fetch_add(1, Relaxed);
            }
            maps.spans.insert(base, size);
            maps.arena.pieces.insert(base, (size, kind));
            maps.arena.free[k].insert(base, size);
        }
        None
    }

    fn arena_give(maps: &mut Maps, kind: Kind, at: u64, bytes: u64) {
        let free = &mut maps.arena.free[kind as usize];
        let (mut start, mut len) = (at, bytes);
        if let Some((&ps, &pl)) = free.range(..start).next_back() {
            if ps + pl == start {
                free.remove(&ps);
                start = ps;
                len += pl;
            }
        }
        if let Some(&nl) = free.get(&(start + len)) {
            free.remove(&(start + len));
            len += nl;
        }
        free.insert(start, len);
    }
}

/// `REBUILD_VRAM_LAZY=0`: textures and heaps get card pages at allocation again (for comparing runs). Read once.
pub fn lazy_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("REBUILD_VRAM_LAZY").map_or(true, |v| v.trim() != "0"))
}

/// Allocations made without card pages (textures, heaps), and their bytes.
pub static LAZY_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static LAZY_BYTES: AtomicU64 = AtomicU64::new(0);

impl rebuild_d3d12x::Memory for Arena {
    fn allocate(&self, bytes: u64, alignment: u64, heap: &rebuild_d3d12x::format::HeapProperties) -> Option<u64> {
        arena_allocate(bytes, alignment, heap, true)
    }

    fn allocate_lazy(&self, bytes: u64, alignment: u64, heap: &rebuild_d3d12x::format::HeapProperties) -> Option<u64> {
        let place = !lazy_on();
        let at = arena_allocate(bytes, alignment, heap, place)?;
        if !place {
            LAZY_ALLOCS.fetch_add(1, Relaxed);
            LAZY_BYTES.fetch_add(bytes, Relaxed);
        }
        Some(at)
    }

    fn free(&self, address: u64, _bytes: u64) {
        arena_free(address)
    }

    fn range(&self, va: u64) -> Option<MemoryRange> {
        range(va)
    }

    fn buffer(&self, address: u64, bytes: u64) -> Option<ID3D12Resource> {
        buffer(address, bytes)
    }
}

/// Console memory from the arena; `place`: its GPU-only pages get card pages now (a buffer the GPU may write before
/// the CPU does), else only when a buffer is made over them ([`buffer`]).
fn arena_allocate(bytes: u64, alignment: u64, heap: &rebuild_d3d12x::format::HeapProperties, place: bool) -> Option<u64> {
    {
        let m = MODEL.get()?;
        let kind = kind_of(heap);
        let bytes = bytes.max(1).next_multiple_of(P);
        let align = alignment.max(P);
        let mut maps = lock(&m.maps);
        let at = if kind == Kind::Readback {
            // its own allocation, committed: the GPU writes it in place, the CPU reads it
            reserve_at(bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE, align)
        } else {
            match m.arena_take(&mut maps, kind, bytes, align) {
                Some(at) if kind == Kind::Cpu && unsafe { VirtualAlloc(Some(at as *const c_void), bytes as usize, MEM_COMMIT, PAGE_READWRITE) }.is_null() => {
                    Model::arena_give(&mut maps, kind, at, bytes);
                    None
                }
                Some(at) => {
                    // a resource's memory gets its card pages now (the GPU may write it before the CPU does);
                    // CPU-visible memory always (the ring writes it), a texture's or a heap's only if asked
                    if place || kind == Kind::Cpu {
                        m.place_pages(&mut lock(&m.tiles), (at..at + bytes).step_by(PAGE));
                    }
                    Some(at)
                }
                None => None,
            }
        };
        let Some(at) = at else {
            if ARENA_REFUSED.fetch_add(1, Relaxed) < 4 {
                eprintln!("memory    arena: no {bytes:#x} bytes ({kind:?})");
            }
            return None;
        };
        maps.arena.allocs.insert(at, (bytes, kind));
        ARENA_BYTES.fetch_add(bytes, Relaxed);
        ARENA_ALLOCS.fetch_add(1, Relaxed);
        Some(at)
    }
}

fn arena_free(address: u64) {
    {
        let Some(m) = MODEL.get() else { return };
        let mut maps = lock(&m.maps);
        let Some((bytes, kind)) = maps.arena.allocs.remove(&address) else { return };
        ARENA_BYTES.fetch_sub(bytes, Relaxed);
        ARENA_FREES.fetch_add(1, Relaxed);
        match kind {
            Kind::Readback => unsafe {
                let _ = VirtualFree(address as *mut c_void, 0, MEM_RELEASE);
            },
            Kind::Cpu => {
                let _ = m.ring.quiet(address, bytes, || unsafe { VirtualFree(address as *mut c_void, bytes as usize, MEM_DECOMMIT) });
                m.retire(&mut lock(&m.tiles), address, address + bytes);
                Model::arena_give(&mut maps, kind, address, bytes);
            }
            Kind::Gpu => {
                m.retire(&mut lock(&m.tiles), address, address + bytes);
                Model::arena_give(&mut maps, kind, address, bytes);
            }
        }
    }
}

// ------------------------------------------------- card pages by key, for crate::mapped's textures

/// A range of `bytes` of console addresses of our own (the arena's GPU-only memory, no card pages): keys for card
/// pages nobody else names (`crate::mapped`).
pub(crate) fn key_range(bytes: u64) -> Option<u64> {
    let m = MODEL.get()?;
    let mut maps = lock(&m.maps);
    let at = m.arena_take(&mut maps, Kind::Gpu, bytes.next_multiple_of(P), P)?;
    maps.arena.allocs.insert(at, (bytes.next_multiple_of(P), Kind::Gpu));
    Some(at)
}

/// The card pages behind key pages `keys` (placed now where there are none), in one lock and at most one mapping
/// call. `None`: no card memory left (the budget). A key page is not mapped in its sub-window (nothing reads it
/// there: its card page is reached through the textures' tiles), except when its card page is reused, so the
/// ring can zero it as it does any reused page.
pub(crate) fn cards_for(keys: &[u64]) -> Vec<Option<u32>> {
    let Some(m) = MODEL.get() else { return vec![None; keys.len()] };
    let mut t = lock(&m.tiles);
    let before = t.uma.card_pages_reached();
    let mut reused: Vec<Backed> = Vec::new();
    let mut placed = 0u64;
    for &key in keys {
        if t.pages.contains_key(&key) {
            continue;
        }
        match t.uma.place(key, PAGE) {
            Ok((id, b)) => {
                t.pages.insert(key, id);
                placed += 1;
                reused.extend(b.into_iter().filter(|b| b.card < before));
            }
            Err(e) => {
                if PAGES_REFUSED.fetch_add(1, Relaxed) < 4 {
                    eprintln!("memory    key page {key:#x} gets no card page: {e:?}");
                }
            }
        }
    }
    let reached = t.uma.card_pages_reached();
    if reached > 0 {
        // the tile thread names heaps by index: they exist before it does
        let _ = t.space.heap((reached - 1) / rebuild_uma::tiles::HEAP_TILES);
    }
    if !reused.is_empty() {
        if t.space.map(&reused).is_ok() {
            m.publish(&mut t);
            for b in &reused {
                let address = b.page as u64 * P;
                t.keys_mapped.insert(address);
                let k = (address / SUB) as usize;
                m.ring.zero(m.vas[k].load(Relaxed) + address % SUB, PAGE as u32);
                PAGES_ZEROED.fetch_add(1, Relaxed);
            }
        } else if MAP_FAILED.fetch_add(1, Relaxed) < 4 {
            eprintln!("memory    mapping {} reused key pages failed", reused.len());
        }
    }
    PAGES_PLACED.fetch_add(placed, Relaxed);
    keys.iter().map(|&k| t.uma.backing(k)).collect()
}

/// Give back key pages' card pages (one unmapping call at most, for those mapped to be zeroed).
pub(crate) fn release_keys(keys: &[u64]) {
    let Some(m) = MODEL.get() else { return };
    let mut t = lock(&m.tiles);
    let mut unmap = Vec::new();
    for &key in keys {
        let Some(id) = t.pages.remove(&key) else { continue };
        let Ok(r) = t.uma.retire(id) else { continue };
        PAGES_RETIRED.fetch_add(r.len() as u64, Relaxed);
        if t.keys_mapped.remove(&key) {
            unmap.extend(r);
        }
    }
    if !unmap.is_empty() {
        if let Err(e) = t.space.unmap(&unmap) {
            if MAP_FAILED.fetch_add(1, Relaxed) < 4 {
                eprintln!("memory    unmapping {} key pages failed: {e}", unmap.len());
            }
        }
    }
}

/// Tile heap `h` (made if it is not yet), for the tile thread (`crate::tilequeue`).
pub(crate) fn tile_heap(h: u32) -> Option<ID3D12Heap> {
    let m = MODEL.get()?;
    lock(&m.tiles).space.heap(h).ok()
}

/// Console address `a` lies in memory the model knows (watched, or the arena).
pub(crate) fn known(a: u64) -> bool {
    let Some(m) = MODEL.get() else { return false };
    let maps = lock(&m.maps);
    maps.spans.range(..=a).next_back().is_some_and(|(&b, &l)| a < b + l)
}

/// Card pages in use and allowed, bytes.
pub fn card_pages() -> Option<(u64, u64)> {
    let m = MODEL.get()?;
    let t = lock(&m.tiles);
    Some((t.uma.card_pages_used() as u64 * P, t.uma.card_pages() as u64 * P))
}

/// A reserved buffer whose byte 0 is console `address`, on the same card pages (sharing by address): what a
/// resource at that address copies and barriers name. Its pages get card pages now if they have none (the GPU may
/// write a resource the CPU never did). `None` off watched memory and the arena, or off a 64 KiB boundary.
pub fn buffer(address: u64, bytes: u64) -> Option<ID3D12Resource> {
    let m = MODEL.get()?;
    if address % P != 0 {
        return None;
    }
    let len = bytes.max(1).next_multiple_of(P);
    {
        let maps = lock(&m.maps);
        let (&b, &l) = maps.spans.range(..=address).next_back()?;
        if address + len > b + l {
            return None;
        }
    }
    let mut t = lock(&m.tiles);
    m.place_pages(&mut t, (address..address + len).step_by(PAGE));
    let cards: Vec<u32> = (0..len / P).map(|i| t.uma.backing(address + i * P)).collect::<Option<Vec<u32>>>()?;
    let mut r: Option<ID3D12Resource> = None;
    unsafe { t.space.device.CreateReservedResource(&rebuild_uma::tiles::buffer_desc(len), D3D12_RESOURCE_STATE_COMMON, None, &mut r) }.ok()?;
    let r = r?;
    if let Err(e) = t.space.alias(&r, &cards) {
        if MAP_FAILED.fetch_add(1, Relaxed) < 4 {
            eprintln!("memory    a buffer at {address:#x} could not share its card pages: {e}");
        }
        return None;
    }
    ALIASED.fetch_add(1, Relaxed);
    Some(r)
}

#[cfg(test)]
mod budget_tests {
    use super::pressure;

    #[test]
    fn pressure_starts_near_the_budget_and_ends_well_under_it() {
        const M: u64 = 1 << 20;
        let budget = 11_000 * M;
        assert!(!pressure(9_000 * M, budget, false));
        assert!(pressure(10_700 * M, budget, false), "within 384 MiB of the budget");
        // hysteresis: stays on until 768 MiB under
        assert!(pressure(10_400 * M, budget, true));
        assert!(!pressure(10_200 * M, budget, true));
        assert!(!pressure(1, 0, true), "no budget known: no pressure");
    }
}
