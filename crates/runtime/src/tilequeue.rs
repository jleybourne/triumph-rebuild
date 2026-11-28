//! Tile mappings of reserved textures (`crate::mapped`): planned into as few `UpdateTileMappings` calls as the API
//! allows, and issued by a thread of their own on a queue of their own, never on the title's threads or the ring's
//! queue.
//!
//! ```text
//! title thread    mapped::texture / remapped -> submit(jobs of tiles) -> value V             (a channel send)
//! tile thread     every queued job: merged per resource -> plan -> UpdateTileMappings, then Signal(fence, highest V)
//! title queue     ExecuteCommandLists: Wait(fence, latest V) first (memory::flush_wait)    the GPU waits
//! ```
//!
//! Why: with every texture mapped at creation and every 0xf8 remap mapped at once on the ring's queue (which every
//! title submission waits for), a world load issued ~120,000 `UpdateTileMappings` in ~20 s from the title's threads,
//! with the memory model's lock held through each call; the card stalled and then hung (`agents/vram.md`, run
//! `fix4`). Here a call covers one resource: one per heap its new card pages are in, its unmapped tiles folded into
//! the first, contiguous tiles onto contiguous card pages as one range. A freshly reserved texture's tiles are
//! unmapped already, so a creation only maps what has a card page. When the thread falls behind, the jobs waiting
//! for one resource are merged (the later mapping of a tile wins), so a slow driver gets fewer, larger calls.
//!
//! **Order.** Jobs run in the order they were submitted (one channel, one thread, one queue); a title submission made
//! after a submit waits for that submit's value on the GPU, so its copies and draws see the new mapping. The tile
//! queue never waits for anything, so no title queue can be made to wait in a cycle. A job holds its resource until
//! the fence passes its value.

#![cfg(windows)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;

/// Tiles per heap of card pages (`rebuild_uma::tiles::HEAP_TILES`): card page `c` is tile `c % HEAP_TILES` of heap
/// `c / HEAP_TILES`.
pub const HEAP_TILES: u32 = rebuild_uma::tiles::HEAP_TILES;

/// One tile of a reserved texture: its subresource, its tile coordinate there, its index in the subresource's tile
/// order (X, then Y, then Z; packed mips: the packed tile's index), and the card page it goes onto (`None`:
/// unmapped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexTile {
    pub sub: u32,
    pub coord: [u32; 3],
    pub index: u32,
    pub card: Option<u32>,
}

/// One `UpdateTileMappings` of one resource: onto heap `heap` (`None`: every range unmapped), regions and ranges one
/// to one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Call {
    pub heap: Option<u32>,
    /// region starts: (subresource, x, y, z)
    pub starts: Vec<(u32, [u32; 3])>,
    /// per region: tiles, and the first card page's tile in the heap (`None`: unmapped)
    pub ranges: Vec<(u32, Option<u32>)>,
}

impl Call {
    pub fn tiles(&self) -> u32 {
        self.ranges.iter().map(|r| r.0).sum()
    }
}

/// The calls that map `tiles` (any order; a tile given twice: the later wins) of one resource: runs of consecutive
/// tiles of one subresource onto consecutive card pages of one heap, or unmapped, each one range; one call per heap,
/// the unmapped runs folded into the first (a range flagged NULL ignores the heap), or one call of their own when
/// nothing is mapped.
pub fn plan(tiles: &[TexTile]) -> Vec<Call> {
    let mut sorted: Vec<TexTile> = Vec::with_capacity(tiles.len());
    // stable: equal tiles stay in the order given, and the last of each is kept
    let mut order: Vec<usize> = (0..tiles.len()).collect();
    order.sort_by_key(|&i| (tiles[i].sub, tiles[i].index));
    for i in order {
        let t = tiles[i];
        match sorted.last_mut() {
            Some(l) if (l.sub, l.index) == (t.sub, t.index) => *l = t,
            _ => sorted.push(t),
        }
    }
    // runs: (first tile, count)
    let mut runs: Vec<(TexTile, u32)> = Vec::new();
    for t in sorted {
        if let Some((r, n)) = runs.last_mut() {
            let next = r.sub == t.sub && r.index + *n == t.index;
            let cards = match (r.card, t.card) {
                (Some(a), Some(b)) => a / HEAP_TILES == b / HEAP_TILES && a + *n == b,
                (None, None) => true,
                _ => false,
            };
            if next && cards {
                *n += 1;
                continue;
            }
        }
        runs.push((t, 1));
    }
    let mut heaps: Vec<u32> = runs.iter().filter_map(|(r, _)| r.card).map(|c| c / HEAP_TILES).collect();
    heaps.sort_unstable();
    heaps.dedup();
    let mut calls: Vec<Call> = if heaps.is_empty() { vec![Call::default()] } else { heaps.iter().map(|&h| Call { heap: Some(h), ..Default::default() }).collect() };
    for (r, n) in runs {
        // unmapped runs go with the first call
        let k = r.card.map_or(0, |c| heaps.binary_search(&(c / HEAP_TILES)).unwrap_or(0));
        calls[k].starts.push((r.sub, r.coord));
        calls[k].ranges.push((n, r.card.map(|c| c % HEAP_TILES)));
    }
    calls.retain(|c| !c.ranges.is_empty());
    calls
}

