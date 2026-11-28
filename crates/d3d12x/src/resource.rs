//! Heaps and resources: console objects with memory at console addresses, and the host
//! resources behind them.
//!
//! ```text
//! Resource  +0x00 header (vtable, bits, refs)  +0x10 ours                 (no inline reads recorded)
//! slots     0x40 Map  0x48 Unmap  0x50 GetDesc  0x58 GetGPUVirtualAddress  0x70 GetHeapProperties
//!           0x60/0x68 Write/ReadFromSubresource, 0x78-0x88 (console, unknown): traps
//! Heap      0x40 GetDesc; 0x48 GetProtectedResourceSession: trap
//! ```
//!
//! Every resource has console memory (`Memory::allocate`, or its heap's, or the address the
//! title gave), so `GetGPUVirtualAddress` is a console address and the title can place, alias
//! and view it by address. A buffer's host resource is the lead's buffer at that address
//! (`Memory::buffer`), so copies name the same bytes; a texture's is the lead's
//! (`Memory::texture`) or a committed host texture made here (contents by the lead's upload and
//! detile path, Build 5).

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::format::{self, AllocationInfo, AllocationInfo1, HeapDesc, HeapProperties, DIMENSION_BUFFER, KIB_64};
use crate::object::{self, Head};
use crate::slots::{Fp, Object};
use crate::{Raw, Resources};

pub fn buffer_desc(bytes: u64, uav: bool) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
        ..Default::default()
    }
}

/// A console resource.
#[repr(C)]
pub struct Resource {
    head: Head,
    /// the description as the title gave it
    pub desc: D3D12_RESOURCE_DESC,
    pub heap: HeapProperties,
    pub heap_flags: u32,
    /// console address of its memory = its GPU address
    pub address: u64,
    /// console bytes it covers (its footprint)
    pub bytes: u64,
    pub host: Option<ID3D12Resource>,
    /// the typed format views default to (the host texture is typeless)
    pub view_format: u32,
    /// memory we allocated (freed with the resource)
    owned: bool,
    /// the heap object it is placed in (held)
    parent: Raw,
    /// view table entries naming it, by a key of the view's description (one entry per
    /// distinct view; freed with it)
    pub(crate) entries: Mutex<Vec<(u64, u32)>>,
}

/// A console heap (0xe0): console memory resources are placed in.
#[repr(C)]
pub struct HeapObject {
    head: Head,
    pub desc: HeapDesc,
    pub address: u64,
}

impl Resource {
    pub fn is_buffer(&self) -> bool {
        self.desc.Dimension.0 == DIMENSION_BUFFER
    }
    pub fn host_raw(&self) -> Raw {
        self.host.as_ref().map_or(std::ptr::null_mut(), |h| h.as_raw())
    }
    /// The host resource takes unordered access.
    pub fn host_uav(&self) -> bool {
        self.host.as_ref().is_some_and(|h| unsafe { h.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0)
    }
}

/// One of our resources, if `p` is one.
pub(crate) fn resource<'a>(p: Raw) -> Option<&'a Resource> {
    // SAFETY: checked to be ours by its table.
    object::is(p, Object::Resource).then(|| unsafe { &*p.cast::<Resource>() })
}

pub(crate) fn heap_object<'a>(p: Raw) -> Option<&'a HeapObject> {
    // SAFETY: checked to be ours by its table.
    object::is(p, Object::Heap).then(|| unsafe { &*p.cast::<HeapObject>() })
}

impl Drop for Resource {
    fn drop(&mut self) {
        let ctx = &self.head.common.ctx;
        let entries: Vec<u32> = self.entries.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|e| e.1).collect();
        ctx.entries.release(self as *const Resource as usize, &entries);
        if self.owned && self.address != 0 {
            ctx.memory.free(self.address, self.bytes);
        }
        if !self.parent.is_null() {
            unsafe { heap_release(self.parent) };
        }
    }
}

impl Drop for HeapObject {
    fn drop(&mut self) {
        if self.address != 0 {
            self.head.common.ctx.memory.free(self.address, self.desc.size);
        }
    }
}

// ---- resource methods ----

unsafe extern "system" fn resource_release(this: Raw) -> u32 {
    unsafe { object::release_boxed::<Resource>(this) }
}

