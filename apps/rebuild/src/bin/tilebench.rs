//! Lane `vram2`: a stress test of reserved-texture tile mappings at the rate a world load streams them (WARP only).
//!
//! The title streams textures the console way (queue 0xf8): textures over reserved GPU addresses, pages of one
//! pool mapped under them as data arrives. `runtime::mapped` makes those textures reserved host textures whose tiles
//! follow the pool pages. A card run (`agents/vram.md`, `fix4`) made ~27,500 such textures and ~67,000 tile remaps
//! in ~20 s, and hung the GPU. This replays a stream of that shape with no title:
//!
//! ```text
//! driver thread   creations (C/s, ~80 % over mapped pages, aliases sharing them) and 0xf8-like batches (B/s of M
//!                 pages: new pages, then a share moved to another pool page or mapped again onto the one they
//!                 have, a few unmapped; the oldest slots streamed out when the pool is full)
//! frame thread    a direct queue at 60 Hz: Wait(mapping fence, latest) -> an empty list -> Signal
//! paths           old   the vram lane's calls: per new key one call on a sub-window buffer, per freed key one,
//!                       per texture a NULL call and one per heap, on the "ring" queue the frames wait for, issued
//!                       on the driver thread
//!                 new   runtime::tilequeue: one call per texture per creation or batch (per heap), same-page
//!                       remaps skipped, key pages not mapped (only reused ones, once a batch), on a queue and a
//!                       thread of their own
//! ```
//!
//! `tilebench [--seconds 20] [--path old|new|both] [--creations 1600] [--batches 18] [--pages 111] [--live 20000]
//! [--pool 32768] [--moved 0.1]`
//!
//! The defaults are the `fix4` run's shape: ~1,600 creations a second, 18 batches of 111 pages (39,191 pages mapped
//! in 352 batches), a pool of ~2 GiB (2,267 MiB mapped at the end), so pages are seldom streamed out (2,919 of
//! 39,191 were unmapped).
//!
//! Reports per path: `UpdateTileMappings` calls, tiles a call and a range, the driver thread's time in mapping
//! calls, mapping latency (submit -> the GPU passing its fence) and frame latency (submit -> done), p50 / p99 /
//! worst, and the device's state at the end.

#[cfg(not(windows))]
fn main() {
    println!("tilebench: Windows only");
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(windows)]
mod imp {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows::core::Interface;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;

    use rebuild_runtime::tilequeue::{self, Call, Heaps, Job, TexTile, TileQueue, HEAP_TILES};

    const P: u64 = 64 << 10;

    struct Opts {
        seconds: f64,
        creations: f64,
        batches: f64,
        pages: usize,
        live: usize,
        pool: u32,
        heaps: u32,
        /// of a batch's pages: the share moved to another pool page or mapped again onto theirs (the rest new)
        moved: f64,
    }

    pub fn main() {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut o = Opts { seconds: 20.0, creations: 1600.0, batches: 18.0, pages: 111, live: 20000, pool: 32768, heaps: 33, moved: 0.1 };
        let mut path = "both".to_string();
        let mut i = 0;
        while i < argv.len() {
            let v = argv.get(i + 1).cloned().unwrap_or_default();
            match argv[i].as_str() {
                "--seconds" => o.seconds = v.parse().unwrap_or(o.seconds),
                "--creations" => o.creations = v.parse().unwrap_or(o.creations),
                "--batches" => o.batches = v.parse().unwrap_or(o.batches),
                "--pages" => o.pages = v.parse().unwrap_or(o.pages),
                "--live" => o.live = v.parse().unwrap_or(o.live),
                "--pool" => {
                    o.pool = v.parse().unwrap_or(o.pool);
                    o.heaps = o.pool.div_ceil(HEAP_TILES) + 1;
                }
                "--moved" => o.moved = v.parse().unwrap_or(o.moved),
                "--path" => path = v.clone(),
                other => {
                    println!("FAILED    unknown argument {other}");
                    std::process::exit(2);
                }
            }
            i += 2;
        }
        println!(
            "tilebench on WARP: {:.0} s, {:.0} creations/s, {:.0} batches/s of {} pages ({:.0} % moved or again), {} live textures at most, pool {} pages, {} heaps of {} tiles, tile queue {:?}",
            o.seconds,
            o.creations,
            o.batches,
            o.pages,
            o.moved * 100.0,
            o.live,
            o.pool,
            o.heaps,
            HEAP_TILES,
            tilequeue::queue_type().0
        );
        let mut ok = true;
        for p in ["old", "new"] {
            if path == p || path == "both" {
                match run(&o, p == "new") {
                    Ok(line) => println!("{line}"),
                    Err(e) => {
                        ok = false;
                        println!("FAILED    {p}: {e}");
                    }
                }
            }
        }
        std::process::exit(if ok { 0 } else { 1 });
    }