/// Tiles of one resource to map (planned on the tile thread).
pub struct Job {
    pub resource: ID3D12Resource,
    pub tiles: Vec<TexTile>,
}

// SAFETY: D3D12 resources are free-threaded.
unsafe impl Send for Job {}

/// Heap `h` of card pages (made if it is not yet), for the tile thread.
pub type Heaps = Arc<dyn Fn(u32) -> Option<ID3D12Heap> + Send + Sync>;

/// Totals over the queue's life.
#[derive(Default)]
pub struct Totals {
    /// submits (one per creation or 0xf8 batch with something to map), jobs (resources), jobs merged into an
    /// earlier one for the same resource, calls, ranges, tiles
    pub submits: AtomicU64,
    pub jobs: AtomicU64,
    pub merged: AtomicU64,
    pub calls: AtomicU64,
    pub ranges: AtomicU64,
    pub tiles: AtomicU64,
    /// tile-thread passes (one Signal each), and the CPU time inside `UpdateTileMappings`
    pub passes: AtomicU64,
    pub call_ns: AtomicU64,
    /// submit -> Signal issued, summed and worst (ns)
    pub queued_ns: AtomicU64,
    pub queued_max_ns: AtomicU64,
    /// calls naming a heap that could not be made (left unmapped)
    pub no_heap: AtomicU64,
    /// title submissions that waited on the fence, and those that found it passed
    pub waits: AtomicU64,
    pub passed: AtomicU64,
    /// the last value the tile thread signalled, and when its current pass of calls began (ns from the queue's
    /// start; 0: not in a pass): a pass that does not end is a driver call that does not return
    pub signalled: AtomicU64,
    pub in_pass_since: AtomicU64,
}

enum Msg {
    Work(u64, Instant, Vec<Job>),
    Stop,
}

