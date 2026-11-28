//! Depth textures written as UAVs, end to end on WARP (`docs/design/depth-uav.md`): a console depth texture with
//! unordered access, its UAV made through the console call, and a console list that clears the depth, takes it
//! into unordered access, clears half of it through the UAV and takes it out again. The host depth must then hold
//! both clears: the draw-side value where the UAV did not write (the copy into the twin at the enter) and the UAV
//! value where it did (the copy back at the exit).
//!
//! `REBUILD_D3D12_DEBUG=1` runs it under the debug layer and fails on any error message (barrier layouts and
//! sync, copy formats).

use std::sync::Arc;

use windows::core::Interface;
use windows::Win32::Foundation::{RECT, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use rebuild_xlist::console::{create_allocator, create_list, segments, HostPipeline, PipelineSource, TWIN_COPIES};

use crate::format::HeapProperties;
use crate::memory::PlainMemory;
use crate::{Host, Quiet, Raw, Resources};

struct NoPipelines;
impl PipelineSource for NoPipelines {
    fn pipeline(&self, _: Raw) -> Option<HostPipeline> {
        None
    }
}

fn warp(debug: bool) -> Option<ID3D12Device> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory4};
    if debug {
        let mut d: Option<ID3D12Debug> = None;
        if unsafe { D3D12GetDebugInterface(&mut d) }.is_ok() {
            unsafe { d?.EnableDebugLayer() };
        }
    }
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.ok()?;
    let adapter: windows::core::IUnknown = unsafe { factory.EnumWarpAdapter() }.ok()?;
    let mut d: Option<ID3D12Device> = None;
    unsafe { D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut d) }.ok()?;
    d
}

fn method<F: Copy>(obj: Raw, offset: usize) -> F {
    unsafe {
        let table = *obj.cast::<*const *const ()>();
        std::mem::transmute_copy(&*table.add(offset / 8))
    }
}

const DEFAULT: HeapProperties = HeapProperties { kind: 1, cpu_pages: 0, pool: 0, creation_nodes: 1, visible_nodes: 1 };

/// `D3D12_TEXTURE_BARRIER` over every subresource of `resource` (the console's 64 bytes).
#[allow(clippy::too_many_arguments)]
fn texture_barrier(resource: Raw, sb: D3D12_BARRIER_SYNC, sa: D3D12_BARRIER_SYNC, ab: D3D12_BARRIER_ACCESS, aa: D3D12_BARRIER_ACCESS, lb: D3D12_BARRIER_LAYOUT, la: D3D12_BARRIER_LAYOUT) -> rebuild_xlist::console::twin::TextureBarrier {
    rebuild_xlist::console::twin::TextureBarrier {
        sync_before: sb.0,
        sync_after: sa.0,
        access_before: ab.0,
        access_after: aa.0,
        layout_before: lb.0,
        layout_after: la.0,
        resource,
        range: [u32::MAX, 0, 0, 0, 0, 0],
        flags: 0,
    }
}

#[repr(C)]
struct Group {
    kind: u32,
    count: u32,
    items: *const rebuild_xlist::console::twin::TextureBarrier,
}

fn barrier(list: Raw, b: rebuild_xlist::console::twin::TextureBarrier) {
    let g = Group { kind: 1, count: 1, items: &b };
    unsafe { method::<unsafe extern "system" fn(Raw, u32, *const Group)>(list, 0x490)(list, 1, &g) };
}

