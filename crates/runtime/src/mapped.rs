//! Textures over page-mapped GPU memory: reserved host textures on card pages keyed by the pool page under them.
//!
//! **Off by default** (`REBUILD_MAPPED_TEXTURES=1` turns it on): a card run with it on hung the GPU; see [`enabled`].
//!
//! The title streams textures the console way (queue 0xf8, `crate::pagemap`): it reserves GPU addresses, makes
//! textures over them (resources at an address, 0x428, and placed views, 0x230), and maps 64 KiB pages of a pool of
//! physical pages at those addresses as data streams in, unmapping them when it streams out. On the console a texture
//! costs the pool pages mapped under it, and nothing while unmapped.
//!
//! Before this, each such texture was a committed host texture of its full size, mapped or not, and a placed view's
//! texture lived for the run. A world load made ~48,000 of them: 19.7 GB of host textures over 1.5 GB of pool pages,
//! and the card ran out (`E_OUTOFMEMORY`, the title's assert at `exe+0x193e00f`).
//!
//! Here such a texture is a **reserved** host texture. Each of its host tiles stands for one 64 KiB page of its
//! console footprint (mip by mip, slice by slice, from `rebuild_xg`'s layout of the console texture), and is mapped
//! to the card page of the **pool page** that console page is mapped to now, or left unmapped (reads zero) while the
//! console page is unmapped. Two textures over the same pool pages share card pages, as on the console. A host tile
//! with no console page of its own (the host's layout is larger there) gets a card page of its own.
//!
//! ```text
//! d3d12x makes a texture       Memory::texture -> texture(): reserved, tiles mapped to what is mapped now
//! the title maps or unmaps     0xf8            -> remapped(): the tiles over those pages follow
//! the texture goes             texture_gone    -> gone(): its card pages are given back when nothing else names them
//! ```
//!
//! Tile mappings go through `crate::tilequeue`: one `UpdateTileMappings` per texture per creation or 0xf8 batch
//! (per heap its card pages are in), contiguous tiles as one range, tiles staying on their pool page skipped, on a
//! queue and thread of their own; the title's next submission waits for them on the GPU. Card pages for a batch's
//! new keys come from the memory model in one call (`memory::cards_for`), and go back in one (`release_keys`).
//!
//! Contents still come from `crate::cputex` (the detile pass), which copies into the host texture when its pages
//! change. Card pages come from the memory model's budget (`REBUILD_TILE_MIB`): when it is spent, a tile stays
//! unmapped (reads zero) instead of the title getting an out-of-memory.
//!
//! **Locks**: this module's state, then the memory model's (`crate::memory`); the model never calls back here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};

use crate::tilequeue::{Job, TexTile};

const P: u64 = 64 << 10;

/// Textures made reserved, refused by reason, and their tiles.
pub static MADE: AtomicU64 = AtomicU64::new(0);
pub static GONE: AtomicU64 = AtomicU64::new(0);
pub static NOT_ELIGIBLE: AtomicU64 = AtomicU64::new(0);
pub static UNALIGNED: AtomicU64 = AtomicU64::new(0);
pub static NO_LAYOUT: AtomicU64 = AtomicU64::new(0);
pub static CREATE_FAILED: AtomicU64 = AtomicU64::new(0);
pub static TILES: AtomicU64 = AtomicU64::new(0);
pub static PRIVATE: AtomicU64 = AtomicU64::new(0);
pub static NO_CARD: AtomicU64 = AtomicU64::new(0);
/// Tiles left unmapped because the card was at its budget.
pub static HELD_BACK: AtomicU64 = AtomicU64::new(0);
/// Textures left to the caller (system memory) because the card was at its budget.
pub static AT_BUDGET: AtomicU64 = AtomicU64::new(0);
pub static REMAPS: AtomicU64 = AtomicU64::new(0);
pub static MAP_FAILED: AtomicU64 = AtomicU64::new(0);
/// 0xf8 batches seen, tiles over the pages they touched, and of those: onto the pool page they had already, and
/// unmapped and still unmapped (both skipped; `REMAPS` is the rest)
pub static BATCHES: AtomicU64 = AtomicU64::new(0);
pub static TOUCHED: AtomicU64 = AtomicU64::new(0);
pub static SAME: AtomicU64 = AtomicU64::new(0);
pub static STILL_UNMAPPED: AtomicU64 = AtomicU64::new(0);
/// Small textures placed at 4 KiB in heaps of our own: made, and bytes live.
pub static SMALL: AtomicU64 = AtomicU64::new(0);
pub static SMALL_BYTES: AtomicU64 = AtomicU64::new(0);