pub struct TileQueue {
    pub queue: ID3D12CommandQueue,
    pub fence: ID3D12Fence,
    /// the last value handed out
    issued: AtomicU64,
    tx: Mutex<Sender<Msg>>,
    pub totals: Arc<Totals>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

// SAFETY: queues and fences are free-threaded; the rest is atomics and a lock.
unsafe impl Send for TileQueue {}
unsafe impl Sync for TileQueue {}

/// The queue type tile mappings run on: `REBUILD_TILE_QUEUE=copy` (default), `compute` or `direct`.
pub fn queue_type() -> D3D12_COMMAND_LIST_TYPE {
    match std::env::var("REBUILD_TILE_QUEUE").as_deref().map(str::trim) {
        Ok("compute") => D3D12_COMMAND_LIST_TYPE_COMPUTE,
        Ok("direct") => D3D12_COMMAND_LIST_TYPE_DIRECT,
        _ => D3D12_COMMAND_LIST_TYPE_COPY,
    }
}

impl TileQueue {
    /// A queue of `kind` on `device` and its thread; `heaps` resolves heap indices.
    pub fn new(device: &ID3D12Device, kind: D3D12_COMMAND_LIST_TYPE, heaps: Heaps) -> Result<TileQueue, String> {
        let desc = D3D12_COMMAND_QUEUE_DESC { Type: kind, ..Default::default() };
        // SAFETY: a description
        let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&desc) }.map_err(|e| format!("tile queue: {e}"))?;
        // SAFETY: plain creation
        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(|e| format!("tile fence: {e}"))?;
        let (tx, rx) = channel();
        let _ = epoch();
        let totals = Arc::new(Totals::default());
        let (q2, f2, t2) = (queue.clone(), fence.clone(), totals.clone());
        let q2 = Wrap(q2);
        let f2 = Wrap(f2);
        let thread = std::thread::Builder::new()
            .name("tile-mappings".into())
            .spawn(move || run(q2.0, f2.0, rx, heaps, t2))
            .map_err(|e| format!("tile thread: {e}"))?;
        Ok(TileQueue { queue, fence, issued: AtomicU64::new(0), tx: Mutex::new(tx), totals, thread: Mutex::new(Some(thread)) })
    }

    /// Queue `jobs` (in order after everything submitted before); the fence value a queue waits for to see them.
    /// `None` when there is nothing to do.
    pub fn submit(&self, jobs: Vec<Job>) -> Option<u64> {
        let jobs: Vec<Job> = jobs.into_iter().filter(|j| !j.tiles.is_empty()).collect();
        if jobs.is_empty() {
            return None;
        }
        let t = &self.totals;
        t.submits.fetch_add(1, Relaxed);
        t.jobs.fetch_add(jobs.len() as u64, Relaxed);
        // values go down the channel in order
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        let v = self.issued.fetch_add(1, Relaxed) + 1;
        let _ = tx.send(Msg::Work(v, Instant::now(), jobs));
        Some(v)
    }

    /// The last value handed out.
    pub fn issued(&self) -> u64 {
        self.issued.load(Relaxed)
    }

    /// Make `queue`'s next work wait (on the GPU) for every mapping submitted so far.
    pub fn wait(&self, queue: &ID3D12CommandQueue) {
        let v = self.issued();
        if v == 0 {
            return;
        }
        // SAFETY: a fence of ours
        if unsafe { self.fence.GetCompletedValue() } >= v {
            self.totals.passed.fetch_add(1, Relaxed);
            return;
        }
        self.totals.waits.fetch_add(1, Relaxed);
        // SAFETY: a queue and a fence on one device
        let _ = unsafe { queue.Wait(&self.fence, v) };
    }

    /// Block until everything submitted so far has run on the GPU (tests, benches).
    pub fn drain(&self) -> Result<(), String> {
        let v = self.issued();
        // SAFETY: a null event blocks until the value is reached
        unsafe { self.fence.SetEventOnCompletion(v, HANDLE::default()) }.map_err(|e| format!("tile fence wait: {e}"))
    }
}

impl Drop for TileQueue {
    fn drop(&mut self) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(Msg::Stop);
        if let Some(t) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = t.join();
        }
    }
}

struct Wrap<T>(T);
// SAFETY: D3D12 queues and fences are free-threaded.
unsafe impl<T> Send for Wrap<T> {}

fn run(queue: ID3D12CommandQueue, fence: ID3D12Fence, rx: Receiver<Msg>, heaps: Heaps, t: Arc<Totals>) {
    let mut cache: Vec<Option<ID3D12Heap>> = Vec::new();
    // resources kept until the fence passes the value of their last job
    let mut alive: VecDeque<(u64, ID3D12Resource)> = VecDeque::new();
    let mut stop = false;
    while !stop {
        let first = match rx.recv() {
            Ok(m) => m,
            Err(_) => break,
        };
        let mut work = vec![first];
        while let Ok(m) = rx.try_recv() {
            work.push(m);
        }
        let mut last = 0u64;
        let mut oldest: Option<Instant> = None;
        // the jobs merged per resource, in the order each resource first comes
        let mut merged: Vec<Job> = Vec::new();
        let mut index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for m in work {
            let (v, at, jobs) = match m {
                Msg::Work(v, at, jobs) => (v, at, jobs),
                Msg::Stop => {
                    stop = true;
                    continue;
                }
            };
            oldest = Some(oldest.map_or(at, |o: Instant| o.min(at)));
            for j in jobs {
                let raw = windows::core::Interface::as_raw(&j.resource) as usize;
                match index.get(&raw) {
                    Some(&k) => {
                        merged[k].tiles.extend(j.tiles);
                        t.merged.fetch_add(1, Relaxed);
                    }
                    None => {
                        index.insert(raw, merged.len());
                        merged.push(j);
                    }
                }
            }
            last = v;
        }
        let started = Instant::now();
        t.in_pass_since.store(epoch().elapsed().as_nanos() as u64 + 1, Relaxed);
        for j in merged {
            for c in plan(&j.tiles) {
                t.calls.fetch_add(1, Relaxed);
                t.ranges.fetch_add(c.ranges.len() as u64, Relaxed);
                t.tiles.fetch_add(u64::from(c.tiles()), Relaxed);
                issue(&queue, &j.resource, &c, &mut cache, &heaps, &t);
            }
            alive.push_back((last, j.resource));
        }
        t.call_ns.fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        t.in_pass_since.store(0, Relaxed);
        if last > 0 {
            // SAFETY: a queue and a fence of ours
            let _ = unsafe { queue.Signal(&fence, last) };
            t.signalled.store(last, Relaxed);
            t.passes.fetch_add(1, Relaxed);
            if let Some(o) = oldest {
                let ns = o.elapsed().as_nanos() as u64;
                t.queued_ns.fetch_add(ns, Relaxed);
                t.queued_max_ns.fetch_max(ns, Relaxed);
            }
        }
        // SAFETY: a fence of ours
        let done = unsafe { fence.GetCompletedValue() };
        while alive.front().is_some_and(|(v, _)| *v <= done) {
            alive.pop_front();
        }
    }
    // the last jobs' resources outlive their calls
    // SAFETY: a null event blocks until the value is reached
    let _ = unsafe { fence.SetEventOnCompletion(alive.back().map_or(0, |a| a.0), HANDLE::default()) };
}