    fn warp() -> Result<ID3D12Device, String> {
        unsafe {
            let f: IDXGIFactory4 = CreateDXGIFactory1().map_err(|e| format!("factory: {e}"))?;
            let a: IDXGIAdapter = f.EnumWarpAdapter().map_err(|e| format!("WARP adapter: {e}"))?;
            let mut d: Option<ID3D12Device> = None;
            D3D12CreateDevice(&a, D3D_FEATURE_LEVEL_11_0, &mut d).map_err(|e| format!("WARP device: {e}"))?;
            d.ok_or_else(|| "WARP device: none".into())
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
        fn chance(&mut self, p: f64) -> bool {
            (self.next() % 1_000_000) as f64 / 1e6 < p
        }
    }

    /// Texture shapes (≈ 10 tiles on average; the run's was 11.6): width, height, mips, format, weight in 100.
    const SHAPES: [(u64, u32, u16, DXGI_FORMAT, u64); 4] = [
        (512, 512, 10, DXGI_FORMAT_BC7_UNORM, 50),
        (256, 256, 9, DXGI_FORMAT_R8G8B8A8_UNORM, 30),
        (1024, 1024, 11, DXGI_FORMAT_BC7_UNORM, 15),
        (2048, 2048, 12, DXGI_FORMAT_BC1_UNORM, 5),
    ];

    fn desc(shape: usize) -> D3D12_RESOURCE_DESC {
        let (w, h, mips, format, _) = SHAPES[shape];
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: mips,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
            ..Default::default()
        }
    }

    /// A shape's host tiles in tile order (as `runtime::mapped` plans them): standard mips' tiles, then packed ones.
    fn tiles_of(device: &ID3D12Device, r: &ID3D12Resource, mips: u32) -> Vec<TexTile> {
        let mut total = 0u32;
        let mut packed = D3D12_PACKED_MIP_INFO::default();
        let mut shape = D3D12_TILE_SHAPE::default();
        let mut n = mips;
        let mut subs = vec![D3D12_SUBRESOURCE_TILING::default(); mips as usize];
        unsafe { device.GetResourceTiling(r, Some(&mut total), Some(&mut packed), Some(&mut shape), Some(&mut n), 0, subs.as_mut_ptr()) };
        let standard = u32::from(packed.NumStandardMips).min(mips);
        let mut out = Vec::with_capacity(total as usize);
        for m in 0..standard {
            let t = subs[m as usize];
            for y in 0..u32::from(t.HeightInTiles) {
                for x in 0..t.WidthInTiles {
                    out.push(TexTile { sub: m, coord: [x, y, 0], index: x + y * t.WidthInTiles, card: None });
                }
            }
        }
        for j in 0..packed.NumTilesForPackedMips {
            out.push(TexTile { sub: standard, coord: [j, 0, 0], index: j, card: None });
        }
        out
    }

    struct Tex {
        resource: ID3D12Resource,
        slot: usize,
        /// per tile: the key (pool page) it names now, and the tile
        keys: Vec<Option<u32>>,
        tiles: Vec<TexTile>,
    }