/// One run for console depth format `format` (UAV format `uav`): the depth values read back, left and right of the
/// half the UAV cleared, as floats.
fn run(device: &ID3D12Device, format: i32, dsv: DXGI_FORMAT, uav: DXGI_FORMAT, read: impl Fn(&[u8]) -> f32) -> (f32, f32, u64) {
    let mem = Arc::new(PlainMemory::new(16 << 20));
    let r = Resources::new(device, mem, Arc::new(Quiet)).expect("resources");
    let host = Arc::new(Host { resources: r.clone(), pipelines: NoPipelines });
    unsafe {
        // the depth texture, made as the title makes its shadow maps: depth + unordered access, starting in UAV
        let desc = D3D12_RESOURCE_DESC1 {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT(format),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAGS(0x8030_0006_u32 as i32),
            ..Default::default()
        };
        let mut tex: Raw = std::ptr::null_mut();
        let hr = r.create_committed3(&DEFAULT, 0, &desc, 3, std::ptr::null(), std::ptr::null_mut(), 0, std::ptr::null(), &ID3D12Resource::IID, &mut tex);
        assert_eq!(hr, S_OK);
        let t = r.uav_twin(tex).expect("a depth texture with UAVs answers a twin");
        assert!(!t.twin.is_null(), "format {format}: a twin");
        // descriptor heaps: shader-visible views, and depth views
        let mut views: Raw = std::ptr::null_mut();
        assert_eq!(r.create_descriptor_heap(&[0, 16, 1, 0], &ID3D12DescriptorHeap::IID, &mut views), S_OK);
        let mut dsvs: Raw = std::ptr::null_mut();
        assert_eq!(r.create_descriptor_heap(&[3, 4, 0, 0], &ID3D12DescriptorHeap::IID, &mut dsvs), S_OK);
        let (vstart, dstart) = (r.heap_state(views).unwrap().start, r.heap_state(dsvs).unwrap().start);
        let u = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: uav, ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D, ..Default::default() };
        r.create_uav(tex, std::ptr::null_mut(), &u, vstart);
        let dv = D3D12_DEPTH_STENCIL_VIEW_DESC { Format: dsv, ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D, ..Default::default() };
        r.create_dsv(tex, &dv, dstart);

        let a = create_allocator(0, device, host).expect("allocator");
        let l = create_list(0, a, std::ptr::null_mut()).expect("list");
        let copies = TWIN_COPIES.load(std::sync::atomic::Ordering::Relaxed);
        // out of the creation's UAV layout into depth write, then a depth clear (the draw side's write)
        barrier(l, texture_barrier(tex, D3D12_BARRIER_SYNC_NONE, D3D12_BARRIER_SYNC_DEPTH_STENCIL, D3D12_BARRIER_ACCESS_NO_ACCESS, D3D12_BARRIER_ACCESS_DEPTH_STENCIL_WRITE, D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS, D3D12_BARRIER_LAYOUT_DEPTH_STENCIL_WRITE));
        type ClearDsv = unsafe extern "system" fn(Raw, D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_CLEAR_FLAGS, f32, u8, u32, *const RECT);
        method::<ClearDsv>(l, 0x198)(l, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dstart as usize }, D3D12_CLEAR_FLAG_DEPTH, 0.25, 0, 0, std::ptr::null());
        // into unordered access; a UAV clear of the left half (the compute side's write); out to copy source
        barrier(l, texture_barrier(tex, D3D12_BARRIER_SYNC_DEPTH_STENCIL, D3D12_BARRIER_SYNC_CLEAR_UNORDERED_ACCESS_VIEW, D3D12_BARRIER_ACCESS_DEPTH_STENCIL_WRITE, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS, D3D12_BARRIER_LAYOUT_DEPTH_STENCIL_WRITE, D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS));
        let heaps = [views];
        method::<unsafe extern "system" fn(Raw, u32, *const Raw)>(l, 0x100)(l, 1, heaps.as_ptr());
        type ClearUav = unsafe extern "system" fn(Raw, u64, usize, Raw, *const f32, u32, *const RECT);
        let value = [0.75f32; 4];
        let left = RECT { left: 0, top: 0, right: 32, bottom: 64 };
        method::<ClearUav>(l, 0x1b0)(l, vstart, vstart as usize, tex, value.as_ptr(), 1, &left);
        barrier(l, texture_barrier(tex, D3D12_BARRIER_SYNC_CLEAR_UNORDERED_ACCESS_VIEW, D3D12_BARRIER_SYNC_COPY, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS, D3D12_BARRIER_ACCESS_COPY_SOURCE, D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS, D3D12_BARRIER_LAYOUT_COPY_SOURCE));
        // a second phase with no work in it: entered and left in this list, nothing copied back
        barrier(l, texture_barrier(tex, D3D12_BARRIER_SYNC_COPY, D3D12_BARRIER_SYNC_COMPUTE_SHADING, D3D12_BARRIER_ACCESS_COPY_SOURCE, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS, D3D12_BARRIER_LAYOUT_COPY_SOURCE, D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS));
        barrier(l, texture_barrier(tex, D3D12_BARRIER_SYNC_COMPUTE_SHADING, D3D12_BARRIER_SYNC_COPY, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS, D3D12_BARRIER_ACCESS_COPY_SOURCE, D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS, D3D12_BARRIER_LAYOUT_COPY_SOURCE));
        assert_eq!(method::<unsafe extern "system" fn(Raw) -> windows::core::HRESULT>(l, 0x48)(l), S_OK, "the list closes");
        let made = TWIN_COPIES.load(std::sync::atomic::Ordering::Relaxed) - copies;

        // read the host depth back (plane 0)
        let depth: ID3D12Resource = ID3D12Resource::from_raw_borrowed(&t.depth).unwrap().clone();
        let dd = depth.GetDesc();
        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        device.GetCopyableFootprints(&dd, 0, 1, 0, Some(&mut fp), None, None, Some(&mut total));
        let mut rb: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &crate::resource::buffer_desc(total, false), D3D12_RESOURCE_STATE_COPY_DEST, None, &mut rb)
            .unwrap();
        let rb = rb.unwrap();
        let ca: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let cl: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &ca, None).unwrap();
        let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&rb), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&depth), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
        cl.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        cl.Close().unwrap();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let mut lists: Vec<Option<ID3D12CommandList>> = segments(l).unwrap().iter().map(|s| Some(s.list.list.cast::<ID3D12CommandList>().unwrap())).collect();
        lists.push(Some(cl.cast().unwrap()));
        queue.ExecuteCommandLists(&lists);
        queue.Signal(&fence, 1).unwrap();
        fence.SetEventOnCompletion(1, windows::Win32::Foundation::HANDLE::default()).unwrap();
        if let Err(e) = device.GetDeviceRemovedReason() {
            panic!("device removed ({e}); debug layer errors: {:#?}", errors(device));
        }
        let mut m: *mut std::ffi::c_void = std::ptr::null_mut();
        rb.Map(0, None, Some(&mut m)).unwrap();
        let bytes = std::slice::from_raw_parts(m.cast::<u8>(), total as usize);
        let texel = |x: usize, y: usize| {
            let size = if fp.Footprint.Format == DXGI_FORMAT_R16_TYPELESS || fp.Footprint.Format == DXGI_FORMAT_D16_UNORM { 2 } else { 4 };
            read(&bytes[y * fp.Footprint.RowPitch as usize + x * size..])
        };
        let out = (texel(8, 20), texel(50, 40), made);
        rb.Unmap(0, None);
        drop(lists);
        let _ = method::<unsafe extern "system" fn(Raw) -> u32>(l, 0x10)(l);
        let _ = method::<unsafe extern "system" fn(Raw) -> u32>(a, 0x10)(a);
        out
    }
}