fn issue(queue: &ID3D12CommandQueue, resource: &ID3D12Resource, c: &Call, cache: &mut Vec<Option<ID3D12Heap>>, heaps: &Heaps, t: &Totals) {
    let heap = match c.heap {
        None => None,
        Some(h) => {
            if cache.len() <= h as usize {
                cache.resize(h as usize + 1, None);
            }
            if cache[h as usize].is_none() {
                cache[h as usize] = heaps(h);
            }
            match &cache[h as usize] {
                Some(x) => Some(x.clone()),
                None => {
                    t.no_heap.fetch_add(1, Relaxed);
                    return;
                }
            }
        }
    };
    let coords: Vec<D3D12_TILED_RESOURCE_COORDINATE> =
        c.starts.iter().map(|&(sub, [x, y, z])| D3D12_TILED_RESOURCE_COORDINATE { X: x, Y: y, Z: z, Subresource: sub }).collect();
    let sizes: Vec<D3D12_TILE_REGION_SIZE> = c.ranges.iter().map(|r| D3D12_TILE_REGION_SIZE { NumTiles: r.0, UseBox: false.into(), Width: 0, Height: 0, Depth: 0 }).collect();
    let flags: Vec<D3D12_TILE_RANGE_FLAGS> = c.ranges.iter().map(|r| if r.1.is_some() { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_NULL }).collect();
    let offsets: Vec<u32> = c.ranges.iter().map(|r| r.1.unwrap_or(0)).collect();
    let counts: Vec<u32> = c.ranges.iter().map(|r| r.0).collect();
    // SAFETY: regions and ranges one to one, each array `ranges.len()` long; a heap of the same device
    unsafe {
        queue.UpdateTileMappings(
            resource,
            coords.len() as u32,
            Some(coords.as_ptr()),
            Some(sizes.as_ptr()),
            heap.as_ref(),
            flags.len() as u32,
            Some(flags.as_ptr()),
            heap.as_ref().map(|_| offsets.as_ptr()),
            Some(counts.as_ptr()),
            D3D12_TILE_MAPPING_FLAG_NONE,
        )
    };
}

/// The run's tile queue (on the host device, made on first use), for `crate::mapped`.
pub fn get() -> Option<&'static TileQueue> {
    static Q: OnceLock<Option<TileQueue>> = OnceLock::new();
    Q.get_or_init(|| {
        let d = crate::device::get()?;
        let heaps: Heaps = Arc::new(crate::memory::tile_heap);
        match TileQueue::new(&d.host, queue_type(), heaps) {
            Ok(q) => Some(q),
            Err(e) => {
                eprintln!("tiles     no tile queue ({e}): reserved textures stay off");
                None
            }
        }
    })
    .as_ref()
}

/// Title submissions wait for the mappings submitted so far (from `crate::memory::flush_wait`).
pub fn wait(queue: &ID3D12CommandQueue) {
    static MADE: OnceLock<()> = OnceLock::new();
    // nothing to wait for until a mapping was submitted (the queue is made by the first)
    if MADE.get().is_none() {
        if !crate::mapped::enabled() || crate::mapped::MADE.load(Relaxed) == 0 {
            return;
        }
        let _ = MADE.set(());
    }
    if let Some(q) = get() {
        q.wait(queue);
    }
}