    struct Model {
        device: ID3D12Device,
        heaps: Vec<ID3D12Heap>,
        /// the sub-window buffer key pages are mapped in (old path; reused pages on the new), and the ring queue
        sub: ID3D12Resource,
        ring: ID3D12CommandQueue,
        ring_fence: ID3D12Fence,
        ring_value: u64,
        ring_issued: u64,
        new: Option<Arc<TileQueue>>,
        shape_tiles: Vec<Vec<TexTile>>,
        /// slots of console pages (texture footprints): base page, shape
        slots: Vec<(u64, usize)>,
        next_page: u64,
        page_slot: HashMap<u64, usize>,
        /// console page -> pool page
        map: HashMap<u64, u32>,
        mapped_slots: VecDeque<usize>,
        unmapped_slots: VecDeque<usize>,
        pool_free: VecDeque<u32>,
        /// key (pool page) -> (tiles naming it, card page)
        refs: HashMap<u32, (u32, u32)>,
        card_free: Vec<u32>,
        card_next: u32,
        card_reached_before: u32,
        textures: HashMap<u64, Tex>,
        order: VecDeque<u64>,
        serial: u64,
        slot_tex: HashMap<usize, Vec<u64>>,
        // counts
        calls: u64,
        tiles: u64,
        ranges: u64,
        call_time: Duration,
        made: u64,
        gone: u64,
        remaps: u64,
        same: u64,
        reused_maps: u64,
    }

    impl Model {
        fn card(&mut self) -> Option<u32> {
            if let Some(c) = self.card_free.pop() {
                return Some(c);
            }
            if self.card_next < self.heaps.len() as u32 * HEAP_TILES {
                self.card_next += 1;
                return Some(self.card_next - 1);
            }
            None
        }

        fn issue(&mut self, r: &ID3D12Resource, c: &Call) {
            let t = Instant::now();
            issue(&self.ring, r, c, &self.heaps);
            self.call_time += t.elapsed();
            self.calls += 1;
            self.ring_issued += 1;
            self.tiles += u64::from(c.tiles());
            self.ranges += c.ranges.len() as u64;
        }

        /// One call on the sub-window buffer: key `key` onto `card`, or unmapped (the old path's per-key calls).
        fn sub_call(&mut self, key: u32, card: Option<u32>) {
            let c = Call { heap: card.map(|c| c / HEAP_TILES), starts: vec![(0, [key, 0, 0])], ranges: vec![(1, card.map(|c| c % HEAP_TILES))] };
            let sub = self.sub.clone();
            self.issue(&sub, &c);
        }

        /// A tile comes to name `key`: its card page (one call on the old path when the key is new).
        fn take(&mut self, key: u32, old_path: bool, reused: &mut Vec<u32>) -> Option<u32> {
            if let Some(e) = self.refs.get_mut(&key) {
                e.0 += 1;
                return Some(e.1);
            }
            let c = self.card()?;
            self.refs.insert(key, (1, c));
            if old_path {
                self.sub_call(key, Some(c));
            } else if c < self.card_reached_before {
                // the new path maps (to zero) only reused card pages, once a batch
                reused.push(key);
            }
            Some(c)
        }

        fn release(&mut self, key: u32, old_path: bool) {
            let Some(e) = self.refs.get_mut(&key) else { return };
            e.0 -= 1;
            if e.0 == 0 {
                let c = e.1;
                self.refs.remove(&key);
                self.card_free.push(c);
                if old_path {
                    self.sub_call(key, None);
                }
            }
        }

        /// The old path's per-texture map: NULL runs in one call, then one call per heap.
        fn old_map(&mut self, r: &ID3D12Resource, tiles: &[TexTile]) {
            let nulls: Vec<TexTile> = tiles.iter().copied().filter(|t| t.card.is_none()).collect();
            let mapped: Vec<TexTile> = tiles.iter().copied().filter(|t| t.card.is_some()).collect();
            let mut calls = tilequeue::plan(&nulls);
            calls.extend(tilequeue::plan(&mapped));
            for c in &calls {
                self.issue(r, c);
            }
        }

        /// New path: reused card pages mapped in their sub-window (to be zeroed), one call a batch.
        fn reused(&mut self, keys: &[u32]) {
            if keys.is_empty() {
                return;
            }
            let tiles: Vec<TexTile> = keys.iter().map(|&k| TexTile { sub: 0, coord: [k, 0, 0], index: k, card: self.refs.get(&k).map(|e| e.1) }).collect();
            let sub = self.sub.clone();
            for c in tilequeue::plan(&tiles) {
                self.issue(&sub, &c);
                self.reused_maps += 1;
            }
        }