/// Bytes of each heap small textures are placed in.
const SMALL_HEAP: u64 = 64 << 20;

/// **Off unless `REBUILD_MAPPED_TEXTURES=1`.** On the card (RTX 4070 Ti, run `fix4`, 2026-11-28) the world load with
/// this path on stalled the GPU ~60 s after A (frames stop at ~106 s, device hung 0x887A0006, two nvlddmkm resets)
/// while card memory was 9.8 of 11.0 GB, so not for want of memory. The suspect is the volume of tile mapping
/// updates on the ring's queue (~120,000 `UpdateTileMappings` calls in ~20 s: every creation and every 0xf8 remap),
/// which every title submission waits on. Not to be turned on by default until a run shows it does not hang.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("REBUILD_MAPPED_TEXTURES").is_ok_and(|v| v.trim() == "1"))
}

/// What a host tile stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// the console page at this GPU address
    Console(u64),
    /// none: a card page of its own
    Private,
}

struct Tex {
    resource: ID3D12Resource,
    address: u64,
    bytes: u64,
    /// per host tile, in the texture's tile order: its coordinate, what it stands for, and the key page it is
    /// mapped to now (`None`: unmapped)
    tiles: Vec<(TexTile, Source, Option<u64>)>,
}

// SAFETY: D3D12 resources are free-threaded; the rest is plain data behind the state lock.
unsafe impl Send for Tex {}

#[derive(Default)]
struct State {
    /// by host resource pointer
    textures: HashMap<usize, Tex>,
    /// console start address -> host resource pointers
    by_address: BTreeMap<u64, Vec<usize>>,
    /// the longest console footprint (for overlap queries)
    longest: u64,
    /// key page -> textures' tiles naming it
    refs: HashMap<u64, u32>,
    /// pool CPU base -> (pool pages, key base)
    pools: BTreeMap<u64, (u64, u64)>,
    /// key pages of our own (private tiles, and pool pages seen from inside: below): free ones, and the rest of the
    /// current range
    private_free: Vec<u64>,
    private_next: u64,
    private_end: u64,
    /// a texture that starts inside a 64 KiB page (4 KiB swizzles pack small textures): its tiles are keyed by the
    /// pool page *and* the start's offset in it, so textures at one address share card pages (a slot streamed into
    /// again) and neighbours in one page do not
    inner: HashMap<(u64, u64), u64>,
    /// keys of our own -> what they stand for (an `inner` key, or none: a private tile)
    own: HashMap<u64, Option<(u64, u64)>>,
    /// small textures (64 KiB or less on the host) that start inside a page: placed at 4 KiB in heaps of our own,
    /// so one costs its size, not a whole 64 KiB tile. Heaps, the next free byte of the last, free slots by size,
    /// and the live ones (host pointer -> heap, offset, size)
    small_heaps: Vec<ID3D12Heap>,
    small_next: u64,
    small_free: HashMap<u64, Vec<(usize, u64)>>,
    small_live: HashMap<usize, (usize, u64, u64)>,
}

// SAFETY: D3D12 heaps are free-threaded; the state is behind its lock.
unsafe impl Send for State {}