/// 0x40 Map `(subresource, read range, *out)`: the CPU address is the console address.
unsafe extern "system" fn map(this: Raw, _sub: u32, _read: *const c_void, out: *mut u64) -> HRESULT {
    let r = unsafe { &*this.cast::<Resource>() };
    if r.address == 0 {
        return E_INVALIDARG;
    }
    if !out.is_null() {
        unsafe { out.write(r.address) };
    }
    S_OK
}

/// 0x48 Unmap: the lead's write-watch sees what the CPU wrote.
unsafe extern "system" fn unmap(_this: Raw, _sub: u32, _written: *const c_void) {}

/// 0x50 GetDesc (struct return; either register order).
unsafe extern "system" fn get_desc(a: Raw, b: Raw) -> Raw {
    let (this, out) = object::this_and_out::<D3D12_RESOURCE_DESC>(a, b, Object::Resource);
    if !out.is_null() {
        unsafe { out.write((*this.cast::<Resource>()).desc) };
    }
    out.cast()
}

/// 0x58 GetGPUVirtualAddress: the console address.
unsafe extern "system" fn gpu_address(this: Raw) -> u64 {
    unsafe { (*this.cast::<Resource>()).address }
}

/// 0x70 GetHeapProperties `(*props, *flags)`.
unsafe extern "system" fn heap_properties(this: Raw, props: *mut HeapProperties, flags: *mut u32) -> HRESULT {
    let r = unsafe { &*this.cast::<Resource>() };
    unsafe {
        if let Some(p) = props.as_mut() {
            *p = r.heap;
        }
        if let Some(f) = flags.as_mut() {
            *f = r.heap_flags;
        }
    }
    S_OK
}

pub(crate) fn resource_methods() -> Vec<(usize, &'static str, Fp)> {
    let mut m = object::common_slots::<0>(resource_release);
    m.extend([
        (0x40, "Map", Fp(map as *const ())),
        (0x48, "Unmap", Fp(unmap as *const ())),
        (0x50, "GetDesc", Fp(get_desc as *const ())),
        (0x58, "GetGPUVirtualAddress", Fp(gpu_address as *const ())),
        (0x70, "GetHeapProperties", Fp(heap_properties as *const ())),
    ]);
    m
}

// ---- heap methods ----

pub(crate) unsafe extern "system" fn heap_release(this: Raw) -> u32 {
    unsafe { object::release_boxed::<HeapObject>(this) }
}

unsafe extern "system" fn heap_desc(a: Raw, b: Raw) -> Raw {
    let (this, out) = object::this_and_out::<HeapDesc>(a, b, Object::Heap);
    if !out.is_null() {
        unsafe { out.write((*this.cast::<HeapObject>()).desc) };
    }
    out.cast()
}

pub(crate) fn heap_methods() -> Vec<(usize, &'static str, Fp)> {
    let mut m = object::common_slots::<1>(heap_release);
    m.push((0x40, "GetDesc", Fp(heap_desc as *const ())));
    m
}

// ---- creation ----

const DEFAULT_HEAP: HeapProperties = HeapProperties { kind: 1, cpu_pages: 0, pool: 0, creation_nodes: 1, visible_nodes: 1 };

/// The title's initial state of a new resource: a legacy state (0xd8, 0xe8) or a barrier layout (0x3f8, 0x428).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Initial {
    State(u32),
    Layout(u32),
}