        fn new_slot(&mut self, rng: &mut Rng) -> usize {
            let mut x = rng.below(100);
            let mut shape = 0;
            for (k, s) in SHAPES.iter().enumerate() {
                if x < s.4 {
                    shape = k;
                    break;
                }
                x -= s.4;
            }
            let pages = self.shape_tiles[shape].len() as u64;
            let k = self.slots.len();
            self.slots.push((self.next_page, shape));
            for j in 0..pages {
                self.page_slot.insert(self.next_page + j, k);
            }
            self.next_page += pages;
            self.unmapped_slots.push_back(k);
            k
        }

        /// Map slot `k`'s pages to free pool pages (streaming out the oldest slots when the pool is full).
        fn stream_in(&mut self, k: usize, pages: &mut Vec<u64>) {
            self.unmapped_slots.retain(|&x| x != k);
            let (base, shape) = self.slots[k];
            for j in 0..self.shape_tiles[shape].len() as u64 {
                if self.pool_free.is_empty() {
                    self.stream_out(pages);
                }
                let Some(pp) = self.pool_free.pop_front() else { break };
                self.map.insert(base + j, pp);
                pages.push(base + j);
            }
            self.mapped_slots.push_back(k);
        }

        fn stream_out(&mut self, pages: &mut Vec<u64>) {
            let Some(k) = self.mapped_slots.pop_front() else { return };
            let (base, shape) = self.slots[k];
            for j in 0..self.shape_tiles[shape].len() as u64 {
                if let Some(cur) = self.map.remove(&(base + j)) {
                    pages.push(base + j);
                    self.pool_free.push_back(cur);
                }
            }
            self.unmapped_slots.push_back(k);
        }

        /// A texture over slot `k`, tiles onto what is mapped now.
        fn create(&mut self, k: usize, old_path: bool) -> Result<Option<Job>, String> {
            let (base, shape) = self.slots[k];
            let mut r: Option<ID3D12Resource> = None;
            unsafe { self.device.CreateReservedResource(&desc(shape), D3D12_RESOURCE_STATE_COMMON, None, &mut r) }.map_err(|e| format!("reserved texture: {e}"))?;
            let r = r.ok_or("reserved texture: none")?;
            let mut tiles = self.shape_tiles[shape].clone();
            let mut keys = Vec::with_capacity(tiles.len());
            let mut reused = Vec::new();
            for (j, t) in tiles.iter_mut().enumerate() {
                let key = self.map.get(&(base + j as u64)).copied();
                let card = key.and_then(|key| self.take(key, old_path, &mut reused));
                t.card = card;
                keys.push(if card.is_some() { key } else { None });
            }
            self.reused(&reused);
            let job = if old_path {
                self.old_map(&r, &tiles);
                None
            } else {
                // a reserved texture starts unmapped: only what has a card page
                let mapped: Vec<TexTile> = tiles.iter().copied().filter(|t| t.card.is_some()).collect();
                (!mapped.is_empty()).then(|| Job { resource: r.clone(), tiles: mapped })
            };
            self.made += 1;
            self.serial += 1;
            let id = self.serial;
            self.slot_tex.entry(k).or_default().push(id);
            self.order.push_back(id);
            self.textures.insert(id, Tex { resource: r, slot: k, keys, tiles });
            Ok(job)
        }

        fn destroy_oldest(&mut self, old_path: bool) {
            let Some(id) = self.order.pop_front() else { return };
            let Some(t) = self.textures.remove(&id) else { return };
            for key in t.keys.iter().flatten() {
                self.release(*key, old_path);
            }
            if let Some(v) = self.slot_tex.get_mut(&t.slot) {
                v.retain(|&x| x != id);
            }
            self.gone += 1;
        }

