//! Host textures the GPU writes (render targets, depth, UAV textures) at title addresses, placed in host heaps so
//! textures the title puts over the same memory share host memory, as they share the console's.
//!
//! The title aliases its targets the console way: two targets (or a target and a UAV texture) at overlapping
//! addresses, used one after the other, each initialised by the title after the switch (a clear, a discard or a
//! full write: RDNA2's compression metadata needs that as the host's does). A committed host texture each paid for
//! both. Here such a texture joins the heap of a live one whose console memory it overlaps, at the same distance
//! from the heap's start as on the console, when its host footprint fits in that heap and overlaps there only
//! textures it also overlaps on the console; otherwise it gets a heap of its own (what a committed texture costs).
//!
//! ```text
//! console   A [0x000 ............ 0x400)          B [0x100 ..... 0x380)      (B over A: the title aliases them)
//! host      heap of A: A at 0, B at 0x100          (B's host bytes fit before the heap's end)
//! ```
//!
//! A heap goes when its last texture goes, two seconds later (the GPU may still be reading it). Heaps of GPU-written
//! textures get the high residency priority: past the card's budget the driver demotes sampled textures first.
//!
//! **Off unless `REBUILD_ALIAS_TARGETS=1`.** Measured on WARP through the world load (run `warp1`, vram3): 1,191
//! textures placed, 104 into a live texture's heap, 3,912 MiB of heaps for 4,001 MiB of textures: 89 MiB saved,
//! because the title's live targets hardly overlap (vram2's census: 2.8 GB of targets in its other memory cover
//! 2.8 GB). And the debug layer flagged placed targets drawn before any clear, discard or copy (#1422): a placed
//! target's compression metadata starts undefined, which a committed one's does not. Not worth that for 89 MiB.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Textures placed in heaps here, those that joined a live texture's heap, and live: heaps, their bytes, and the
/// host bytes of the textures in them (what committed textures would take).
pub static PLACED: AtomicU64 = AtomicU64::new(0);
pub static JOINED: AtomicU64 = AtomicU64::new(0);
pub static HEAPS: AtomicU64 = AtomicU64::new(0);
pub static HEAP_BYTES: AtomicU64 = AtomicU64::new(0);
pub static TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);

/// How long an emptied heap is kept.
const GRACE: Duration = Duration::from_secs(2);

/// Off unless `REBUILD_ALIAS_TARGETS=1` (see the module notes). Read once.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("REBUILD_ALIAS_TARGETS").is_ok_and(|v| v.trim() == "1"))
}

/// A texture in a heap: its offset there, its host bytes, its console range, its host pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub offset: u64,
    pub bytes: u64,
    pub console: (u64, u64),
    pub host: usize,
}

/// A heap whose byte `o` stands for console address `base + o`.
pub struct Group<H> {
    pub heap: H,
    pub size: u64,
    pub align: u64,
    /// the heap takes only render targets and depth (resource heap tier 1), or only other textures
    pub kind: u8,
    pub members: Vec<Member>,
}

/// Live heaps by console base.
pub struct Groups<H> {
    groups: BTreeMap<u64, Vec<Group<H>>>,
    longest: u64,
    by_host: HashMap<usize, u64>,
    dead: VecDeque<(Instant, H)>,
}

impl<H> Default for Groups<H> {
    fn default() -> Self {
        Groups { groups: BTreeMap::new(), longest: 0, by_host: HashMap::new(), dead: VecDeque::new() }
    }
}