/// The host barrier layout a texture is created in: the title's, as the host knows it, when `hd`'s flags allow
/// it (a flag the host stripped, depth with unordered access say, falls back to common).
pub(crate) fn initial_layout(initial: Initial, hd: &D3D12_RESOURCE_DESC) -> D3D12_BARRIER_LAYOUT {
    // layouts: 1 generic read, 2 render target, 3 unordered access, 4 depth write, 5 depth read, 6 shader
    // resource, 7 copy source, 8 copy dest, 9 resolve source, 10 resolve dest
    let l: i32 = match initial {
        Initial::State(s) => match s {
            0x4 => 2,
            0x8 => 3,
            0x10 => 4,
            0x20 => 5,
            0x40 | 0x80 | 0xc0 => 6,
            0x800 => 7,
            0x400 => 8,
            0x2000 => 9,
            0x1000 => 10,
            0 => 0,
            // read combinations (generic read and the like)
            s if s & !(0x1 | 0x2 | 0x20 | 0x40 | 0x80 | 0x200 | 0x800 | 0x2000) == 0 => 1,
            _ => 0,
        },
        // the console's read layout (31): generic read
        Initial::Layout(31) => 1,
        Initial::Layout(l) if l <= 10 => l as i32,
        Initial::Layout(_) => 0,
    };
    let f = hd.Flags.0;
    let has = |flag: D3D12_RESOURCE_FLAGS| f & flag.0 != 0;
    let ok = match l {
        2 => has(D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET),
        3 => has(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
        4 | 5 => has(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL),
        1 | 6 => !has(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE),
        // resolve layouts need multisampling (source) or a resolve-capable target: keep it simple
        9 | 10 => false,
        _ => true,
    };
    D3D12_BARRIER_LAYOUT(if ok { l } else { 0 })
}

/// Where a new resource's memory comes from.
enum Backing {
    /// allocate console memory for it (committed)
    Own,
    /// at this console address, inside a heap (held) or the title's own memory
    At(u64, Raw),
}

impl Resources {
    #[allow(clippy::too_many_arguments)]
    unsafe fn make_resource(
        &self,
        desc: &D3D12_RESOURCE_DESC,
        heap: HeapProperties,
        heap_flags: u32,
        backing: Backing,
        initial: Initial,
        clear: *const D3D12_CLEAR_VALUE,
        iid: *const GUID,
        out: *mut Raw,
    ) -> HRESULT {
        let ctx = &self.inner;
        let Some(info) = format::footprint(desc) else {
            ctx.counters.refused.fetch_add(1, Ordering::Relaxed);
            ctx.events.note("resource without a size", desc.Width);
            return E_INVALIDARG;
        };
        let (address, owned, parent) = match backing {
            Backing::Own => match ctx.memory.allocate(info.size, info.alignment, &heap) {
                Some(a) => (a, true, std::ptr::null_mut()),
                None => {
                    ctx.counters.refused.fetch_add(1, Ordering::Relaxed);
                    return E_OUTOFMEMORY;
                }
            },
            Backing::At(a, p) => (a, false, p),
        };
        let (hd, view_format) = format::host_desc(desc);
        let host = if desc.Dimension.0 == DIMENSION_BUFFER {
            ctx.memory.buffer(address, desc.Width.max(1))
        } else {
            match ctx.memory.texture(address, &hd) {
                Some(t) => Some(t),
                None => {
                    // SAFETY: the title passes a clear value or null.
                    let clear = unsafe { clear.as_ref() }.map(|c| {
                        let mut c = *c;
                        c.Format = DXGI_FORMAT(format::host_format(c.Format.0 as u32) as i32);
                        if let Some(s) = format::stand_in(c.Format.0 as u32) {
                            c.Format = DXGI_FORMAT(s as i32);
                        }
                        c
                    });
                    let clear = clear.filter(|_| hd.Flags.0 & (D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0) != 0);
                    let layout = initial_layout(initial, &hd);
                    match ctx.host.resource_in(&hd, clear.as_ref(), layout) {
                        Ok(t) => t,
                        Err(hr) => {
                            if owned {
                                ctx.memory.free(address, info.size);
                            }
                            ctx.counters.refused.fetch_add(1, Ordering::Relaxed);
                            ctx.events.note("host texture refused", hr.0 as u32 as u64);
                            ctx.events.note("  its (dimension, host format, flags, layout)", (hd.Dimension.0 as u64) << 48 | (hd.Format.0 as u64 & 0xffff) << 32 | (hd.Flags.0 as u64 & 0xffff) << 16 | layout.0 as u64 & 0xffff);
                            ctx.events.note("  its (width, height)", hd.Width << 32 | u64::from(hd.Height));
                            ctx.events.note("  its (depth or array, mips, samples, quality)", u64::from(hd.DepthOrArraySize) << 48 | u64::from(hd.MipLevels) << 32 | u64::from(hd.SampleDesc.Count) << 16 | u64::from(hd.SampleDesc.Quality) & 0xffff);
                            ctx.events.note("  its (title format, title flags, clear format or ffff)", (desc.Format.0 as u64 & 0xffff) << 32 | (desc.Flags.0 as u64 & 0xffff) << 16 | clear.map_or(0xffff, |c| c.Format.0 as u64 & 0xffff));
                            return hr;
                        }
                    }
                }
            }
        };
        if !parent.is_null() {
            object::add_ref(parent);
        }
        let r = Box::new(Resource {
            head: Head::new(Object::Resource, ctx),
            desc: *desc,
            heap,
            heap_flags,
            address,
            bytes: info.size,
            host,
            view_format,
            owned,
            parent,
            entries: Mutex::new(Vec::new()),
        });
        ctx.counters.resources.fetch_add(1, Ordering::Relaxed);
        object::hand_out(Box::into_raw(r).cast(), Object::Resource, iid, out, resource_release)
    }

    /// 0xd8 CreateCommittedResource `(*heap props, heap flags, *desc, state, *clear, iid, *out)`.
    ///
    /// # Safety
    /// Pointers as the title passes them (each may be null).
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn create_committed(&self, heap: *const HeapProperties, heap_flags: u32, desc: *const D3D12_RESOURCE_DESC, state: u32, clear: *const D3D12_CLEAR_VALUE, iid: *const GUID, out: *mut Raw) -> HRESULT {
        let (Some(&h), Some(d)) = (unsafe { heap.as_ref() }, unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        unsafe { self.make_resource(d, h, heap_flags, Backing::Own, Initial::State(state), clear, iid, out) }
    }

    /// 0x3f8 CreateCommittedResource3 `(*heap, flags, *desc1, layout, *clear, protected, ncast,
    /// *cast, iid, *out)`: the back buffers (heap flag "allow display"). `D3D12_RESOURCE_DESC1`
    /// starts with the plain description.
    ///
    /// # Safety
    /// As [`Resources::create_committed`].
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn create_committed3(
        &self,
        heap: *const HeapProperties,
        heap_flags: u32,
        desc: *const D3D12_RESOURCE_DESC1,
        layout: u32,
        clear: *const D3D12_CLEAR_VALUE,
        _protected: Raw,
        _ncast: u32,
        _cast: *const u32,
        iid: *const GUID,
        out: *mut Raw,
    ) -> HRESULT {
        let (Some(&h), Some(d)) = (unsafe { heap.as_ref() }, unsafe { desc.cast::<D3D12_RESOURCE_DESC>().as_ref() }) else {
            return E_INVALIDARG;
        };
        unsafe { self.make_resource(d, h, heap_flags, Backing::Own, Initial::Layout(layout), clear, iid, out) }
    }

    /// 0xe0 CreateHeap `(*desc, iid, *out)`: console memory of the heap's size.
    ///
    /// # Safety
    /// As [`Resources::create_committed`].
    pub unsafe fn create_heap(&self, desc: *const HeapDesc, iid: *const GUID, out: *mut Raw) -> HRESULT {
        let ctx = &self.inner;
        let Some(&d) = (unsafe { desc.as_ref() }) else { return E_INVALIDARG };
        if d.size == 0 {
            return E_INVALIDARG;
        }
        let Some(address) = ctx.memory.allocate(d.size, d.alignment.max(KIB_64), &d.properties) else {
            ctx.counters.refused.fetch_add(1, Ordering::Relaxed);
            return E_OUTOFMEMORY;
        };
        let h = Box::new(HeapObject { head: Head::new(Object::Heap, ctx), desc: d, address });
        object::hand_out(Box::into_raw(h).cast(), Object::Heap, iid, out, heap_release)
    }

    /// 0xe8 CreatePlacedResource `(heap, offset, *desc, state, *clear, iid, *out)`.
    ///
    /// # Safety
    /// As [`Resources::create_committed`]; `heap` one of ours.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn create_placed(&self, heap: Raw, offset: u64, desc: *const D3D12_RESOURCE_DESC, state: u32, clear: *const D3D12_CLEAR_VALUE, iid: *const GUID, out: *mut Raw) -> HRESULT {
        let (Some(h), Some(d)) = (heap_object(heap), unsafe { desc.as_ref() }) else {
            return E_INVALIDARG;
        };
        let fits = format::footprint(d).is_some_and(|i| offset.checked_add(i.size).is_some_and(|end| end <= h.desc.size));
        if !fits {
            self.inner.counters.refused.fetch_add(1, Ordering::Relaxed);
            self.inner.events.note("placed resource past its heap", offset);
            return E_INVALIDARG;
        }
        unsafe { self.make_resource(d, h.desc.properties, h.desc.flags, Backing::At(h.address + offset, heap), Initial::State(state), clear, iid, out) }
    }

    /// 0x428 a resource at a title-mapped address `(address, *desc1, layout, *clear, iid, *out)`.
    ///
    /// # Safety
    /// As [`Resources::create_committed`].
    pub unsafe fn create_at_address(&self, address: u64, desc: *const D3D12_RESOURCE_DESC1, layout: u32, clear: *const D3D12_CLEAR_VALUE, iid: *const GUID, out: *mut Raw) -> HRESULT {
        let Some(d) = (unsafe { desc.cast::<D3D12_RESOURCE_DESC>().as_ref() }) else { return E_INVALIDARG };
        if address == 0 {
            return E_INVALIDARG;
        }
        unsafe { self.make_resource(d, DEFAULT_HEAP, 0, Backing::At(address, std::ptr::null_mut()), Initial::Layout(layout), clear, iid, out) }
    }

    /// 0xc8 GetResourceAllocationInfo `(visible mask, count, *descs)` → `*out` (struct return:
    /// the caller's buffer, returned).
    ///
    /// # Safety
    /// `out` has room; `descs` holds `count` descriptions or is null.
    pub unsafe fn allocation_info(&self, out: *mut AllocationInfo, _visible: u32, count: u32, descs: *const D3D12_RESOURCE_DESC) -> *mut AllocationInfo {
        let d = if descs.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(descs, count as usize) } };
        let total = format::place(d, |_, _| ());
        if !out.is_null() {
            unsafe { out.write(total) };
        }
        out
    }

    /// 0x360 GetResourceAllocationInfo2 `(visible, count, *desc1s, *each)` → `*out`.
    ///
    /// # Safety
    /// As [`Resources::allocation_info`]; `each` has room for `count` or is null.
    pub unsafe fn allocation_info2(&self, out: *mut AllocationInfo, _visible: u32, count: u32, descs: *const D3D12_RESOURCE_DESC1, each: *mut AllocationInfo1) -> *mut AllocationInfo {
        let d: Vec<D3D12_RESOURCE_DESC> = if descs.is_null() {
            Vec::new()
        } else {
            (0..count as usize).map(|i| unsafe { descs.add(i).cast::<D3D12_RESOURCE_DESC>().read() }).collect()
        };
        let total = format::place(&d, |i, info| {
            if !each.is_null() {
                unsafe { each.add(i).write(info) };
            }
        });
        if !out.is_null() {
            unsafe { out.write(total) };
        }
        out
    }

    /// 0xd0 GetCustomHeapProperties `(node mask, heap type)` → `*out`. The console's memory is
    /// unified and coherent: every type is in pool L0; default heaps are not CPU-visible, the
    /// others write-back.
    ///
    /// # Safety
    /// `out` has room or is null.
    pub unsafe fn custom_heap_properties(&self, out: *mut HeapProperties, nodes: u32, kind: u32) -> *mut HeapProperties {
        const CUSTOM: u32 = 4;
        let nodes = nodes.max(1);
        let cpu_pages = match kind {
            1 => 1,     // NOT_AVAILABLE
            2 | 3 => 3, // WRITE_BACK
            _ => 0,
        };
        if let Some(o) = unsafe { out.as_mut() } {
            *o = HeapProperties { kind: CUSTOM, cpu_pages, pool: u32::from(cpu_pages != 0), creation_nodes: nodes, visible_nodes: nodes };
        }
        out
    }

    /// 0x130 GetCopyableFootprints: the host's rules, since the copies they describe run on the
    /// host (`CopyTextureRegion` through the list translator).
    ///
    /// # Safety
    /// As the D3D12 method.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn copyable_footprints(&self, desc: *const D3D12_RESOURCE_DESC, first: u32, count: u32, base: u64, layouts: *mut D3D12_PLACED_SUBRESOURCE_FOOTPRINT, rows: *mut u32, row_bytes: *mut u64, total: *mut u64) {
        let Some(d) = (unsafe { desc.as_ref() }) else { return };
        let (hd, view) = format::host_desc(d);
        // footprints are of the typed format the title copies, not the typeless host family
        let mut hd = hd;
        if d.Dimension.0 != DIMENSION_BUFFER {
            hd.Format = DXGI_FORMAT(view as i32);
        }
        self.inner.host.footprints(&hd, first, count, base, layouts, rows, row_bytes, total);
    }

    /// 0x250 register a page pool `(address (64 KiB aligned), pages, *handle)`.
    ///
    /// # Safety
    /// `handle` has room or is null.
    pub unsafe fn register_page_pool(&self, address: u64, pages: u32, handle: *mut usize) -> HRESULT {
        if handle.is_null() || pages == 0 || address % KIB_64 != 0 {
            return E_INVALIDARG;
        }
        let mut pools = self.inner.pools.lock().unwrap_or_else(|e| e.into_inner());
        pools.push(Some((address, pages)));
        unsafe { handle.write(pools.len()) };
        S_OK
    }

    /// 0x258 unregister a page pool.
    pub fn unregister_page_pool(&self, handle: usize) -> HRESULT {
        match self.inner.pools.lock().unwrap_or_else(|e| e.into_inner()).get_mut(handle.wrapping_sub(1)) {
            Some(p @ Some(_)) => {
                *p = None;
                S_OK
            }
            _ => E_INVALIDARG,
        }
    }

    /// A registered page pool: `(first address, pages)` (the queue's page mapping, 0xf8).
    pub fn page_pool(&self, handle: usize) -> Option<(u64, u32)> {
        self.inner.pools.lock().unwrap_or_else(|e| e.into_inner()).get(handle.wrapping_sub(1)).copied().flatten()
    }

    /// The console resource behind a pointer, if it is one of ours.
    pub fn console_resource(&self, p: Raw) -> Option<&Resource> {
        resource(p)
    }

    /// A console resource as a host `ID3D12Resource` (borrowed): `ListHost::resource`, and the
    /// back buffers for the lead's present.
    #[inline]
    pub fn host_resource(&self, p: Raw) -> Option<Raw> {
        resource(p).and_then(|r| r.host.as_ref().map(|h| h.as_raw()))
    }
}