fn errors(device: &ID3D12Device) -> Vec<String> {
    let Ok(q) = device.cast::<ID3D12InfoQueue>() else { return Vec::new() };
    let mut out = Vec::new();
    unsafe {
        for i in 0..q.GetNumStoredMessages() {
            let mut len = 0usize;
            let _ = q.GetMessage(i, None, &mut len);
            let mut buf = vec![0u8; len];
            let m = buf.as_mut_ptr().cast::<D3D12_MESSAGE>();
            if q.GetMessage(i, Some(m), &mut len).is_ok() && (*m).Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                out.push(String::from_utf8_lossy(std::slice::from_raw_parts((*m).pDescription, (*m).DescriptionByteLength)).into_owned());
            }
        }
    }
    out
}

/// D32, D16 and D32 with stencil: the UAV's half holds its value, the rest the depth clear's, and the copies are
/// the three phase edges that need one (out of the creation's UAV layout, into UAV, out after work), none for the
/// phase with no work.
#[test]
fn a_uav_write_into_a_depth_texture_reaches_the_depth_on_warp() {
    let debug = std::env::var_os("REBUILD_D3D12_DEBUG").is_some();
    let Some(device) = warp(debug) else {
        println!("no WARP device: skipped");
        return;
    };
    let f32_at = |b: &[u8]| f32::from_le_bytes(b[..4].try_into().unwrap());
    let unorm16 = |b: &[u8]| f32::from(u16::from_le_bytes(b[..2].try_into().unwrap())) / 65535.0;
    for (name, format, dsv, uav, read) in [
        ("D32", DXGI_FORMAT_R32_TYPELESS.0, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_R32_FLOAT, &f32_at as &dyn Fn(&[u8]) -> f32),
        ("D16", DXGI_FORMAT_R16_TYPELESS.0, DXGI_FORMAT_D16_UNORM, DXGI_FORMAT_R16_UNORM, &unorm16),
        ("D32S8", DXGI_FORMAT_R32G8X24_TYPELESS.0, DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R32_FLOAT, &f32_at),
    ] {
        let (uav_side, depth_side, copies) = run(&device, format, dsv, uav, read);
        println!("{name}: UAV half {uav_side}, depth half {depth_side}, {copies} copy calls");
        assert!((uav_side - 0.75).abs() < 1e-3, "{name}: the UAV's write reached the depth ({uav_side})");
        assert!((depth_side - 0.25).abs() < 1e-3, "{name}: the depth clear survived the phase ({depth_side})");
        // three copying edges (the empty phase's enter finds the two equal, its exit finds no work); a texture with
        // stencil copies plane 0 by subresource (one here)
        assert_eq!(copies, 3, "{name}: copies only at the edges that need one");
    }
    let e = errors(&device);
    assert!(e.is_empty(), "debug layer errors: {e:#?}");
}