fn overlap(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

impl<H: Clone> Groups<H> {
    /// A live heap a texture of `bytes` host bytes at `align`, console range `console`, of `kind` can join: (its
    /// console base, its index there, the texture's offset in it, the heap). It must overlap a member on the
    /// console, and every member it overlaps in the heap.
    pub fn find(&self, console: (u64, u64), bytes: u64, align: u64, kind: u8) -> Option<(u64, usize, u64, H)> {
        let address = console.0;
        for (&base, list) in self.groups.range(address.saturating_sub(self.longest)..=address) {
            let off = address - base;
            for (i, g) in list.iter().enumerate() {
                if g.kind != kind || align > g.align || off % align != 0 || off + bytes > g.size {
                    continue;
                }
                let mine = (off, off + bytes);
                let mut shares = false;
                let fits = g.members.iter().all(|m| {
                    let on_console = overlap(console, m.console);
                    shares |= on_console;
                    !overlap(mine, (m.offset, m.offset + m.bytes)) || on_console
                });
                if fits && shares {
                    return Some((base, i, off, g.heap.clone()));
                }
            }
        }
        None
    }

    /// A texture placed in heap `index` at console base `base` (from [`Groups::find`]).
    pub fn join(&mut self, base: u64, index: usize, m: Member) {
        if let Some(g) = self.groups.get_mut(&base).and_then(|l| l.get_mut(index)) {
            g.members.push(m);
            self.by_host.insert(m.host, base);
            PLACED.fetch_add(1, Relaxed);
            JOINED.fetch_add(1, Relaxed);
            TEXTURE_BYTES.fetch_add(m.bytes, Relaxed);
        }
    }

    /// A heap of its own for a texture at console address `base` (its offset 0).
    pub fn create(&mut self, base: u64, heap: H, size: u64, align: u64, kind: u8, m: Member) {
        self.longest = self.longest.max(size);
        self.groups.entry(base).or_default().push(Group { heap, size, align, kind, members: vec![m] });
        self.by_host.insert(m.host, base);
        PLACED.fetch_add(1, Relaxed);
        HEAPS.fetch_add(1, Relaxed);
        HEAP_BYTES.fetch_add(size, Relaxed);
        TEXTURE_BYTES.fetch_add(m.bytes, Relaxed);
    }

    /// The texture `host` is gone; its heap goes (after [`GRACE`]) when nothing else is in it. False when the
    /// texture was not placed here.
    pub fn leave(&mut self, host: usize, now: Instant) -> bool {
        let Some(base) = self.by_host.remove(&host) else { return false };
        let Some(list) = self.groups.get_mut(&base) else { return false };
        let Some(gi) = list.iter().position(|g| g.members.iter().any(|m| m.host == host)) else { return false };
        let g = &mut list[gi];
        let mi = g.members.iter().position(|m| m.host == host).expect("a member");
        TEXTURE_BYTES.fetch_sub(g.members[mi].bytes, Relaxed);
        g.members.swap_remove(mi);
        if g.members.is_empty() {
            let g = list.swap_remove(gi);
            HEAPS.fetch_sub(1, Relaxed);
            HEAP_BYTES.fetch_sub(g.size, Relaxed);
            self.dead.push_back((now, g.heap));
            if list.is_empty() {
                self.groups.remove(&base);
            }
        }
        true
    }

    /// Heaps emptied more than [`GRACE`] before `now` are let go.
    pub fn reap(&mut self, now: Instant) {
        while self.dead.front().is_some_and(|(t, _)| now.duration_since(*t) >= GRACE) {
            self.dead.pop_front();
        }
    }

    pub fn dead(&self) -> usize {
        self.dead.len()
    }
}

/// One line for the VRAM line.
pub fn report() -> String {
    format!(
        "aliased targets: {} placed ({} into a live texture's heap); {} heaps live, {} MiB, for {} MiB of textures",
        PLACED.load(Relaxed),
        JOINED.load(Relaxed),
        HEAPS.load(Relaxed),
        HEAP_BYTES.load(Relaxed) >> 20,
        TEXTURE_BYTES.load(Relaxed) >> 20
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: u64 = 64 << 10;

    fn m(offset: u64, bytes: u64, console: (u64, u64), host: usize) -> Member {
        Member { offset, bytes, console, host }
    }

    #[test]
    fn a_target_over_a_live_one_joins_its_heap_at_the_console_distance() {
        let mut g: Groups<u32> = Groups::default();
        // A: console 0x100000..+64 K*16, host 64 K*17 (the host's is larger)
        let a = (0x10_0000, 0x10_0000 + 16 * K);
        g.create(a.0, 1, 17 * K, K, 0, m(0, 17 * K, a, 0xa));
        // B over A from 4 pages in, 8 pages long: placed at 4 pages
        let b = (a.0 + 4 * K, a.0 + 12 * K);
        let (base, i, off, heap) = g.find(b, 8 * K, K, 0).expect("joins");
        assert_eq!((base, i, off, heap), (a.0, 0, 4 * K, 1));
        g.join(base, i, m(off, 8 * K, b, 0xb));
        // C beside A on the console (after its end) but inside A's host tail: it would overlap A's host bytes
        // without overlapping A on the console, so it may not join
        let c = (a.1, a.1 + K);
        assert!(g.find(c, K, K, 0).is_none());
        // too large for the heap, misaligned, or of the other kind: no
        assert!(g.find((a.0 + 8 * K, a.0 + 16 * K), 10 * K, K, 0).is_none());
        assert!(g.find((a.0 + K / 2, a.0 + 2 * K), K, K, 0).is_none());
        assert!(g.find(b, 8 * K, K, 1).is_none());
        // nothing there on the console: a heap of its own
        assert!(g.find((0x90_0000, 0x90_0000 + K), K, K, 0).is_none());
    }

    #[test]
    fn a_heap_goes_after_its_last_texture_and_a_grace() {
        let mut g: Groups<u32> = Groups::default();
        let a = (0x20_0000, 0x20_0000 + 4 * K);
        let (heaps, bytes) = (HEAPS.load(Relaxed), HEAP_BYTES.load(Relaxed));
        g.create(a.0, 7, 4 * K, K, 0, m(0, 4 * K, a, 1));
        let (base, i, off, _) = g.find(a, 4 * K, K, 0).expect("the same memory");
        g.join(base, i, m(off, 4 * K, a, 2));
        let t = Instant::now();
        assert!(g.leave(1, t));
        assert_eq!(g.dead(), 0, "still holds texture 2");
        assert!(g.leave(2, t));
        assert!(!g.leave(2, t), "once");
        assert_eq!(g.dead(), 1);
        g.reap(t + Duration::from_millis(100));
        assert_eq!(g.dead(), 1, "within the grace");
        g.reap(t + GRACE);
        assert_eq!(g.dead(), 0);
        assert!(g.find(a, 4 * K, K, 0).is_none(), "the heap is gone");
        // other tests count too: only that this one's heap came and went
        assert!(HEAPS.load(Relaxed) <= heaps + 8 && HEAP_BYTES.load(Relaxed) <= bytes + (1 << 30));
    }
}