        /// The textures over `pages` follow their new mapping.
        fn follow(&mut self, pages: &[u64], old_path: bool) -> Vec<Job> {
            let touched: HashSet<u64> = pages.iter().copied().collect();
            let mut slots: Vec<usize> = pages.iter().filter_map(|p| self.page_slot.get(p).copied()).collect();
            slots.sort_unstable();
            slots.dedup();
            let ids: Vec<u64> = slots.iter().flat_map(|k| self.slot_tex.get(k).cloned().unwrap_or_default()).collect();
            let mut jobs = Vec::new();
            let mut reused = Vec::new();
            for id in ids {
                let Some(t) = self.textures.get(&id) else { continue };
                let base = self.slots[t.slot].0;
                let js: Vec<usize> = (0..t.tiles.len()).filter(|&j| touched.contains(&(base + j as u64))).collect();
                let mut changed = Vec::new();
                for j in js {
                    let old = self.textures[&id].keys[j];
                    let new = self.map.get(&(base + j as u64)).copied();
                    if new == old && !old_path {
                        self.same += 1;
                        continue;
                    }
                    // the old path took the new key before giving back the old, and then skipped same keys
                    let card = new.and_then(|k| self.take(k, old_path, &mut reused));
                    if let Some(k) = old {
                        self.release(k, old_path);
                    }
                    if old_path && new == old && card.is_some() {
                        self.same += 1;
                        continue;
                    }
                    let t = self.textures.get_mut(&id).expect("live");
                    t.keys[j] = if card.is_some() { new } else { None };
                    t.tiles[j].card = card;
                    changed.push(t.tiles[j]);
                }
                if changed.is_empty() {
                    continue;
                }
                self.remaps += changed.len() as u64;
                let r = self.textures[&id].resource.clone();
                if old_path {
                    self.old_map(&r, &changed);
                } else {
                    jobs.push(Job { resource: r, tiles: changed });
                }
            }
            self.reused(&reused);
            jobs
        }

        /// One 0xf8-like batch: new slots' pages (70 %), then pages of mapped slots moved to another pool page or
        /// mapped again onto theirs, and a few unmapped.
        fn batch(&mut self, rng: &mut Rng, o: &Opts, old_path: bool) -> Vec<Job> {
            let mut pages: Vec<u64> = Vec::new();
            let fresh = o.pages - (o.pages as f64 * o.moved) as usize;
            while pages.len() < fresh {
                let k = match self.unmapped_slots.front() {
                    Some(&k) => k,
                    None => self.new_slot(rng),
                };
                self.stream_in(k, &mut pages);
            }
            for i in 0..o.pages - fresh {
                if self.mapped_slots.is_empty() {
                    break;
                }
                let k = self.mapped_slots[rng.below(self.mapped_slots.len() as u64) as usize];
                let (base, shape) = self.slots[k];
                let p = base + rng.below(self.shape_tiles[shape].len() as u64);
                let Some(&cur) = self.map.get(&p) else { continue };
                if i % 3 == 0 {
                    // onto the pool page it has already
                    pages.push(p);
                } else if let Some(pp) = self.pool_free.pop_front() {
                    self.map.insert(p, pp);
                    self.pool_free.push_back(cur);
                    pages.push(p);
                }
            }
            for _ in 0..8 {
                if self.mapped_slots.len() < 64 {
                    break;
                }
                let k = self.mapped_slots[rng.below(self.mapped_slots.len() as u64 / 2) as usize];
                let p = self.slots[k].0;
                if let Some(cur) = self.map.remove(&p) {
                    self.pool_free.push_back(cur);
                    pages.push(p);
                }
            }
            pages.sort_unstable();
            pages.dedup();
            self.follow(&pages, old_path)
        }

        /// New path: one submit; old path: the ring signals what was issued. Either way the frames wait for it.
        fn submit(&mut self, jobs: Vec<Job>, latest: &AtomicU64, tx: &Sender<(u64, Instant)>) {
            let t = Instant::now();
            let v = match &self.new {
                Some(q) => {
                    let v = q.submit(jobs);
                    self.call_time += t.elapsed();
                    v
                }
                None => None,
            };
            // the ring's value covers calls issued on it (all calls on the old path; reused pages on the new)
            if self.ring_issued > 0 {
                self.ring_issued = 0;
                self.ring_value += 1;
                let _ = unsafe { self.ring.Signal(&self.ring_fence, self.ring_value) };
                if self.new.is_none() {
                    latest.store(self.ring_value, Relaxed);
                    let _ = tx.send((self.ring_value, t));
                }
            }
            if let Some(v) = v {
                let _ = tx.send((v, t));
            }
        }
    }

