//! `extern "system"` functions for the device slots this crate serves, over the [`Resources`]
//! installed with [`install`] (one device per process, as on the console). The lead puts
//! [`device_slots`] into its device table.
//!
//! Struct returns (0xc8, 0xd0, 0x360): MSVC C++ passes `(this, out)`; the fork declared them
//! `(out, this)`. The thunk tells the two apart by the device pointer
//! ([`Resources::set_device`]), so either order works.

use std::sync::OnceLock;

use windows::core::{GUID, HRESULT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D12::*;

use crate::format::{AllocationInfo, AllocationInfo1, HeapDesc, HeapProperties};
use crate::slots::Fp;
use crate::{Raw, Resources};

static INSTALLED: OnceLock<Resources> = OnceLock::new();

/// Make `r` the one the thunks serve. Later calls are ignored (returns false).
pub fn install(r: Resources) -> bool {
    INSTALLED.set(r).is_ok()
}

#[inline]
fn r() -> Option<&'static Resources> {
    INSTALLED.get()
}

/// `(this, out)` or `(out, this)`: the one that is the device is `this`.
fn out_of<T>(a: Raw, b: Raw) -> *mut T {
    let device = r().map_or(std::ptr::null_mut(), |r| r.inner.device.load(std::sync::atomic::Ordering::Relaxed));
    if !device.is_null() && b == device {
        a.cast()
    } else {
        b.cast()
    }
}

unsafe extern "system" fn create_descriptor_heap(_: Raw, desc: *const [u32; 4], iid: *const GUID, out: *mut Raw) -> HRESULT {
    r().map_or(E_FAIL, |r| unsafe { r.create_descriptor_heap(desc, iid, out) })
}
unsafe extern "system" fn increment(_: Raw, kind: u32) -> u32 {
    r().map_or(0, |r| r.increment(kind))
}
unsafe extern "system" fn cbv(_: Raw, desc: *const D3D12_CONSTANT_BUFFER_VIEW_DESC, handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_cbv(desc, handle) }
    }
}
unsafe extern "system" fn srv(_: Raw, res: Raw, desc: *const D3D12_SHADER_RESOURCE_VIEW_DESC, handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_srv(res, desc, handle) }
    }
}
unsafe extern "system" fn uav(_: Raw, res: Raw, counter: Raw, desc: *const D3D12_UNORDERED_ACCESS_VIEW_DESC, handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_uav(res, counter, desc, handle) }
    }
}
unsafe extern "system" fn rtv(_: Raw, res: Raw, desc: *const D3D12_RENDER_TARGET_VIEW_DESC, handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_rtv(res, desc, handle) }
    }
}
unsafe extern "system" fn dsv(_: Raw, res: Raw, desc: *const D3D12_DEPTH_STENCIL_VIEW_DESC, handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_dsv(res, desc, handle) }
    }
}
unsafe extern "system" fn sampler(_: Raw, desc: *const D3D12_SAMPLER_DESC, handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_sampler(desc, handle) }
    }
}
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn copy(_: Raw, nd: u32, ds: *const u64, dz: *const u32, ns: u32, ss: *const u64, sz: *const u32, kind: u32) {
    if let Some(r) = r() {
        unsafe { r.copy_descriptors(nd, ds, dz, ns, ss, sz, kind) }
    }
}
unsafe extern "system" fn copy_simple(_: Raw, count: u32, dst: u64, src: u64, kind: u32) {
    if let Some(r) = r() {
        unsafe { r.copy_descriptors_simple(count, dst, src, kind) }
    }
}
unsafe extern "system" fn allocation_info(a: Raw, b: Raw, visible: u32, count: u32, descs: *const D3D12_RESOURCE_DESC) -> *mut AllocationInfo {
    let out = out_of::<AllocationInfo>(a, b);
    match r() {
        Some(r) => unsafe { r.allocation_info(out, visible, count, descs) },
        None => out,
    }
}
unsafe extern "system" fn custom_heap_properties(a: Raw, b: Raw, nodes: u32, kind: u32) -> *mut HeapProperties {
    let out = out_of::<HeapProperties>(a, b);
    match r() {
        Some(r) => unsafe { r.custom_heap_properties(out, nodes, kind) },
        None => out,
    }
}
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn committed(_: Raw, heap: *const HeapProperties, flags: u32, desc: *const D3D12_RESOURCE_DESC, state: u32, clear: *const D3D12_CLEAR_VALUE, iid: *const GUID, out: *mut Raw) -> HRESULT {
    r().map_or(E_FAIL, |r| unsafe { r.create_committed(heap, flags, desc, state, clear, iid, out) })
}
unsafe extern "system" fn heap(_: Raw, desc: *const HeapDesc, iid: *const GUID, out: *mut Raw) -> HRESULT {
    r().map_or(E_FAIL, |r| unsafe { r.create_heap(desc, iid, out) })
}
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn placed(_: Raw, heap: Raw, offset: u64, desc: *const D3D12_RESOURCE_DESC, state: u32, clear: *const D3D12_CLEAR_VALUE, iid: *const GUID, out: *mut Raw) -> HRESULT {
    r().map_or(E_FAIL, |r| unsafe { r.create_placed(heap, offset, desc, state, clear, iid, out) })
}
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn footprints(_: Raw, desc: *const D3D12_RESOURCE_DESC, first: u32, count: u32, base: u64, layouts: *mut D3D12_PLACED_SUBRESOURCE_FOOTPRINT, rows: *mut u32, rb: *mut u64, total: *mut u64) {
    if let Some(r) = r() {
        unsafe { r.copyable_footprints(desc, first, count, base, layouts, rows, rb, total) }
    }
}
unsafe extern "system" fn sampler_x(_: Raw, desc: *const [u32; 27], handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_sampler_x(desc, handle) }
    }
}
unsafe extern "system" fn placed_srv(_: Raw, res: *const [u32; 14], view: *const [u32; 16], handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_placed_view(false, res, view, handle) }
    }
}
unsafe extern "system" fn placed_uav(_: Raw, res: *const [u32; 14], view: *const [u32; 16], handle: u64) {
    if let Some(r) = r() {
        unsafe { r.create_placed_view(true, res, view, handle) }
    }
}
unsafe extern "system" fn register_pool(_: Raw, address: u64, pages: u32, handle: *mut usize) -> HRESULT {
    r().map_or(E_FAIL, |r| unsafe { r.register_page_pool(address, pages, handle) })
}
unsafe extern "system" fn unregister_pool(_: Raw, handle: usize) -> HRESULT {
    r().map_or(E_FAIL, |r| r.unregister_page_pool(handle))
}
unsafe extern "system" fn allocation_info2(a: Raw, b: Raw, visible: u32, count: u32, descs: *const D3D12_RESOURCE_DESC1, each: *mut AllocationInfo1) -> *mut AllocationInfo {
    let out = out_of::<AllocationInfo>(a, b);
    match r() {
        Some(r) => unsafe { r.allocation_info2(out, visible, count, descs, each) },
        None => out,
    }
}
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn committed3(
    _: Raw,
    heap: *const HeapProperties,
    flags: u32,
    desc: *const D3D12_RESOURCE_DESC1,
    layout: u32,
    clear: *const D3D12_CLEAR_VALUE,
    protected: Raw,
    ncast: u32,
    cast: *const u32,
    iid: *const GUID,
    out: *mut Raw,
) -> HRESULT {
    r().map_or(E_FAIL, |r| unsafe { r.create_committed3(heap, flags, desc, layout, clear, protected, ncast, cast, iid, out) })
}
unsafe extern "system" fn at_address(_: Raw, address: u64, desc: *const D3D12_RESOURCE_DESC1, layout: u32, clear: *const D3D12_CLEAR_VALUE, iid: *const GUID, out: *mut Raw) -> HRESULT {
    r().map_or(E_FAIL, |r| unsafe { r.create_at_address(address, desc, layout, clear, iid, out) })
}

