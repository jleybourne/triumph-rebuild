//! The title run's GPU address space (`docs/design/memory-run.md`): one reserved buffer per 512 MiB
//! **sub-window** of console addresses, made the first time a page in it is mapped.
//!
//! 512 MiB is what one raw view covers (2^27 R32 elements), so a sub-window is exactly one SRV and one UAV in
//! the view heap; it is also a size WARP can reserve (it cannot reserve 4 GiB). `Uma` still decides which
//! card page backs which page of the address space; this maps them, on one queue (the ring's), so a queue that
//! waits for the ring's fence is ordered after every mapping.

#![cfg(windows)]

use windows::Win32::Graphics::Direct3D12::*;

use crate::tiles::{buffer_desc, runs_in, Run, HEAP_TILES};
use crate::{Backed, PAGE};

/// Bytes of console address space per reserved buffer (and per window view).
pub const SUB: u64 = 512 << 20;
/// Sub-windows the view heap holds (`WINDOW_HEAP_BASE .. + 32768`): 16 TiB of address space.
pub const SUBS: usize = 1 << 15;

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

pub struct Space {
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    subs: Vec<Option<ID3D12Resource>>,
    heaps: Vec<ID3D12Heap>,
    created: Vec<u32>,
}

// SAFETY: D3D12 objects are free-threaded; the owner serializes calls (a lock).
unsafe impl Send for Space {}
unsafe impl Sync for Space {}

impl Space {
    /// `queue` is where tile mappings run (the ring's queue).
    pub fn new(device: &ID3D12Device, queue: &ID3D12CommandQueue) -> Result<Space, String> {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut _, std::mem::size_of_val(&o) as u32) }.map_err(err("options"))?;
        if o.TiledResourcesTier.0 < D3D12_TILED_RESOURCES_TIER_2.0 {
            return Err(format!("tiled resources tier {}: tier 2 is required (unmapped reads must return zero)", o.TiledResourcesTier.0));
        }
        Ok(Space { device: device.clone(), queue: queue.clone(), subs: vec![None; SUBS], heaps: Vec::new(), created: Vec::new() })
    }

    /// Card memory the heaps hold, bytes.
    pub fn heap_bytes(&self) -> u64 {
        self.heaps.len() as u64 * HEAP_TILES as u64 * PAGE as u64
    }

    /// The reserved buffer of sub-window `k`, if made.
    pub fn sub(&self, k: usize) -> Option<&ID3D12Resource> {
        self.subs.get(k)?.as_ref()
    }

    /// The GPU address of sub-window `k`'s byte 0, if made.
    pub fn va(&self, k: usize) -> Option<u64> {
        self.sub(k).map(|r| unsafe { r.GetGPUVirtualAddress() })
    }

    /// Sub-windows made since the last call (their views are to be placed).
    pub fn take_created(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.created)
    }

    /// Sub-window `k`'s buffer, made if it is not yet.
    pub fn ensure(&mut self, k: usize) -> Result<ID3D12Resource, String> {
        if k >= SUBS {
            return Err(format!("sub-window {k} is past the {SUBS} the view heap holds (address >= 16 TiB)"));
        }
        if let Some(r) = &self.subs[k] {
            return Ok(r.clone());
        }
        let mut r: Option<ID3D12Resource> = None;
        unsafe { self.device.CreateReservedResource(&buffer_desc(SUB), D3D12_RESOURCE_STATE_COMMON, None, &mut r) }.map_err(err("reserved sub-window"))?;
        let r = r.ok_or("reserved sub-window: none")?;
        self.subs[k] = Some(r.clone());
        self.created.push(k as u32);
        Ok(r)
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

    /// Map the given pages onto their card pages, making sub-windows as needed.
    pub fn map(&mut self, pages: &[Backed]) -> Result<(), String> {
        let rs = runs_in(pages, SUB);
        if let Some(max) = rs.iter().map(|r| r.heap).max() {
            self.ensure_heap(max)?;
        }
        let mut i = 0;
        while i < rs.len() {
            let (w, h) = (rs[i].window, rs[i].heap);
            let group: Vec<Run> = rs[i..].iter().take_while(|r| r.window == w && r.heap == h).copied().collect();
            i += group.len();
            let sub = self.ensure(w)?;
            self.update(&sub, &group, Some(h))?;
        }
        Ok(())
    }

    /// Unmap pages: reads of them return zero, writes are dropped.
    pub fn unmap(&mut self, pages: &[Backed]) -> Result<(), String> {
        let rs = runs_in(pages, SUB);
        let mut i = 0;
        while i < rs.len() {
            let w = rs[i].window;
            let group: Vec<Run> = rs[i..].iter().take_while(|r| r.window == w).copied().collect();
            i += group.len();
            if let Some(sub) = self.sub(w).cloned() {
                self.update(&sub, &group, None)?;
            }
        }
        Ok(())
    }

    /// Map tiles `0..` of another reserved resource onto `cards` (consecutive pages): it shares those bytes.
    pub fn alias(&mut self, resource: &ID3D12Resource, cards: &[u32]) -> Result<(), String> {
        let mut rs: Vec<Run> = Vec::new();
        for (k, &card) in cards.iter().enumerate() {
            let (heap, offset) = (card / HEAP_TILES, card % HEAP_TILES);
            let tile = k as u32;
            if let Some(r) = rs.last_mut() {
                if r.heap == heap && r.tile + r.count == tile && r.offset + r.count == offset {
                    r.count += 1;
                    continue;
                }
            }
            rs.push(Run { window: 0, tile, heap, offset, count: 1 });
        }
        if let Some(max) = rs.iter().map(|r| r.heap).max() {
            self.ensure_heap(max)?;
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
        let coords: Vec<D3D12_TILED_RESOURCE_COORDINATE> = group.iter().map(|r| D3D12_TILED_RESOURCE_COORDINATE { X: r.tile, Y: 0, Z: 0, Subresource: 0 }).collect();
        let sizes: Vec<D3D12_TILE_REGION_SIZE> = group.iter().map(|r| D3D12_TILE_REGION_SIZE { NumTiles: r.count, UseBox: false.into(), Width: 0, Height: 0, Depth: 0 }).collect();
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
}