    fn issue(queue: &ID3D12CommandQueue, r: &ID3D12Resource, c: &Call, heaps: &[ID3D12Heap]) {
        let coords: Vec<D3D12_TILED_RESOURCE_COORDINATE> = c.starts.iter().map(|&(s, [x, y, z])| D3D12_TILED_RESOURCE_COORDINATE { X: x, Y: y, Z: z, Subresource: s }).collect();
        let sizes: Vec<D3D12_TILE_REGION_SIZE> = c.ranges.iter().map(|r| D3D12_TILE_REGION_SIZE { NumTiles: r.0, UseBox: false.into(), Width: 0, Height: 0, Depth: 0 }).collect();
        let flags: Vec<D3D12_TILE_RANGE_FLAGS> = c.ranges.iter().map(|r| if r.1.is_some() { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_NULL }).collect();
        let offsets: Vec<u32> = c.ranges.iter().map(|r| r.1.unwrap_or(0)).collect();
        let counts: Vec<u32> = c.ranges.iter().map(|r| r.0).collect();
        let heap = c.heap.map(|h| &heaps[h as usize]);
        unsafe {
            queue.UpdateTileMappings(
                r,
                coords.len() as u32,
                Some(coords.as_ptr()),
                Some(sizes.as_ptr()),
                heap,
                flags.len() as u32,
                Some(flags.as_ptr()),
                heap.map(|_| offsets.as_ptr()),
                Some(counts.as_ptr()),
                D3D12_TILE_MAPPING_FLAG_NONE,
            )
        };
    }

    struct F(ID3D12Fence);
    // SAFETY: fences are free-threaded
    unsafe impl Send for F {}

    /// Latencies of fence values (in order), recorded by a thread that blocks on each in turn.
    fn watcher(fence: ID3D12Fence, rx: Receiver<(u64, Instant)>, out: Arc<Mutex<Vec<f64>>>) -> std::thread::JoinHandle<()> {
        let f = F(fence);
        std::thread::spawn(move || {
            let f = f;
            for (v, t) in rx {
                let _ = unsafe { f.0.SetEventOnCompletion(v, HANDLE::default()) };
                out.lock().unwrap().push(t.elapsed().as_secs_f64() * 1000.0);
            }
        })
    }