/// The device slots served here: `(offset, name, function)`.
pub fn device_slots() -> Vec<(usize, &'static str, Fp)> {
    vec![
        (0x070, "CreateDescriptorHeap", Fp(create_descriptor_heap as *const ())),
        (0x078, "GetDescriptorHandleIncrementSize", Fp(increment as *const ())),
        (0x088, "CreateConstantBufferView", Fp(cbv as *const ())),
        (0x090, "CreateShaderResourceView", Fp(srv as *const ())),
        (0x098, "CreateUnorderedAccessView", Fp(uav as *const ())),
        (0x0a0, "CreateRenderTargetView", Fp(rtv as *const ())),
        (0x0a8, "CreateDepthStencilView", Fp(dsv as *const ())),
        (0x0b0, "CreateSampler", Fp(sampler as *const ())),
        (0x0b8, "CopyDescriptors", Fp(copy as *const ())),
        (0x0c0, "CopyDescriptorsSimple", Fp(copy_simple as *const ())),
        (0x0c8, "GetResourceAllocationInfo", Fp(allocation_info as *const ())),
        (0x0d0, "GetCustomHeapProperties", Fp(custom_heap_properties as *const ())),
        (0x0d8, "CreateCommittedResource", Fp(committed as *const ())),
        (0x0e0, "CreateHeap", Fp(heap as *const ())),
        (0x0e8, "CreatePlacedResource", Fp(placed as *const ())),
        (0x130, "GetCopyableFootprints", Fp(footprints as *const ())),
        (0x1e8, "CreateSamplerX", Fp(sampler_x as *const ())),
        (0x230, "CreatePlacedShaderResourceView", Fp(placed_srv as *const ())),
        (0x238, "CreatePlacedUnorderedAccessView", Fp(placed_uav as *const ())),
        (0x250, "RegisterPagePool", Fp(register_pool as *const ())),
        (0x258, "UnregisterPagePool", Fp(unregister_pool as *const ())),
        (0x360, "GetResourceAllocationInfo2", Fp(allocation_info2 as *const ())),
        (0x3f8, "CreateCommittedResource3", Fp(committed3 as *const ())),
        (0x428, "CreateResourceAtAddress", Fp(at_address as *const ())),
    ]
}