#[cfg(test)]
mod initial_tests {
    use super::{initial_layout, Initial};
    use windows::Win32::Graphics::Direct3D12::*;

    fn tex(flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: 64, Height: 64, DepthOrArraySize: 1, MipLevels: 1, Flags: flags, ..Default::default() }
    }

    #[test]
    fn the_titles_initial_layout_is_kept_when_the_host_flags_allow_it() {
        let plain = tex(D3D12_RESOURCE_FLAG_NONE);
        let uav = tex(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
        let depth = tex(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL);
        let l = |i, d: &D3D12_RESOURCE_DESC| initial_layout(i, d).0;
        assert_eq!(l(Initial::Layout(6), &plain), 6);
        assert_eq!(l(Initial::Layout(3), &uav), 3);
        assert_eq!(l(Initial::Layout(31), &plain), 1, "the console's read layout");
        assert_eq!(l(Initial::Layout(3), &plain), 0, "UAV stripped (depth with unordered access)");
        assert_eq!(l(Initial::Layout(2), &plain), 0, "no render target flag");
        assert_eq!(l(Initial::Layout(4), &depth), 4);
        assert_eq!(l(Initial::Layout(0xffff_ffff), &plain), 0, "undefined");
        // legacy states
        assert_eq!(l(Initial::State(0xc0), &plain), 6);
        assert_eq!(l(Initial::State(0x8), &uav), 3);
        assert_eq!(l(Initial::State(0xac3), &plain), 1, "generic read");
        assert_eq!(l(Initial::State(0x4 | 0x8), &tex(D3D12_RESOURCE_FLAGS(0x1 | 0x4))), 0, "two writes");
        assert_eq!(l(Initial::State(0x8000_0000), &plain), 0);
    }
}