impl State {
    /// A slot of `size` bytes (a multiple of 4 KiB, at most 64 KiB) in a small-texture heap.
    fn small_slot(&mut self, device: &ID3D12Device, size: u64) -> Option<(usize, u64)> {
        if let Some(slot) = self.small_free.get_mut(&size).and_then(Vec::pop) {
            return Some(slot);
        }
        if self.small_heaps.is_empty() || self.small_next + size > SMALL_HEAP {
            let mut h: Option<ID3D12Heap> = None;
            // at the card's budget, a new heap is system memory (read over the bus, never paged)
            let properties = if rebuild_d3d12x::vram::PRESSURE.load(Relaxed) {
                D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_CUSTOM, CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE, MemoryPoolPreference: D3D12_MEMORY_POOL_L0, ..Default::default() }
            } else {
                D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }
            };
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: SMALL_HEAP,
                Properties: properties,
                Alignment: P,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
            };
            // SAFETY: a description and an out-parameter
            unsafe { device.CreateHeap(&desc, &mut h) }.ok()?;
            self.small_heaps.push(h?);
            self.small_next = 0;
        }
        let at = self.small_next;
        self.small_next += size;
        Some((self.small_heaps.len() - 1, at))
    }
}

/// A small texture (`host` takes 64 KiB or less at 4 KiB alignment) placed in a small-texture heap.
fn small(s: &mut State, device: &ID3D12Device, host: &D3D12_RESOURCE_DESC, layout: D3D12_BARRIER_LAYOUT) -> Option<ID3D12Resource> {
    let mut d = *host;
    d.Alignment = 4096;
    // SAFETY: one description
    let info = unsafe { device.GetResourceAllocationInfo(0, std::slice::from_ref(&d)) };
    if info.Alignment != 4096 || info.SizeInBytes == 0 || info.SizeInBytes > P {
        return None;
    }
    let size = info.SizeInBytes.next_multiple_of(4096);
    let (heap, offset) = s.small_slot(device, size)?;
    let d1 = D3D12_RESOURCE_DESC1 {
        Dimension: d.Dimension,
        Alignment: d.Alignment,
        Width: d.Width,
        Height: d.Height,
        DepthOrArraySize: d.DepthOrArraySize,
        MipLevels: d.MipLevels,
        Format: d.Format,
        SampleDesc: d.SampleDesc,
        Layout: d.Layout,
        Flags: d.Flags,
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    let made = match device.cast::<ID3D12Device10>() {
        // SAFETY: a heap of ours, an offset inside it, a description and an out-parameter
        Ok(d10) => unsafe { d10.CreatePlacedResource2(&s.small_heaps[heap], offset, &d1, layout, None, None, &mut r) },
        Err(_) => unsafe { device.CreatePlacedResource(&s.small_heaps[heap], offset, &d, D3D12_RESOURCE_STATE_COMMON, None, &mut r) },
    };
    let Some(r) = made.ok().and(r) else {
        s.small_free.entry(size).or_default().push((heap, offset));
        return None;
    };
    s.small_live.insert(r.as_raw() as usize, (heap, offset, size));
    SMALL.fetch_add(1, Relaxed);
    SMALL_BYTES.fetch_add(size, Relaxed);
    Some(r)
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The title registered or used page pool `pool` of `pages` pages (0xf8 names it in every batch).
pub fn pool(pool: u64, pages: u32) {
    if !enabled() || pool == 0 || pages == 0 {
        return;
    }
    let mut g = state();
    let s = g.get_or_insert_with(State::default);
    if s.pools.get(&pool).is_some_and(|p| p.0 >= u64::from(pages)) {
        return;
    }
    // keys for this pool's pages: a range of our own console addresses, one key page per pool page
    if let Some(base) = crate::memory::key_range(u64::from(pages) * P) {
        s.pools.insert(pool, (u64::from(pages), base));
    }
}

impl State {
    /// The pool page (its CPU address) under console GPU page `va` now, if it is in a pool we know, and its key
    /// page when tiles start at the page's start.
    fn pool_page(&self, va: u64) -> Option<(u64, u64)> {
        let cpu = crate::pagemap::page(va)?;
        let (&base, &(pages, key)) = self.pools.range(..=cpu).next_back()?;
        let off = cpu - base;
        (off < pages * P).then_some((cpu, key + off / P * P))
    }

    /// The key page a tile standing for `src` of a texture starting `inside` bytes into a page names now. `None`:
    /// unmapped (or no key left).
    fn key_of(&mut self, src: Source, inside: u64) -> Option<u64> {
        match src {
            Source::Console(va) => match self.pool_page(va) {
                None => None,
                Some((_, key)) if inside == 0 => Some(key),
                Some((cpu, _)) => match self.inner.get(&(cpu, inside)) {
                    Some(&k) => Some(k),
                    None => {
                        let k = self.private_key()?;
                        self.inner.insert((cpu, inside), k);
                        self.own.insert(k, Some((cpu, inside)));
                        Some(k)
                    }
                },
            },
            Source::Private => {
                PRIVATE.fetch_add(1, Relaxed);
                let k = self.private_key()?;
                self.own.insert(k, None);
                Some(k)
            }
        }
    }

    /// Tiles come to name `keys` (one entry per tile; `None`: unmapped): their card pages, from one call to the
    /// memory model. One more tile names each key with a card page; a key left without one (the budget) is
    /// cleared to `None` and the tile stays unmapped (reads zero) until it is mapped again.
    fn take(&mut self, keys: &mut [Option<u64>]) -> Vec<Option<u32>> {
        let pressure = rebuild_d3d12x::vram::PRESSURE.load(Relaxed);
        let mut ask: Vec<u64> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::new();
        for &k in keys.iter().flatten() {
            if !seen.insert(k) {
                continue;
            }
            // at the card's budget, no new card pages
            if pressure && !self.refs.contains_key(&k) {
                HELD_BACK.fetch_add(1, Relaxed);
                continue;
            }
            ask.push(k);
        }
        let cards: HashMap<u64, u32> = if ask.is_empty() {
            HashMap::new()
        } else {
            ask.iter().copied().zip(crate::memory::cards_for(&ask)).filter_map(|(k, c)| c.map(|c| (k, c))).collect()
        };
        keys.iter_mut()
            .map(|k| match k.and_then(|key| cards.get(&key).map(|&c| (key, c))) {
                Some((key, c)) => {
                    *self.refs.entry(key).or_insert(0) += 1;
                    Some(c)
                }
                None => {
                    if let Some(key) = k.take() {
                        NO_CARD.fetch_add(1, Relaxed);
                        if !self.refs.contains_key(&key) {
                            self.forget(key);
                        }
                    }
                    None
                }
            })
            .collect()
    }

    /// One tile less names each of `keys`: card pages nothing names any more go back, in one call.
    fn drop_keys(&mut self, keys: &[u64]) {
        let mut gone = Vec::new();
        for &key in keys {
            let Some(n) = self.refs.get_mut(&key) else { continue };
            *n -= 1;
            if *n == 0 {
                self.refs.remove(&key);
                self.forget(key);
                gone.push(key);
            }
        }
        if !gone.is_empty() {
            crate::memory::release_keys(&gone);
        }
    }

    /// A key of our own nothing names any more goes back to the free list.
    fn forget(&mut self, key: u64) {
        if let Some(what) = self.own.remove(&key) {
            if let Some(k) = what {
                self.inner.remove(&k);
            }
            self.private_free.push(key);
        }
    }

    fn private_key(&mut self) -> Option<u64> {
        if let Some(k) = self.private_free.pop() {
            return Some(k);
        }
        if self.private_next >= self.private_end {
            const CHUNK: u64 = 256 << 20;
            let base = crate::memory::key_range(CHUNK)?;
            self.private_next = base;
            self.private_end = base + CHUNK;
        }
        let k = self.private_next;
        self.private_next += P;
        Some(k)
    }
}

/// GPU-only console memory: not watched, not the arena, not memory the CPU has committed (the GPU addresses the
/// title maps pool pages at).
fn gpu_addresses(address: u64) -> bool {
    if crate::memory::watched(address, 1) || crate::memory::known(address) {
        return false;
    }
    let mut m = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a query of our own address space
    let n = unsafe { VirtualQuery(Some(address as *const std::ffi::c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
    n == 0 || m.State != MEM_COMMIT
}

/// Per host subresource (mip + slice * mips), the console byte range `[start, end)` from the texture's address, and
/// for packed host mips per slice, the range of the console's mips from the first packed one.
struct Console {
    mips: u32,
    slices: u32,
    /// (mip, slice) -> (start, end)
    ranges: Vec<(u64, u64)>,
}

fn console_layout(console: &D3D12_RESOURCE_DESC) -> Option<Console> {
    let c = rebuild_xg::Computer::new(&rebuild_d3d12x::format::xg_desc(console)).ok()?;
    if c.texture.planes.len() != 1 {
        return None;
    }
    let plane = &c.texture.planes[0];
    let l = &plane.layout;
    if l.thick || console.Dimension.0 != rebuild_d3d12x::format::DIMENSION_TEXTURE2D {
        return None;
    }
    let mips = l.mips.len() as u32;
    // each mip's start in a slice: the tail's mips share the tail block
    let ft = l.first_tail_mip as usize;
    let start: Vec<u64> = (0..l.mips.len()).map(|m| if m >= ft && ft < l.mips.len() { l.mips[ft].offset } else { l.mips[m].offset }).collect();
    let mut bounds: Vec<u64> = start.clone();
    bounds.push(l.slice_size);
    bounds.sort_unstable();
    bounds.dedup();
    let end_of = |s: u64| bounds.iter().copied().find(|&b| b > s).unwrap_or(l.slice_size).min(l.slice_size);
    let mut ranges = Vec::with_capacity((mips * l.slices) as usize);
    for s in 0..l.slices {
        let base = plane.offset + u64::from(s) * l.slice_size;
        for m in 0..mips as usize {
            ranges.push((base + start[m], base + end_of(start[m]).max(start[m] + 1)));
        }
    }
    Some(Console { mips, slices: l.slices, ranges })
}

/// The host tiles of reserved texture `r` (whose console texture is `con`, at `address`), each with what it stands
/// for. `None` if the host's subresources do not match the console's.
fn plan(device: &ID3D12Device, r: &ID3D12Resource, desc: &D3D12_RESOURCE_DESC, con: &Console, address: u64) -> Option<Vec<(TexTile, Source)>> {
    let mips = u32::from(desc.MipLevels.max(1));
    let slices = u32::from(desc.DepthOrArraySize.max(1));
    if mips != con.mips || slices != con.slices {
        return None;
    }
    let mut total = 0u32;
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let mut shape = D3D12_TILE_SHAPE::default();
    let mut n = mips * slices;
    let mut subs = vec![D3D12_SUBRESOURCE_TILING::default(); n as usize];
    // SAFETY: room for `n` tilings
    unsafe { device.GetResourceTiling(r, Some(&mut total), Some(&mut packed), Some(&mut shape), Some(&mut n), 0, subs.as_mut_ptr()) };
    let standard = u32::from(packed.NumStandardMips);
    let mut used: HashSet<u64> = HashSet::new();
    let mut out: Vec<(TexTile, Source)> = Vec::with_capacity(total as usize);
    // console pages of `[start, end)` from the texture's address, each claimed once in this texture
    let pages_of = |range: (u64, u64)| -> Vec<u64> {
        let first = (address + range.0) / P * P;
        let end = (address + range.1).div_ceil(P) * P;
        (first..end).step_by(P as usize).collect()
    };
    let mut claim = |pages: &[u64], k: usize| -> Source {
        match pages.get(k) {
            Some(&p) if used.insert(p) => Source::Console(p),
            _ => Source::Private,
        }
    };
    for s in 0..slices {
        for m in 0..standard.min(mips) {
            let i = (m + s * mips) as usize;
            let t = subs[i];
            let pages = pages_of(con.ranges[i]);
            let mut k = 0usize;
            for z in 0..u32::from(t.DepthInTiles) {
                for y in 0..u32::from(t.HeightInTiles) {
                    for x in 0..t.WidthInTiles {
                        let index = x + y * t.WidthInTiles + z * t.WidthInTiles * u32::from(t.HeightInTiles);
                        out.push((TexTile { sub: i as u32, coord: [x, y, z], index, card: None }, claim(&pages, k)));
                        k += 1;
                    }
                }
            }
        }
        if packed.NumPackedMips > 0 && standard < mips {
            // the console's mips from the first packed one, as one range
            let lo = (standard..mips).map(|m| con.ranges[(m + s * mips) as usize].0).min().unwrap_or(0);
            let hi = (standard..mips).map(|m| con.ranges[(m + s * mips) as usize].1).max().unwrap_or(lo + 1);
            let pages = pages_of((lo, hi));
            let sub = standard + s * mips;
            for j in 0..packed.NumTilesForPackedMips {
                out.push((TexTile { sub, coord: [j, 0, 0], index: j, card: None }, claim(&pages, j as usize)));
            }
        }
    }
    (out.len() as u32 == total).then_some(out)
}

/// `Memory::texture`: a reserved host texture for console texture `console` at `address` (host description `host`,
/// starting in barrier layout `layout`), if it is one of the textures this module keeps; `None` otherwise (the
/// caller makes a committed one).
pub fn texture(address: u64, console: &D3D12_RESOURCE_DESC, host: &D3D12_RESOURCE_DESC, layout: D3D12_BARRIER_LAYOUT) -> Option<ID3D12Resource> {
    if !enabled() || crate::memory::get().is_none() {
        return None;
    }
    const WRITTEN: i32 = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
    let eligible = host.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && host.SampleDesc.Count <= 1
        && host.Flags.0 & WRITTEN == 0
        && console.Flags.0 & WRITTEN == 0
        && gpu_addresses(address);
    if !eligible {
        NOT_ELIGIBLE.fetch_add(1, Relaxed);
        return None;
    }
    if address % P != 0 {
        // starts inside a page: kept apart from its neighbours there by the start's offset (`State::inner`)
        UNALIGNED.fetch_add(1, Relaxed);
    }
    let Some(con) = console_layout(console) else {
        NO_LAYOUT.fetch_add(1, Relaxed);
        return None;
    };
    let d = crate::device::get()?;
    let device = &d.host;
    // tile mappings go on their own queue; without one, the caller makes a committed texture
    let queue = crate::tilequeue::get()?;
    if address % P != 0 {
        // small textures packed into one page: placed at 4 KiB rather than a 64 KiB tile each
        let mut g = state();
        if let Some(r) = small(g.get_or_insert_with(State::default), device, host, layout) {
            return Some(r);
        }
    }
    if rebuild_d3d12x::vram::PRESSURE.load(Relaxed) {
        // at the card's budget: the caller's committed texture goes to system memory (`rebuild_d3d12x::vram`)
        AT_BUDGET.fetch_add(1, Relaxed);
        return None;
    }
    let mut rd = *host;
    rd.Layout = D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE;
    rd.Alignment = 0;
    let mut r: Option<ID3D12Resource> = None;
    let made = match device.cast::<ID3D12Device10>() {
        // SAFETY: a description and an out-parameter
        Ok(d10) => unsafe { d10.CreateReservedResource2(&rd, layout, None, None, None, &mut r) },
        Err(_) => unsafe { device.CreateReservedResource(&rd, D3D12_RESOURCE_STATE_COMMON, None, &mut r) },
    };
    let Some(r) = made.ok().and(r) else {
        if CREATE_FAILED.fetch_add(1, Relaxed) < 4 {
            eprintln!("mapped    no reserved texture for {}x{}x{} format {} mips {} at {address:#x}", rd.Width, rd.Height, rd.DepthOrArraySize, rd.Format.0, rd.MipLevels);
        }
        return None;
    };
    let Some(plan) = plan(device, &r, &rd, &con, address) else {
        NO_LAYOUT.fetch_add(1, Relaxed);
        return None;
    };
    let bytes = con.ranges.iter().map(|x| x.1).max().unwrap_or(P);
    let mut g = state();
    let s = g.get_or_insert_with(State::default);
    let inside = address % P;
    let mut keys: Vec<Option<u64>> = plan.iter().map(|&(_, src)| s.key_of(src, inside)).collect();
    let cards = s.take(&mut keys);
    let mut tiles = Vec::with_capacity(plan.len());
    // a reserved texture starts unmapped: only tiles with a card page are mapped
    let mut map = Vec::new();
    for (((mut t, src), key), card) in plan.into_iter().zip(keys).zip(cards) {
        t.card = card;
        if card.is_some() {
            map.push(t);
        }
        tiles.push((t, src, key));
    }
    if !map.is_empty() {
        queue.submit(vec![Job { resource: r.clone(), tiles: map }]);
    }
    TILES.fetch_add(tiles.len() as u64, Relaxed);
    MADE.fetch_add(1, Relaxed);
    let raw = r.as_raw() as usize;
    s.longest = s.longest.max(bytes);
    s.by_address.entry(address).or_default().push(raw);
    s.textures.insert(raw, Tex { resource: r.clone(), address, bytes, tiles });
    Some(r)
}

/// A texture made here is gone: its tiles' card pages go back when nothing else names them.
pub fn gone(host: &ID3D12Resource) {
    let raw = host.as_raw() as usize;
    let mut g = state();
    let Some(s) = g.as_mut() else { return };
    if let Some((heap, offset, size)) = s.small_live.remove(&raw) {
        s.small_free.entry(size).or_default().push((heap, offset));
        SMALL_BYTES.fetch_sub(size, Relaxed);
        GONE.fetch_add(1, Relaxed);
        return;
    }
    let Some(t) = s.textures.remove(&raw) else { return };
    if let Some(v) = s.by_address.get_mut(&t.address) {
        v.retain(|&x| x != raw);
        if v.is_empty() {
            s.by_address.remove(&t.address);
        }
    }
    let keys: Vec<u64> = t.tiles.iter().filter_map(|x| x.2).collect();
    s.drop_keys(&keys);
    GONE.fetch_add(1, Relaxed);
}

/// The title mapped or unmapped GPU pages `pages` (sorted): the tiles over them follow.
pub fn remapped(pages: &[u64]) {
    if pages.is_empty() || !enabled() {
        return;
    }
    let mut g = state();
    let Some(s) = g.as_mut() else { return };
    if s.textures.is_empty() {
        return;
    }
    let (lo, hi) = (pages[0], pages[pages.len() - 1] + P);
    let candidates: Vec<usize> = s
        .by_address
        .range(lo.saturating_sub(s.longest)..hi)
        .flat_map(|(_, v)| v.iter().copied())
        .collect();
    let touched: HashSet<u64> = pages.iter().copied().collect();
    BATCHES.fetch_add(1, Relaxed);
    // per texture over the pages: (tile, the key it names now) for the tiles whose key changes
    let mut work: Vec<(usize, Vec<(usize, Option<u64>)>)> = Vec::new();
    for raw in candidates {
        let Some(t) = s.textures.get(&raw) else { continue };
        if t.address + t.bytes <= lo || t.address >= hi {
            continue;
        }
        let inside = t.address % P;
        let over: Vec<(usize, Source, Option<u64>)> = t
            .tiles
            .iter()
            .enumerate()
            .filter(|(_, x)| matches!(x.1, Source::Console(va) if touched.contains(&va)))
            .map(|(i, x)| (i, x.1, x.2))
            .collect();
        let mut list = Vec::new();
        for (i, src, old) in over {
            TOUCHED.fetch_add(1, Relaxed);
            let new = s.key_of(src, inside);
            if new == old {
                // onto the pool page it has already, or unmapped and still unmapped: nothing to do
                if new.is_some() { SAME.fetch_add(1, Relaxed) } else { STILL_UNMAPPED.fetch_add(1, Relaxed) };
                continue;
            }
            list.push((i, new));
        }
        if !list.is_empty() {
            work.push((raw, list));
        }
    }
    if work.is_empty() {
        return;
    }
    // the new keys' card pages in one call, taken before the old keys are given back (a key named on both sides
    // keeps its card page)
    let mut keys: Vec<Option<u64>> = work.iter().flat_map(|(_, l)| l.iter().map(|x| x.1)).collect();
    let cards = s.take(&mut keys);
    let mut olds: Vec<u64> = Vec::new();
    let mut jobs: Vec<Job> = Vec::with_capacity(work.len());
    let mut k = 0usize;
    for (raw, list) in work {
        let Some(t) = s.textures.get_mut(&raw) else { continue };
        let mut changed = Vec::with_capacity(list.len());
        for (i, _) in list {
            let (mut tile, src, old) = t.tiles[i];
            let (new, card) = (keys[k], cards[k]);
            k += 1;
            olds.extend(old);
            if new.is_none() && old.is_none() {
                continue;
            }
            tile.card = card;
            t.tiles[i] = (tile, src, new);
            changed.push(tile);
        }
        if !changed.is_empty() {
            REMAPS.fetch_add(changed.len() as u64, Relaxed);
            jobs.push(Job { resource: t.resource.clone(), tiles: changed });
        }
    }
    s.drop_keys(&olds);
    // one submit (one fence value) for the whole batch, one job per texture
    if let Some(q) = crate::tilequeue::get() {
        q.submit(jobs);
    }
}

/// For the VRAM line and the run report.
pub fn report() -> String {
    let (live, keys, pools, heaps) = state().as_ref().map_or((0, 0, 0, 0), |s| (s.textures.len(), s.refs.len(), s.pools.len(), s.small_heaps.len()));
    let queue = if MADE.load(Relaxed) > 0 { crate::tilequeue::get().map(crate::tilequeue::report) } else { None };
    format!(
        "{} small textures placed ({} MiB live in {} heaps); {} reserved textures live ({} made, {} gone) over {} pools, {} card pages ({} MiB) under them; tiles {} ({} private), {} without a card page ({} held back at the budget); {} 0xf8 batches: {} tiles over their pages, {} onto the pool page they had, {} still unmapped, {} remapped; {} starting inside a page; not taken: {} at the budget, {} not eligible, {} no layout, {} not made, {} mapping failures; tile queue: {}",
        SMALL.load(Relaxed),
        SMALL_BYTES.load(Relaxed) >> 20,
        heaps,
        live,
        MADE.load(Relaxed),
        GONE.load(Relaxed),
        pools,
        keys,
        (keys as u64 * P) >> 20,
        TILES.load(Relaxed),
        PRIVATE.load(Relaxed),
        NO_CARD.load(Relaxed),
        HELD_BACK.load(Relaxed),
        BATCHES.load(Relaxed),
        TOUCHED.load(Relaxed),
        SAME.load(Relaxed),
        STILL_UNMAPPED.load(Relaxed),
        REMAPS.load(Relaxed),
        UNALIGNED.load(Relaxed),
        AT_BUDGET.load(Relaxed),
        NOT_ELIGIBLE.load(Relaxed),
        NO_LAYOUT.load(Relaxed),
        CREATE_FAILED.load(Relaxed),
        MAP_FAILED.load(Relaxed),
        queue.unwrap_or_else(|| "none".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    fn tex(w: u64, h: u32, arrays: u16, mips: u16, format: DXGI_FORMAT) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w,
            Height: h,
            DepthOrArraySize: arrays,
            MipLevels: mips,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        }
    }

    #[test]
    fn each_console_mip_has_a_range_inside_the_texture() {
        for d in [tex(2048, 2048, 1, 12, DXGI_FORMAT_BC7_UNORM), tex(512, 256, 6, 10, DXGI_FORMAT_R8G8B8A8_UNORM), tex(256, 128, 1, 1, DXGI_FORMAT_R8_UNORM)] {
            let c = console_layout(&d).expect("a layout");
            assert_eq!(c.mips, u32::from(d.MipLevels));
            assert_eq!(c.slices, u32::from(d.DepthOrArraySize));
            assert_eq!(c.ranges.len() as u32, c.mips * c.slices);
            let total = c.ranges.iter().map(|r| r.1).max().unwrap();
            for (i, &(s, e)) in c.ranges.iter().enumerate() {
                assert!(s < e && e <= total, "range {i} {s:#x}..{e:#x}");
            }
            // slices follow one another
            if c.slices > 1 {
                assert!(c.ranges[c.mips as usize].0 > c.ranges[0].0);
            }
            // the largest mip of a big texture spans whole pages of its own
            if d.Width >= 2048 {
                assert!(c.ranges[0].1 - c.ranges[0].0 >= 4 * P, "{:x?}", c.ranges[0]);
            }
        }
    }

    #[test]
    fn a_3d_texture_has_no_mapped_layout() {
        let mut d = tex(64, 64, 16, 1, DXGI_FORMAT_R8G8B8A8_UNORM);
        d.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        assert!(console_layout(&d).is_none());
    }
}