fn epoch() -> Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    *E.get_or_init(Instant::now)
}

/// For the run report.
pub fn report(q: &TileQueue) -> String {
    let t = &q.totals;
    let since = t.in_pass_since.load(Relaxed);
    let pass = if since == 0 { "idle".to_string() } else { format!("in a pass for {:.0} ms", (epoch().elapsed().as_nanos() as u64).saturating_sub(since - 1) as f64 / 1e6) };
    // SAFETY: a fence of ours
    let completed = unsafe { q.fence.GetCompletedValue() };
    let calls = t.calls.load(Relaxed).max(1);
    let passes = t.passes.load(Relaxed).max(1);
    format!(
        "{} submits, {} jobs ({} merged into an earlier one), {} UpdateTileMappings ({} ranges, {} tiles; {:.1} tiles a call) in {} passes; {:.1} us of CPU a call on the tile thread (planning included); submit to signal {:.2} ms average, {:.1} ms worst; {} without a heap; title waits {} ({} found it passed); now: issued {}, signalled {}, the GPU past {}, thread {}",
        t.submits.load(Relaxed),
        t.jobs.load(Relaxed),
        t.merged.load(Relaxed),
        t.calls.load(Relaxed),
        t.ranges.load(Relaxed),
        t.tiles.load(Relaxed),
        t.tiles.load(Relaxed) as f64 / calls as f64,
        t.passes.load(Relaxed),
        t.call_ns.load(Relaxed) as f64 / 1000.0 / calls as f64,
        t.queued_ns.load(Relaxed) as f64 / 1e6 / passes as f64,
        t.queued_max_ns.load(Relaxed) as f64 / 1e6,
        t.no_heap.load(Relaxed),
        t.waits.load(Relaxed),
        t.passed.load(Relaxed),
        q.issued(),
        t.signalled.load(Relaxed),
        completed,
        pass,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sub: u32, index: u32, card: Option<u32>) -> TexTile {
        TexTile { sub, coord: [index, 0, 0], index, card }
    }

    #[test]
    fn contiguous_tiles_onto_contiguous_pages_are_one_range_and_nulls_join_the_heap_call() {
        let tiles = [t(0, 0, Some(10)), t(0, 1, Some(11)), t(0, 2, Some(12)), t(0, 3, None), t(0, 4, None), t(1, 0, Some(13)), t(1, 1, Some(40))];
        let calls = plan(&tiles);
        assert_eq!(calls.len(), 1, "{calls:?}");
        let c = &calls[0];
        assert_eq!(c.heap, Some(0));
        assert_eq!(c.ranges, vec![(3, Some(10)), (2, None), (1, Some(13)), (1, Some(40))]);
        assert_eq!(c.starts, vec![(0, [0, 0, 0]), (0, [3, 0, 0]), (1, [0, 0, 0]), (1, [1, 0, 0])]);
        assert_eq!(c.tiles(), 7);
    }

    #[test]
    fn one_call_per_heap_and_an_all_null_call_has_no_heap() {
        let h = HEAP_TILES;
        let calls = plan(&[t(0, 0, Some(h - 1)), t(0, 1, Some(h)), t(0, 2, None)]);
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[0].heap, calls[0].ranges.clone()), (Some(0), vec![(1, Some(h - 1)), (1, None)]));
        assert_eq!((calls[1].heap, calls[1].ranges.clone()), (Some(1), vec![(1, Some(0))]));
        // a tile given twice: the later wins
        let twice = plan(&[t(0, 0, Some(5)), t(0, 1, Some(9)), t(0, 0, None)]);
        assert_eq!(twice.len(), 1);
        assert_eq!(twice[0].ranges, vec![(1, None), (1, Some(9))]);
        let nulls = plan(&[t(2, 5, None), t(2, 4, None), t(2, 9, None)]);
        assert_eq!(nulls.len(), 1);
        assert_eq!(nulls[0].heap, None);
        // unsorted input is sorted: 4 and 5 are one run
        assert_eq!(nulls[0].ranges, vec![(2, None), (1, None)]);
        assert!(plan(&[]).is_empty());
    }
}
