//! What the host textures behind console textures cost in card memory, by where they came from.
//!
//! Every console texture gets a host texture of its own (console layouts are not the host's). Counted here at
//! creation and release, with the bytes the host says each takes (`GetResourceAllocationInfo`), so the run can
//! say how card memory splits between host textures and the memory model's tiles (`rebuild_runtime::memory`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

/// Where a host texture came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// a committed console resource (0xd8 / 0x3f8): console memory of its own
    Committed = 0,
    /// placed in a console heap (0xe8)
    Placed = 1,
    /// at an address the title mapped (0x428)
    AtAddress = 2,
    /// a view the title placed on its own memory (0x230 / 0x238), no console resource
    PlacedView = 3,
    /// a depth texture's colour twin (`docs/design/depth-uav.md`)
    Twin = 4,
}

pub const ORIGINS: usize = 5;
const NAMES: [&str; ORIGINS] = ["committed", "placed", "at address", "placed views", "twins"];

/// Live host textures and their bytes, and how many were ever made, by [`Origin`].
pub static LIVE: [AtomicU64; ORIGINS] = [const { AtomicU64::new(0) }; ORIGINS];
pub static BYTES: [AtomicU64; ORIGINS] = [const { AtomicU64::new(0) }; ORIGINS];
pub static MADE: [AtomicU64; ORIGINS] = [const { AtomicU64::new(0) }; ORIGINS];
/// The console footprint bytes behind the live ones (what the title's memory holds for them).
pub static CONSOLE: [AtomicU64; ORIGINS] = [const { AtomicU64::new(0) }; ORIGINS];
/// The most bytes live at once (all origins).
pub static PEAK: AtomicU64 = AtomicU64::new(0);

/// A live host texture: its console range, origin, card bytes, and whether the GPU writes it (target, depth or UAV).
#[derive(Clone, Copy, Debug)]
pub struct Live {
    pub address: u64,
    pub console: u64,
    pub origin: Origin,
    pub bytes: u64,
    pub gpu_written: bool,
}

/// Live host textures by host pointer (`REBUILD_VRAM_LOG` only: the census behind [`live`]).
static TABLE: Mutex<Option<HashMap<usize, Live>>> = Mutex::new(None);

fn census() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REBUILD_VRAM_LOG").is_some())
}

/// Record a host texture (`host`: its pointer) with its console range.
pub fn track(host: usize, l: Live) {
    if census() {
        TABLE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(host, l);
    }
}

pub fn untrack(host: usize) {
    if census() {
        if let Some(t) = TABLE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            t.remove(&host);
        }
    }
}

/// The live host textures (empty without `REBUILD_VRAM_LOG`).
pub fn live() -> Vec<Live> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or_else(Vec::new, |t| t.values().copied().collect())
}

/// Bytes covered by the union of `ranges` (start, end).
pub fn union(mut ranges: Vec<(u64, u64)>) -> u64 {
    ranges.sort_unstable();
    let (mut total, mut cur) = (0u64, None::<(u64, u64)>);
    for (s, e) in ranges {
        match cur {
            Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total += ce - cs;
                cur = Some((s, e));
            }
            None => cur = Some((s, e)),
        }
    }
    total + cur.map_or(0, |(s, e)| e - s)
}

pub fn made(o: Origin, bytes: u64, console: u64) {
    let i = o as usize;
    CONSOLE[i].fetch_add(console, Relaxed);
    LIVE[i].fetch_add(1, Relaxed);
    MADE[i].fetch_add(1, Relaxed);
    BYTES[i].fetch_add(bytes, Relaxed);
    PEAK.fetch_max(total(), Relaxed);
}

pub fn gone(o: Origin, bytes: u64, console: u64) {
    let i = o as usize;
    CONSOLE[i].fetch_sub(console, Relaxed);
    LIVE[i].fetch_sub(1, Relaxed);
    BYTES[i].fetch_sub(bytes, Relaxed);
}

/// Resources put in system memory because the card had no room (`E_OUTOFMEMORY` on the default heap, or the card
/// at its budget).
pub static FELL_BACK: AtomicU64 = AtomicU64::new(0);

/// The card is at its budget (`QueryVideoMemoryInfo`, set by the lead's watcher): new textures nothing renders to go
/// to system memory instead of pushing what is in use out of the card.
pub static PRESSURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Under [`PRESSURE`], a default-heap resource of `desc` goes to system memory (render targets and depth stay: the
/// GPU writes them every frame).
pub fn to_system(desc: &windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DESC) -> bool {
    use windows::Win32::Graphics::Direct3D12::*;
    let written = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0;
    PRESSURE.load(Relaxed) && desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER && desc.Flags.0 & written == 0
}

pub fn fell_back(d: &windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DESC) {
    if FELL_BACK.fetch_add(1, Relaxed) < 8 {
        eprintln!(
            "vram      the card is full: a {}x{}x{} format {} mips {} resource goes to system memory instead",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0, d.MipLevels
        );
    }
}

/// Live host texture bytes, all origins.
pub fn total() -> u64 {
    BYTES.iter().map(|b| b.load(Relaxed)).sum()
}

/// One line: live count and MiB by origin (and ever made, when more).
pub fn report() -> String {
    let mut s = format!("{} MiB live (peak {} MiB):", total() >> 20, PEAK.load(Relaxed) >> 20);
    for (i, name) in NAMES.iter().enumerate() {
        let (live, made) = (LIVE[i].load(Relaxed), MADE[i].load(Relaxed));
        s += &format!(" {name} {live} / {} MiB (console {} MiB)", BYTES[i].load(Relaxed) >> 20, CONSOLE[i].load(Relaxed) >> 20);
        if made != live {
            s += &format!(" ({made} made)");
        }
        s.push(';');
    }
    s.pop();
    let f = FELL_BACK.load(Relaxed);
    if f > 0 {
        s += &format!("; {f} put in system memory (card full)");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_union_counts_overlapping_ranges_once() {
        assert_eq!(union(vec![]), 0);
        assert_eq!(union(vec![(0, 10)]), 10);
        // two textures at one address (a slot streamed into again) and one beside them
        assert_eq!(union(vec![(100, 200), (0, 50), (100, 150), (150, 260)]), 50 + 160);
        assert_eq!(union(vec![(0, 10), (10, 20)]), 20, "touching ranges join");
    }

    #[test]
    fn made_and_gone_balance_by_origin() {
        // other tests may count too: only what this one adds is checked
        let i = Origin::Twin as usize;
        let (live, bytes, console) = (LIVE[i].load(Relaxed), BYTES[i].load(Relaxed), CONSOLE[i].load(Relaxed));
        made(Origin::Twin, 1 << 20, 3 << 16);
        assert!(PEAK.load(Relaxed) >= 1 << 20);
        gone(Origin::Twin, 1 << 20, 3 << 16);
        assert_eq!((LIVE[i].load(Relaxed), BYTES[i].load(Relaxed), CONSOLE[i].load(Relaxed)), (live, bytes, console));
        assert!(report().contains("twins"));
    }
}
