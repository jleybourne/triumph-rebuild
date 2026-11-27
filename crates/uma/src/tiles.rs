//! The GPU side of the address space: each 4 GiB window is one reserved (tiled) buffer, and
//! each page the title's resources cover is a 64 KiB tile mapped onto a card page in a pool of
//! 64 MiB heaps. `Uma` decides which page maps to which card page; this module does the mapping.
//!
//! Another reserved resource placed at the same console address (a second buffer, a render
//! target) maps its tiles onto the same card pages, so the two see each other's writes — the
//! console's sharing by address, with no copy.

#![cfg(windows)]

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::{Backed, PAGE, WINDOW};

/// card pages (tiles) per heap: 64 MiB heaps
pub const HEAP_TILES: u32 = 1024;

pub struct Tiles {
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    pub windows: Vec<ID3D12Resource>,
    heaps: Vec<ID3D12Heap>,
    pub tier: i32,
}

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

pub fn buffer_desc(bytes: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    }
}

/// A run of consecutive pages backed by consecutive card pages in one heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    window: usize,
    tile: u32,
    heap: u32,
    offset: u32,
    count: u32,
}

fn runs(pages: &[Backed]) -> Vec<Run> {
    let per_window = (WINDOW / PAGE) as u32;
    let mut sorted = pages.to_vec();
    sorted.sort_by_key(|b| b.page);
    let mut out: Vec<Run> = Vec::new();
    for b in sorted {
        let window = b.page / per_window as usize;
        let tile = (b.page % per_window as usize) as u32;
        let (heap, offset) = (b.card / HEAP_TILES, b.card % HEAP_TILES);
        if let Some(r) = out.last_mut() {
            if r.window == window && r.heap == heap && r.tile + r.count == tile && r.offset + r.count == offset {
                r.count += 1;
                continue;
            }
        }
        out.push(Run { window, tile, heap, offset, count: 1 });
    }
    out
}