    fn pct(v: &mut [f64]) -> String {
        if v.is_empty() {
            return "none".into();
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
        format!("p50 {:.2} p99 {:.2} worst {:.2} ms ({})", at(0.5), at(0.99), v[v.len() - 1], v.len())
    }

    fn run(o: &Opts, new_path: bool) -> Result<String, String> {
        let device = warp()?;
        let name = if new_path { "new" } else { "old" };
        let mut heaps = Vec::new();
        for _ in 0..o.heaps {
            let mut h: Option<ID3D12Heap> = None;
            let d = D3D12_HEAP_DESC {
                SizeInBytes: u64::from(HEAP_TILES) * P,
                Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                Alignment: P,
                Flags: D3D12_HEAP_FLAG_NONE,
            };
            unsafe { device.CreateHeap(&d, &mut h) }.map_err(|e| format!("heap: {e}"))?;
            heaps.push(h.ok_or("heap: none")?);
        }
        let buffer = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: u64::from(o.pool) * P,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut sub: Option<ID3D12Resource> = None;
        unsafe { device.CreateReservedResource(&buffer, D3D12_RESOURCE_STATE_COMMON, None, &mut sub) }.map_err(|e| format!("sub-window: {e}"))?;
        let ring: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COMPUTE, ..Default::default() }) }.map_err(|e| format!("ring queue: {e}"))?;
        let ring_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(|e| format!("fence: {e}"))?;
        let new = if new_path {
            struct H(Vec<ID3D12Heap>);
            // SAFETY: heaps are free-threaded
            unsafe impl Send for H {}
            unsafe impl Sync for H {}
            let hs = H(heaps.clone());
            let resolve: Heaps = Arc::new(move |h| hs.0.get(h as usize).cloned());
            Some(Arc::new(TileQueue::new(&device, tilequeue::queue_type(), resolve)?))
        } else {
            None
        };
        let mut shape_tiles = Vec::new();
        for s in 0..SHAPES.len() {
            let mut r: Option<ID3D12Resource> = None;
            unsafe { device.CreateReservedResource(&desc(s), D3D12_RESOURCE_STATE_COMMON, None, &mut r) }.map_err(|e| format!("shape {s}: {e}"))?;
            shape_tiles.push(tiles_of(&device, &r.ok_or("shape")?, u32::from(SHAPES[s].2)));
        }
        let mut m = Model {
            device: device.clone(),
            heaps,
            sub: sub.ok_or("sub-window: none")?,
            ring: ring.clone(),
            ring_fence: ring_fence.clone(),
            ring_value: 0,
            ring_issued: 0,
            new: new.clone(),
            shape_tiles,
            slots: Vec::new(),
            next_page: 0,
            page_slot: HashMap::new(),
            map: HashMap::new(),
            mapped_slots: VecDeque::new(),
            unmapped_slots: VecDeque::new(),
            pool_free: (0..o.pool).collect(),
            refs: HashMap::new(),
            card_free: Vec::new(),
            card_next: 0,
            card_reached_before: 0,
            textures: HashMap::new(),
            order: VecDeque::new(),
            serial: 0,
            slot_tex: HashMap::new(),
            calls: 0,
            tiles: 0,
            ranges: 0,
            call_time: Duration::ZERO,
            made: 0,
            gone: 0,
            remaps: 0,
            same: 0,
            reused_maps: 0,
        };
        // the mapping fence the frames wait for: the ring's on the old path, the tile queue's on the new
        let map_fence = new.as_ref().map_or(ring_fence.clone(), |q| q.fence.clone());
        let latest = Arc::new(AtomicU64::new(0));
        let map_lat = Arc::new(Mutex::new(Vec::new()));
        let (map_tx, map_rx) = channel();
        let map_watch = watcher(map_fence.clone(), map_rx, map_lat.clone());
        // the frame thread
        let stop = Arc::new(AtomicBool::new(false));
        let frame_lat = Arc::new(Mutex::new(Vec::new()));
        let frames = {
            struct S(ID3D12Device, ID3D12Fence, Option<Arc<TileQueue>>);
            // SAFETY: D3D12 objects are free-threaded
            unsafe impl Send for S {}
            let s = S(device.clone(), map_fence.clone(), new.clone());
            let (stop, latest, out) = (stop.clone(), latest.clone(), frame_lat.clone());
            std::thread::spawn(move || -> Result<u64, String> {
                let s = s;
                let d = &s.0;
                let q: ID3D12CommandQueue = unsafe { d.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }) }.map_err(|e| format!("frame queue: {e}"))?;
                let alloc: ID3D12CommandAllocator = unsafe { d.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }.map_err(|e| format!("allocator: {e}"))?;
                let list: ID3D12GraphicsCommandList = unsafe { d.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None) }.map_err(|e| format!("list: {e}"))?;
                unsafe { list.Close() }.map_err(|e| format!("close: {e}"))?;
                let fence: ID3D12Fence = unsafe { d.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(|e| format!("fence: {e}"))?;
                let (tx, rx) = channel();
                let w = watcher(fence.clone(), rx, out);
                let mut n = 0u64;
                let start = Instant::now();
                while !stop.load(Relaxed) {
                    let due = start + Duration::from_secs_f64(n as f64 / 60.0);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    match &s.2 {
                        Some(tq) => tq.wait(&q),
                        None => {
                            let v = latest.load(Relaxed);
                            if v > 0 {
                                let _ = unsafe { q.Wait(&s.1, v) };
                            }
                        }
                    }
                    n += 1;
                    let t = Instant::now();
                    unsafe {
                        q.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                        let _ = q.Signal(&fence, n);
                    }
                    let _ = tx.send((n, t));
                }
                drop(tx);
                let _ = w.join();
                Ok(n)
            })
        };
        // seed: slots mapped before the stream starts
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut seeded = Vec::new();
        for _ in 0..64 {
            let k = m.new_slot(&mut rng);
            m.stream_in(k, &mut seeded);
        }
        let start = Instant::now();
        let (mut created, mut batched, mut batches) = (0.0f64, 0.0f64, 0u64);
        let mut failed: Option<String> = None;
        let mut driver = Duration::ZERO;
        // 1 ms ticks of stream time: both paths replay the same stream; a path slower than real time lags
        let ticks = (o.seconds * 1000.0) as u64;
        let mut lag_ms = 0.0f64;
        for tick in 0..ticks {
            let now = (tick + 1) as f64 / 1000.0;
            let t0 = Instant::now();
            m.card_reached_before = m.card_next;
            let mut jobs: Vec<Job> = Vec::new();
            while created < now * o.creations {
                created += 1.0;
                // ~80 % over recently mapped slots (aliases share them), the rest over slots not mapped yet
                let k = if !m.mapped_slots.is_empty() && rng.chance(0.8) {
                    let n = m.mapped_slots.len() as u64;
                    m.mapped_slots[(n - 1 - rng.below(n.min(256))) as usize]
                } else if let Some(&k) = m.unmapped_slots.back() {
                    k
                } else {
                    m.new_slot(&mut rng)
                };
                match m.create(k, !new_path) {
                    Ok(Some(j)) => jobs.push(j),
                    Ok(None) => {}
                    Err(e) => failed = Some(e),
                }
                if m.textures.len() > o.live {
                    m.destroy_oldest(!new_path);
                }
            }
            m.submit(jobs, &latest, &map_tx);
            if batched < now * o.batches {
                batched += 1.0;
                batches += 1;
                m.card_reached_before = m.card_next;
                let jobs = m.batch(&mut rng, o, !new_path);
                m.submit(jobs, &latest, &map_tx);
            }
            driver += t0.elapsed();
            if failed.is_some() {
                break;
            }
            let due = start + Duration::from_secs_f64(now);
            match due.checked_duration_since(Instant::now()) {
                Some(w) => std::thread::sleep(w),
                None => lag_ms = lag_ms.max(start.elapsed().as_secs_f64() * 1000.0 - now * 1000.0),
            }
        }
        let wall = start.elapsed().as_secs_f64();
        stop.store(true, Relaxed);
        let frames = frames.join().map_err(|_| "frame thread panicked")??;
        let drained = Instant::now();
        if let Some(q) = &new {
            q.drain()?;
        }
        let _ = unsafe { ring_fence.SetEventOnCompletion(m.ring_value, HANDLE::default()) };
        let drain_ms = drained.elapsed().as_secs_f64() * 1000.0;
        drop(map_tx);
        let _ = map_watch.join();
        let removed = unsafe { device.GetDeviceRemovedReason() };
        let (mut calls, mut tiles, mut ranges) = (m.calls, m.tiles, m.ranges);
        let mut extra = String::new();
        if let Some(q) = &new {
            calls += q.totals.calls.load(Relaxed);
            tiles += q.totals.tiles.load(Relaxed);
            ranges += q.totals.ranges.load(Relaxed);
            extra = format!("\n          tile queue: {}", tilequeue::report(q));
        }
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(format!(
            "{name}       {:.1} s: {} textures made ({} gone), {} batches, {} tiles remapped ({} onto the page they had); {} UpdateTileMappings ({:.0}/s; {} for reused pages), {:.1} tiles a call, {:.1} a range; driver thread: {:.1} ms a second in mapping calls/submits, {:.1} busy; drain at the end {:.1} ms; worst lag behind the stream {:.0} ms\n          mapping latency {}\n          frame latency   {} ({} frames, {:.1} fps); device {}{extra}",
            wall,
            m.made,
            m.gone,
            batches,
            m.remaps,
            m.same,
            calls,
            calls as f64 / wall,
            m.reused_maps,
            tiles as f64 / calls.max(1) as f64,
            tiles as f64 / ranges.max(1) as f64,
            m.call_time.as_secs_f64() * 1000.0 / wall,
            driver.as_secs_f64() * 1000.0 / wall,
            drain_ms,
            lag_ms,
            pct(&mut map_lat.lock().unwrap()),
            pct(&mut frame_lat.lock().unwrap()),
            frames,
            frames as f64 / wall,
            match removed {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("REMOVED {e}"),
            },
        ))
    }
}