impl Tiles {
    pub fn new(device: &ID3D12Device, queue: &ID3D12CommandQueue, windows: usize) -> Result<Tiles, String> {
        unsafe {
            let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
            device
                .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut _, std::mem::size_of_val(&o) as u32)
                .map_err(err("options"))?;
            let tier = o.TiledResourcesTier.0;
            if tier < D3D12_TILED_RESOURCES_TIER_2.0 {
                return Err(format!("tiled resources tier {tier}: tier 2 is required (unmapped reads must return zero)"));
            }
            let mut ws = Vec::with_capacity(windows);
            for _ in 0..windows {
                let mut r: Option<ID3D12Resource> = None;
                device
                    .CreateReservedResource(&buffer_desc(WINDOW as u64), D3D12_RESOURCE_STATE_COMMON, None, &mut r)
                    .map_err(err("reserved window"))?;
                ws.push(r.ok_or("reserved window: none")?);
            }
            Ok(Tiles { device: device.clone(), queue: queue.clone(), windows: ws, heaps: Vec::new(), tier })
        }
    }

    /// Card memory the heaps hold, bytes.
    pub fn heap_bytes(&self) -> u64 {
        self.heaps.len() as u64 * HEAP_TILES as u64 * PAGE as u64
    }

    fn ensure_heap(&mut self, heap: u32) -> Result<(), String> {
        while self.heaps.len() <= heap as usize {
            let mut h: Option<ID3D12Heap> = None;
            unsafe {
                self.device.CreateHeap(
                    &D3D12_HEAP_DESC {
                        SizeInBytes: HEAP_TILES as u64 * PAGE as u64,
                        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                        Alignment: PAGE as u64,
                        Flags: D3D12_HEAP_FLAG_NONE,
                    },
                    &mut h,
                )
            }
            .map_err(|e| format!("tile heap {} ({} MiB so far): {e}", self.heaps.len(), self.heap_bytes() >> 20))?;
            self.heaps.push(h.ok_or("tile heap: none")?);
        }
        Ok(())
    }

    /// Map the given pages of the windows onto their card pages.
    pub fn map(&mut self, pages: &[Backed]) -> Result<(), String> {
        let rs = runs(pages);
        if let Some(max) = rs.iter().map(|r| r.heap).max() {
            self.ensure_heap(max)?;
        }
        // one call per (window, heap): regions and ranges paired one to one
        let mut i = 0;
        while i < rs.len() {
            let (w, h) = (rs[i].window, rs[i].heap);
            let group: Vec<Run> = rs[i..].iter().take_while(|r| r.window == w && r.heap == h).copied().collect();
            i += group.len();
            self.update(&self.windows[w].clone(), &group, Some(h))?;
        }
        Ok(())
    }

    /// Unmap pages: reads of them return zero, writes are dropped.
    pub fn unmap(&mut self, pages: &[Backed]) -> Result<(), String> {
        let rs = runs(pages);
        let mut i = 0;
        while i < rs.len() {
            let w = rs[i].window;
            let group: Vec<Run> = rs[i..].iter().take_while(|r| r.window == w).copied().collect();
            i += group.len();
            self.update(&self.windows[w].clone(), &group, None)?;
        }
        Ok(())
    }

    /// Map tiles `first..` of another reserved resource onto the same card pages as the window
    /// pages listed: that resource now shares those bytes by address.
    pub fn alias(&mut self, resource: &ID3D12Resource, first: u32, cards: &[u32]) -> Result<(), String> {
        let mut rs: Vec<Run> = Vec::new();
        for (k, &card) in cards.iter().enumerate() {
            let (heap, offset) = (card / HEAP_TILES, card % HEAP_TILES);
            let tile = first + k as u32;
            if let Some(r) = rs.last_mut() {
                if r.heap == heap && r.tile + r.count == tile && r.offset + r.count == offset {
                    r.count += 1;
                    continue;
                }
            }
            rs.push(Run { window: 0, tile, heap, offset, count: 1 });
        }
        let mut i = 0;
        while i < rs.len() {
            let h = rs[i].heap;
            let group: Vec<Run> = rs[i..].iter().take_while(|r| r.heap == h).copied().collect();
            i += group.len();
            self.update(resource, &group, Some(h))?;
        }
        Ok(())
    }

    fn update(&self, resource: &ID3D12Resource, group: &[Run], heap: Option<u32>) -> Result<(), String> {
        let coords: Vec<D3D12_TILED_RESOURCE_COORDINATE> =
            group.iter().map(|r| D3D12_TILED_RESOURCE_COORDINATE { X: r.tile, Y: 0, Z: 0, Subresource: 0 }).collect();
        let sizes: Vec<D3D12_TILE_REGION_SIZE> =
            group.iter().map(|r| D3D12_TILE_REGION_SIZE { NumTiles: r.count, UseBox: false.into(), Width: 0, Height: 0, Depth: 0 }).collect();
        unsafe {
            match heap {
                Some(h) => {
                    let offsets: Vec<u32> = group.iter().map(|r| r.offset).collect();
                    let counts: Vec<u32> = group.iter().map(|r| r.count).collect();
                    let flags = vec![D3D12_TILE_RANGE_FLAG_NONE; group.len()];
                    self.queue.UpdateTileMappings(
                        resource,
                        group.len() as u32,
                        Some(coords.as_ptr()),
                        Some(sizes.as_ptr()),
                        &self.heaps[h as usize],
                        group.len() as u32,
                        Some(flags.as_ptr()),
                        Some(offsets.as_ptr()),
                        Some(counts.as_ptr()),
                        D3D12_TILE_MAPPING_FLAG_NONE,
                    );
                }
                None => {
                    let total: u32 = group.iter().map(|r| r.count).sum();
                    let flag = D3D12_TILE_RANGE_FLAG_NULL;
                    self.queue.UpdateTileMappings(
                        resource,
                        group.len() as u32,
                        Some(coords.as_ptr()),
                        Some(sizes.as_ptr()),
                        None::<&ID3D12Heap>,
                        1,
                        Some(&flag),
                        None,
                        Some(&total),
                        D3D12_TILE_MAPPING_FLAG_NONE,
                    );
                }
            }
            self.device.GetDeviceRemovedReason().map_err(err("device removed after UpdateTileMappings"))
        }
    }

    /// The GPU address of a console address.
    pub fn va(&self, address: u64) -> u64 {
        let w = address as usize / WINDOW;
        unsafe { self.windows[w].GetGPUVirtualAddress() + (address as usize % WINDOW) as u64 }
    }

    /// The window resource and offset of a console address.
    pub fn locate(&self, address: u64) -> (&ID3D12Resource, u64) {
        let w = address as usize / WINDOW;
        (&self.windows[w], (address as usize % WINDOW) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_pages_on_consecutive_card_pages_are_one_run() {
        let per_window = WINDOW / PAGE;
        let pages = [
            Backed { page: 5, card: 10 },
            Backed { page: 3, card: 8 },
            Backed { page: 4, card: 9 },
            Backed { page: 6, card: 40 },
            Backed { page: per_window + 1, card: 41 },
            Backed { page: 7, card: HEAP_TILES },
        ];
        let r = runs(&pages);
        assert_eq!(
            r,
            vec![
                Run { window: 0, tile: 3, heap: 0, offset: 8, count: 3 },
                Run { window: 0, tile: 6, heap: 0, offset: 40, count: 1 },
                Run { window: 0, tile: 7, heap: 1, offset: 0, count: 1 },
                Run { window: 1, tile: 1, heap: 0, offset: 41, count: 1 },
            ]
        );
    }
}
