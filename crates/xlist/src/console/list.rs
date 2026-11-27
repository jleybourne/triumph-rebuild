//! The list object and its slot functions.
//!
//! Every function here is called by title code with the console's argument layout (Windows x64,
//! `this` first). Each one translates its call into the current host list at once: no recording
//! buffer, no lock, no allocation once the list's scratch vectors have grown. The per-list
//! [`Shadow`] keeps what is bound, so a set that repeats the bound value is dropped (as in
//! [`crate::translate`]) and the state can be set again on the next host list when the list is
//! cut at a memory wait.

use std::mem::offset_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{BOOL, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_POINTER, RECT, S_OK};
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;

use super::alloc::{self, AllocatorState};
use super::slots::{self, Fp};
use super::{Backend, HeapBinding, Header, LabelValue, LabelWrite, ListHost, MemoryRange, MemoryWait, Raw, WaitAction, CONSOLE_RTV_INCREMENT, TITLE_BYTES};

const DXGI_ERROR_NOT_FOUND: HRESULT = HRESULT(0x887A_0002_u32 as i32);
const DXGI_ERROR_MORE_DATA: HRESULT = HRESULT(0x887A_0003_u32 as i32);

/// A host list and the newer interfaces it answers (asked once, when it is made).
pub struct HostList {
    pub list: ID3D12GraphicsCommandList,
    l1: Option<ID3D12GraphicsCommandList1>,
    l2: Option<ID3D12GraphicsCommandList2>,
    l6: Option<ID3D12GraphicsCommandList6>,
    l7: Option<ID3D12GraphicsCommandList7>,
}

impl HostList {
    fn new(list: ID3D12GraphicsCommandList) -> HostList {
        HostList { l1: list.cast().ok(), l2: list.cast().ok(), l6: list.cast().ok(), l7: list.cast().ok(), list }
    }
}

/// One part of a title list, cut at its memory waits: run `list` once every wait in `waits` holds.
/// A list without waits is one segment with no waits.
pub struct Segment {
    pub list: HostList,
    pub waits: Vec<MemoryWait>,
}

/// Counts since the list's last reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListStats {
    /// every call through the table, traps excluded
    pub calls: u32,
    pub draws: u32,
    pub dispatches: u32,
    /// sets dropped because they repeat what is bound
    pub elided: u32,
    /// calls (or parts of calls) not recorded: an object or address the host side does not know
    pub refused: u32,
    /// host lists the list was cut into beyond the first
    pub splits: u32,
}

/// The list as the title sees it: [`TITLE_BYTES`] of console layout, then ours.
#[repr(C, align(16))]
pub(crate) struct ListObject {
    pub header: Header,
    /// list type: 0 direct, 2 compute, 3 copy
    pub kind: u32,
    _pad: u32,
    /// The put pointer, then the plain, draw, API and unbiased API limits: all zero, so the
    /// title's inline packet writer always calls instead.
    pub put: [u64; 5],
    /// The title's: root user-data headers (+0x140, +0x340), the pipeline shadow it copies a
    /// pipeline's 32-byte packet into (+0x544), and the newer title's fields past +0x560.
    pub title: [u8; TITLE_BYTES - 0x40],
    pub ours: ListState,
}

const _: () = {
    assert!(offset_of!(ListObject, kind) == 0x10);
    assert!(offset_of!(ListObject, put) == 0x18);
    assert!(offset_of!(ListObject, title) == 0x40);
    assert!(offset_of!(ListObject, ours) == TITLE_BYTES);
};

#[derive(Clone, Copy, Default, PartialEq)]
struct Arg {
    /// 0 none, 1 table, 2 CBV, 3 SRV, 4 UAV
    kind: u8,
    console: u64,
    host: u64,
}

struct Consts {
    v: [[u32; 64]; 64],
    mask: [u64; 64],
}

/// What is bound, in host terms (for replay) and console terms (for elision).
struct Shadow {
    console_pipeline: Raw,
    pso: Raw,
    /// 0 graphics, 1 compute
    root: [Raw; 2],
    args: [[Arg; 64]; 2],
    consts: [Option<Box<Consts>>; 2],
    heaps_console: [Raw; 2],
    heaps_host: [Raw; 2],
    heap_count: u32,
    topology: Option<i32>,
    viewports: [D3D12_VIEWPORT; 16],
    viewport_count: u32,
    scissors: [RECT; 16],
    scissor_count: u32,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<(f32, f32)>,
    rts: [D3D12_CPU_DESCRIPTOR_HANDLE; 8],
    rt_count: u32,
    dsv: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
    rts_set: bool,
    ib_console: Option<(u64, u32, u32)>,
    ib: D3D12_INDEX_BUFFER_VIEW,
    vb_console: [(u64, u32, u32); 32],
    vbs: [D3D12_VERTEX_BUFFER_VIEW; 32],
    vb_used: u32,
}

const NULL: Raw = std::ptr::null_mut();

impl Shadow {
    fn new() -> Shadow {
        Shadow {
            console_pipeline: NULL,
            pso: NULL,
            root: [NULL; 2],
            args: [[Arg::default(); 64]; 2],
            consts: [None, None],
            heaps_console: [NULL; 2],
            heaps_host: [NULL; 2],
            heap_count: 0,
            topology: None,
            viewports: [D3D12_VIEWPORT::default(); 16],
            viewport_count: 0,
            scissors: [RECT::default(); 16],
            scissor_count: 0,
            blend: None,
            stencil: None,
            depth_bounds: None,
            rts: [D3D12_CPU_DESCRIPTOR_HANDLE::default(); 8],
            rt_count: 0,
            dsv: None,
            rts_set: false,
            ib_console: None,
            ib: D3D12_INDEX_BUFFER_VIEW::default(),
            vb_console: [(u64::MAX, 0, 0); 32],
            vbs: [D3D12_VERTEX_BUFFER_VIEW::default(); 32],
            vb_used: 0,
        }
    }

    fn clear(&mut self) {
        let consts = std::mem::take(&mut self.consts);
        *self = Shadow::new();
        self.consts = consts;
        for c in self.consts.iter_mut().flatten() {
            c.mask = [0; 64];
        }
    }

    fn root_changed(&mut self, k: usize, root: Raw) {
        self.root[k] = root;
        self.args[k] = [Arg::default(); 64];
        if let Some(c) = self.consts[k].as_mut() {
            c.mask = [0; 64];
        }
    }
}

pub(crate) struct ListState {
    host: Arc<dyn ListHost>,
    backend: Arc<dyn Backend>,
    enhanced_barriers: bool,
    kind: u32,
    allocator: Raw,
    /// the current host list, for calls without a lookup
    raw: Raw,
    vt: *const ID3D12GraphicsCommandList_Vtbl,
    segments: Vec<Segment>,
    spare: Vec<HostList>,
    open: bool,
    /// work recorded in the current segment (a wait before any work needs no cut)
    seg_work: bool,
    /// work since the last barrier (a pipe sync after none needs no barrier)
    work_since_barrier: bool,
    sh: Shadow,
    heaps: [Option<HeapBinding>; 2],
    last: MemoryRange,
    barriers: Vec<D3D12_RESOURCE_BARRIER>,
    globals: Vec<[u8; 16]>,
    textures: Vec<[u8; 64]>,
    buffers: Vec<[u8; 40]>,
    stats: ListStats,
    private: Vec<(GUID, Vec<u8>)>,
}

// SAFETY: a list is recorded by one thread at a time (D3D12's rule, and the title's).
unsafe impl Send for ListState {}

macro_rules! call {
    ($s:ident . $m:ident ( $($a:expr),* $(,)? )) => {
        unsafe { ((*$s.vt).$m)($s.raw, $($a),*) }
    };
}

impl ListState {
    #[inline]
    fn work(&mut self) {
        self.seg_work = true;
        self.work_since_barrier = true;
    }

    fn current(&self) -> &HostList {
        &self.segments.last().expect("a list has a segment").list
    }

    fn point_at_current(&mut self) {
        let (raw, vt) = {
            let l = &self.current().list;
            (l.as_raw(), l.vtable() as *const ID3D12GraphicsCommandList_Vtbl)
        };
        self.raw = raw;
        self.vt = vt;
    }

    fn allocator(&self) -> Option<&AllocatorState> {
        alloc::state(self.allocator)
    }

    /// Host GPU address of console `va`; 0 stays 0 (a null view).
    #[inline]
    fn va(&mut self, va: u64) -> Option<u64> {
        if va == 0 {
            return Some(0);
        }
        match self.last.map(va) {
            Some(h) => Some(h),
            None => self.range_slow(va).and_then(|r| r.map(va)),
        }
    }

    #[inline]
    fn place(&mut self, va: u64, len: u64) -> Option<(Raw, u64)> {
        match self.last.place(va, len) {
            Some(p) => Some(p),
            None => self.range_slow(va).and_then(|r| r.place(va, len)),
        }
    }

    #[cold]
    fn range_slow(&mut self, va: u64) -> Option<MemoryRange> {
        let r = self.host.memory(va).or_else(|| self.allocator().and_then(|a| a.find(va)))?;
        self.last = r;
        Some(r)
    }

    #[inline]
    fn table(&self, handle: u64) -> Option<u64> {
        self.heaps[0].and_then(|h| h.map(handle)).or_else(|| self.heaps[1].and_then(|h| h.map(handle)))
    }

    #[inline]
    fn resource(&mut self, console: Raw) -> Option<Raw> {
        if console.is_null() {
            return Some(NULL);
        }
        let r = self.host.resource(console);
        if r.is_none() {
            self.stats.refused += 1;
        }
        r
    }

    fn set_root(&mut self, k: usize, root: Raw) {
        if self.sh.root[k] == root {
            self.stats.elided += 1;
            return;
        }
        if k == 0 {
            call!(self.SetGraphicsRootSignature(root));
        } else {
            call!(self.SetComputeRootSignature(root));
        }
        self.sh.root_changed(k, root);
    }

    fn set_pipeline(&mut self, console: Raw) {
        if console == self.sh.console_pipeline {
            self.stats.elided += 1;
            return;
        }
        let Some(p) = self.host.pipeline(console) else {
            self.stats.refused += 1;
            return;
        };
        self.sh.console_pipeline = console;
        if !p.root_signature.is_null() {
            self.set_root(p.compute as usize, p.root_signature);
        }
        if p.pso != self.sh.pso {
            call!(self.SetPipelineState(p.pso));
            self.sh.pso = p.pso;
        }
    }

    /// A barrier that orders every earlier shader write before every later access.
    fn global_barrier(&mut self) {
        let b = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() };
        call!(self.ResourceBarrier(1, &b));
        self.work_since_barrier = false;
        self.seg_work = true;
    }

    fn close_current(&mut self) -> HRESULT {
        if !self.open {
            return E_FAIL;
        }
        self.open = false;
        call!(self.Close())
    }

    /// Cut the list at a memory wait: the work so far is one host list, the rest another that
    /// runs once `wait` holds.
    fn split(&mut self, wait: MemoryWait) {
        if !self.seg_work {
            self.segments.last_mut().expect("a segment").waits.push(wait);
            return;
        }
        let Some(a) = self.allocator() else {
            self.stats.refused += 1;
            return;
        };
        let allocator = a.allocator.clone();
        let _ = self.close_current();
        let next = match self.spare.pop() {
            Some(h) => unsafe { h.list.Reset(&allocator, None) }.map(|_| h),
            None => self.backend.list(self.kind, &allocator).map(HostList::new),
        };
        match next {
            Ok(h) => {
                self.segments.push(Segment { list: h, waits: vec![wait] });
                self.open = true;
                self.point_at_current();
                self.stats.splits += 1;
                self.replay();
                self.seg_work = false;
                self.work_since_barrier = false;
            }
            // Device lost or out of memory: the list stays closed; the queue's submit reports it.
            Err(_) => self.stats.refused += 1,
        }
    }

    /// Set everything bound again on a fresh host list.
    fn replay(&mut self) {
        let sh = &self.sh;
        if sh.heap_count > 0 {
            call!(self.SetDescriptorHeaps(sh.heap_count, sh.heaps_host.as_ptr()));
        }
        if !sh.root[0].is_null() {
            call!(self.SetGraphicsRootSignature(sh.root[0]));
        }
        if !sh.root[1].is_null() {
            call!(self.SetComputeRootSignature(sh.root[1]));
        }
        if !sh.pso.is_null() {
            call!(self.SetPipelineState(sh.pso));
        }
        for k in 0..2 {
            for (slot, a) in sh.args[k].iter().enumerate() {
                let slot = slot as u32;
                match (k, a.kind) {
                    (_, 0) => {}
                    (0, 1) => call!(self.SetGraphicsRootDescriptorTable(slot, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: a.host })),
                    (1, 1) => call!(self.SetComputeRootDescriptorTable(slot, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: a.host })),
                    (0, 2) => call!(self.SetGraphicsRootConstantBufferView(slot, a.host)),
                    (1, 2) => call!(self.SetComputeRootConstantBufferView(slot, a.host)),
                    (0, 3) => call!(self.SetGraphicsRootShaderResourceView(slot, a.host)),
                    (1, 3) => call!(self.SetComputeRootShaderResourceView(slot, a.host)),
                    (0, 4) => call!(self.SetGraphicsRootUnorderedAccessView(slot, a.host)),
                    _ => call!(self.SetComputeRootUnorderedAccessView(slot, a.host)),
                }
            }
            if let Some(c) = &sh.consts[k] {
                for slot in 0..64 {
                    let mut m = c.mask[slot];
                    while m != 0 {
                        let off = m.trailing_zeros();
                        m &= m - 1;
                        let v = c.v[slot][off as usize];
                        if k == 0 {
                            call!(self.SetGraphicsRoot32BitConstant(slot as u32, v, off));
                        } else {
                            call!(self.SetComputeRoot32BitConstant(slot as u32, v, off));
                        }
                    }
                }
            }
        }
        if let Some(t) = sh.topology {
            call!(self.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(t)));
        }
        if sh.viewport_count > 0 {
            call!(self.RSSetViewports(sh.viewport_count, sh.viewports.as_ptr()));
        }
        if sh.scissor_count > 0 {
            call!(self.RSSetScissorRects(sh.scissor_count, sh.scissors.as_ptr()));
        }
        if let Some(b) = sh.blend {
            call!(self.OMSetBlendFactor(b.as_ptr()));
        }
        if let Some(r) = sh.stencil {
            call!(self.OMSetStencilRef(r));
        }
        if let (Some((lo, hi)), Some(l1)) = (sh.depth_bounds, &self.current().l1) {
            unsafe { l1.OMSetDepthBounds(lo, hi) };
        }
        if sh.rts_set {
            let dsv = sh.dsv.as_ref().map_or(std::ptr::null(), |d| d as *const _);
            call!(self.OMSetRenderTargets(sh.rt_count, sh.rts.as_ptr(), BOOL(0), dsv));
        }
        if sh.ib_console.is_some() {
            call!(self.IASetIndexBuffer(&sh.ib));
        }
        if sh.vb_used > 0 {
            call!(self.IASetVertexBuffers(0, sh.vb_used, sh.vbs.as_ptr()));
        }
    }
}

#[inline]
fn enter<'a>(this: Raw) -> &'a mut ListState {
    // SAFETY: title code calls list methods on lists we handed out, one thread at a time.
    let s = unsafe { &mut (*this.cast::<ListObject>()).ours };
    s.stats.calls += 1;
    s
}

/// Ours, if `p` is a list we made.
fn state<'a>(p: Raw) -> Option<&'a mut ListState> {
    if p.is_null() {
        return None;
    }
    let obj = p.cast::<ListObject>();
    // SAFETY: the first word of any object the title passes is its table.
    unsafe { std::ptr::eq((*obj).header.vtable, slots::list_table().funcs.as_ptr()).then(|| &mut (*obj).ours) }
}

pub(crate) fn host_of<'a>(p: Raw) -> Option<&'a Arc<dyn ListHost>> {
    state(p).map(|s| &s.host)
}

/// Make a list of type `kind` on a console allocator from [`super::create_allocator`], open, with
/// `pipeline` (a console pipeline, or null) bound. Device 0x60 and 0x248 hand this pointer to
/// the title. It starts with one reference; the title's `Release` frees it.
pub fn create_list(kind: u32, allocator: Raw, pipeline: Raw) -> windows::core::Result<Raw> {
    let a = alloc::state(allocator).ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
    let host_list = a.backend.list(kind, &a.allocator)?;
    let enhanced_barriers = a.backend.enhanced_barriers();
    let first = HostList::new(host_list);
    let obj = Box::new(ListObject {
        header: Header { vtable: slots::list_table().funcs.as_ptr(), bits: 0, refs: AtomicU32::new(1) },
        kind,
        _pad: 0,
        put: [0; 5],
        title: [0; TITLE_BYTES - 0x40],
        ours: ListState {
            host: a.host.clone(),
            backend: a.backend.clone(),
            enhanced_barriers,
            kind,
            allocator,
            raw: first.list.as_raw(),
            vt: first.list.vtable(),
            segments: vec![Segment { list: first, waits: Vec::new() }],
            spare: Vec::new(),
            open: true,
            seg_work: false,
            work_since_barrier: false,
            sh: Shadow::new(),
            heaps: [None; 2],
            last: MemoryRange::EMPTY,
            barriers: Vec::new(),
            globals: Vec::new(),
            textures: Vec::new(),
            buffers: Vec::new(),
            stats: ListStats::default(),
            private: Vec::new(),
        },
    });
    let p: Raw = Box::into_raw(obj).cast();
    if !pipeline.is_null() {
        enter(p).set_pipeline(pipeline);
    }
    Ok(p)
}

/// The host list to submit for `list`, when it is one host list with no waits (the common case).
/// A list cut at memory waits has several: use [`segments`].
pub fn host_list<'a>(list: Raw) -> Option<&'a ID3D12GraphicsCommandList> {
    let s = state(list)?;
    match s.segments.as_slice() {
        [one] if one.waits.is_empty() => Some(&one.list.list),
        _ => None,
    }
}

/// The host lists to submit for `list`, in order, each after its waits hold.
pub fn segments<'a>(list: Raw) -> Option<&'a [Segment]> {
    state(list).map(|s| &s.segments[..])
}

/// The list's counts since its last reset.
pub fn stats(list: Raw) -> Option<ListStats> {
    state(list).map(|s| s.stats)
}

/// `QueryInterface` on the device the host side names (or `E_NOINTERFACE` without one).
pub(crate) unsafe fn device_query(device: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if device.is_null() {
        unsafe { *out = NULL };
        return E_NOINTERFACE;
    }
    // SAFETY: a COM-shaped object: its first word is a table whose slot 0 is QueryInterface.
    unsafe {
        let qi: unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT = std::mem::transmute(**device.cast::<*const *const ()>());
        qi(device, iid, out)
    }
}

// ---- IUnknown, object, device child, command list ----

/// The console's own `ID3D12GraphicsCommandList5` / `6` (the title asks for 6; the PC ids of 5 and
/// 6 differ), from the fork's notes.
const CONSOLE_LIST5: GUID = GUID::from_u128(0x55050859_4024_474c_98cf_645b51dc4874);
const CONSOLE_LIST6: GUID = GUID::from_u128(0xc3827890_4024_474c_98cf_645b51dc4874);

const IIDS: [GUID; 12] = [
    ID3D12GraphicsCommandList::IID,
    CONSOLE_LIST6,
    CONSOLE_LIST5,
    ID3D12GraphicsCommandList4::IID,
    ID3D12GraphicsCommandList3::IID,
    ID3D12GraphicsCommandList2::IID,
    ID3D12GraphicsCommandList1::IID,
    ID3D12CommandList::IID,
    ID3D12DeviceChild::IID,
    ID3D12Object::IID,
    ID3D12Pageable::IID,
    windows::core::IUnknown::IID,
];

unsafe extern "system" fn query(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        if !iid.is_null() && IIDS.contains(&*iid) {
            add_ref(this);
            *out = this;
            return S_OK;
        }
        *out = NULL;
        if let (Some(h), false) = (host_of(this), iid.is_null()) {
            h.unknown_iid("list", &*iid);
        }
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: Raw) -> u32 {
    unsafe { (*this.cast::<Header>()).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: Raw) -> u32 {
    let left = unsafe { (*this.cast::<Header>()).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        drop(unsafe { Box::from_raw(this.cast::<ListObject>()) });
    }
    left
}

unsafe extern "system" fn get_private_data(this: Raw, guid: *const GUID, size: *mut u32, data: *mut u8) -> HRESULT {
    if guid.is_null() || size.is_null() {
        return E_INVALIDARG;
    }
    let s = enter(this);
    unsafe {
        let Some((_, v)) = s.private.iter().find(|(g, _)| *g == *guid) else {
            *size = 0;
            return DXGI_ERROR_NOT_FOUND;
        };
        let have = *size;
        *size = v.len() as u32;
        if data.is_null() {
            return S_OK;
        }
        if (have as usize) < v.len() {
            return DXGI_ERROR_MORE_DATA;
        }
        data.copy_from_nonoverlapping(v.as_ptr(), v.len());
    }
    S_OK
}

unsafe extern "system" fn set_private_data(this: Raw, guid: *const GUID, size: u32, data: *const u8) -> HRESULT {
    if guid.is_null() {
        return E_INVALIDARG;
    }
    let s = enter(this);
    let guid = unsafe { *guid };
    s.private.retain(|(g, _)| *g != guid);
    if !data.is_null() && size > 0 {
        let v = unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec();
        s.private.push((guid, v));
    }
    S_OK
}

unsafe extern "system" fn set_name(this: Raw, _name: *const u16) -> HRESULT {
    enter(this);
    S_OK
}

unsafe extern "system" fn get_device(this: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
    let s = enter(this);
    unsafe { device_query(s.host.device(), iid, out) }
}

unsafe extern "system" fn get_type(this: Raw) -> u32 {
    enter(this).kind
}

/// 0x48 Close. Plan decision 3's "translate at Close" is already done: every call was
/// translated as it came, on this thread.
unsafe extern "system" fn close(this: Raw) -> HRESULT {
    enter(this).close_current()
}

/// 0x50 Reset `(allocator, pipeline)`.
unsafe extern "system" fn reset(this: Raw, allocator: Raw, pipeline: Raw) -> HRESULT {
    let s = enter(this);
    let Some(a) = alloc::state(allocator) else { return E_INVALIDARG };
    let host_allocator = a.allocator.clone();
    if s.open {
        let _ = s.close_current();
    }
    while s.segments.len() > 1 {
        let seg = s.segments.pop().expect("more than one");
        s.spare.push(seg.list);
    }
    s.segments[0].waits.clear();
    if let Err(e) = unsafe { s.segments[0].list.list.Reset(&host_allocator, None) } {
        return e.code();
    }
    s.allocator = allocator;
    s.point_at_current();
    s.open = true;
    s.seg_work = false;
    s.work_since_barrier = false;
    s.sh.clear();
    s.heaps = [None; 2];
    s.stats = ListStats::default();
    if !pipeline.is_null() {
        s.set_pipeline(pipeline);
    }
    S_OK
}

/// 0x458 ResetX `(allocator, pipeline, flags)`: the flags tune the console's command memory.
unsafe extern "system" fn reset_x(this: Raw, allocator: Raw, pipeline: Raw, _flags: u32) -> HRESULT {
    unsafe { reset(this, allocator, pipeline) }
}

// ---- draws, dispatches, copies ----

unsafe extern "system" fn draw_instanced(this: Raw, vertices: u32, instances: u32, first_vertex: u32, first_instance: u32) {
    let s = enter(this);
    s.work();
    s.stats.draws += 1;
    call!(s.DrawInstanced(vertices, instances, first_vertex, first_instance));
}

unsafe extern "system" fn draw_indexed(this: Raw, indices: u32, instances: u32, first_index: u32, base_vertex: i32, first_instance: u32) {
    let s = enter(this);
    s.work();
    s.stats.draws += 1;
    call!(s.DrawIndexedInstanced(indices, instances, first_index, base_vertex, first_instance));
}

unsafe extern "system" fn dispatch(this: Raw, x: u32, y: u32, z: u32) {
    let s = enter(this);
    s.work();
    s.stats.dispatches += 1;
    call!(s.Dispatch(x, y, z));
}

/// 0x280 DispatchX `(x, y, z, flags)` (the BVH builder's).
unsafe extern "system" fn dispatch_x(this: Raw, x: u32, y: u32, z: u32, _flags: u32) {
    unsafe { dispatch(this, x, y, z) }
}

/// 0x408 DispatchMesh.
unsafe extern "system" fn dispatch_mesh(this: Raw, x: u32, y: u32, z: u32) {
    let s = enter(this);
    s.work();
    s.stats.dispatches += 1;
    match &s.current().l6 {
        Some(l6) => unsafe { l6.DispatchMesh(x, y, z) },
        None => s.stats.refused += 1,
    }
}

/// 0x1e8 ExecuteIndirect `(signature, max count, args, args offset, count buffer, count offset)`.
unsafe extern "system" fn execute_indirect(this: Raw, signature: Raw, max: u32, args: Raw, args_offset: u64, count: Raw, count_offset: u64) {
    let s = enter(this);
    s.work();
    s.stats.draws += 1;
    let Some(sig) = s.host.command_signature(signature) else {
        s.stats.refused += 1;
        return;
    };
    let (Some(a), Some(c)) = (s.resource(args), s.resource(count)) else { return };
    call!(s.ExecuteIndirect(sig, max, a, args_offset, c, count_offset));
}

unsafe extern "system" fn copy_buffer_region(this: Raw, dst: Raw, dst_offset: u64, src: Raw, src_offset: u64, bytes: u64) {
    let s = enter(this);
    s.work();
    let (Some(d), Some(r)) = (s.resource(dst), s.resource(src)) else { return };
    call!(s.CopyBufferRegion(d, dst_offset, r, src_offset, bytes));
}

unsafe extern "system" fn copy_resource(this: Raw, dst: Raw, src: Raw) {
    let s = enter(this);
    s.work();
    let (Some(d), Some(r)) = (s.resource(dst), s.resource(src)) else { return };
    call!(s.CopyResource(d, r));
}

/// 0x2b0 CopyResourceX `(dst, src, flags)`.
unsafe extern "system" fn copy_resource_x(this: Raw, dst: Raw, src: Raw, _flags: u32) {
    unsafe { copy_resource(this, dst, src) }
}

/// 0x1f0 CopyMemoryToMemoryX `(dst va, src va, size, flags)`: a buffer copy between the host
/// buffers behind the two addresses.
unsafe extern "system" fn copy_memory(this: Raw, dst: u64, src: u64, size: u64, _flags: u32) {
    let s = enter(this);
    s.work();
    let Some((d, d_off)) = s.place(dst, size) else {
        s.stats.refused += 1;
        return;
    };
    let Some((r, r_off)) = s.place(src, size) else {
        s.stats.refused += 1;
        return;
    };
    call!(s.CopyBufferRegion(d, d_off, r, r_off, size));
}

unsafe extern "system" fn discard(this: Raw, resource: Raw, region: *const D3D12_DISCARD_REGION) {
    let s = enter(this);
    if let Some(r) = s.resource(resource) {
        if !r.is_null() {
            call!(s.DiscardResource(r, region));
        }
    }
}

// ---- fixed-function state ----

unsafe extern "system" fn topology(this: Raw, t: i32) {
    let s = enter(this);
    if s.sh.topology == Some(t) {
        s.stats.elided += 1;
        return;
    }
    s.sh.topology = Some(t);
    call!(s.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(t)));
}

unsafe extern "system" fn viewports(this: Raw, n: u32, v: *const D3D12_VIEWPORT) {
    let s = enter(this);
    let n = n.min(16);
    if !v.is_null() {
        unsafe { s.sh.viewports.as_mut_ptr().copy_from_nonoverlapping(v, n as usize) };
        s.sh.viewport_count = n;
    }
    call!(s.RSSetViewports(n, v));
}

unsafe extern "system" fn scissors(this: Raw, n: u32, r: *const RECT) {
    let s = enter(this);
    let n = n.min(16);
    if !r.is_null() {
        unsafe { s.sh.scissors.as_mut_ptr().copy_from_nonoverlapping(r, n as usize) };
        s.sh.scissor_count = n;
    }
    call!(s.RSSetScissorRects(n, r));
}

unsafe extern "system" fn blend_factor(this: Raw, f: *const f32) {
    let s = enter(this);
    s.sh.blend = (!f.is_null()).then(|| unsafe { *f.cast::<[f32; 4]>() });
    call!(s.OMSetBlendFactor(f));
}

unsafe extern "system" fn stencil_ref(this: Raw, r: u32) {
    let s = enter(this);
    s.sh.stencil = Some(r);
    call!(s.OMSetStencilRef(r));
}

/// 0x230 OMSetDepthBounds.
unsafe extern "system" fn depth_bounds(this: Raw, min: f32, max: f32) {
    let s = enter(this);
    s.sh.depth_bounds = Some((min, max));
    match &s.current().l1 {
        Some(l1) => unsafe { l1.OMSetDepthBounds(min, max) },
        None => s.stats.refused += 1,
    }
}

unsafe extern "system" fn set_pipeline(this: Raw, pipeline: Raw) {
    enter(this).set_pipeline(pipeline);
}

// ---- barriers ----

/// The console's barrier is D3D12's: 32 bytes, `(type, flags, union)`.
const _: () = assert!(size_of::<D3D12_RESOURCE_BARRIER>() == 32);

#[repr(C)]
#[derive(Clone, Copy)]
struct ConsoleBarrier {
    kind: u32,
    flags: u32,
    /// transition: resource; aliasing: before; UAV: resource
    a: Raw,
    /// transition: subresource (low) and state before (high); aliasing: after
    b: u64,
    /// transition: state after
    c: u64,
}

/// 0xd0 ResourceBarrier `(count, barriers)`: resources mapped to the host's; a barrier on a
/// resource the host side does not know becomes one barrier on every shader write, so ordering
/// is never lost. Returns the console's barrier "actions" (none).
unsafe extern "system" fn resource_barrier(this: Raw, n: u32, barriers: *const ConsoleBarrier) -> u32 {
    let s = enter(this);
    if barriers.is_null() || n == 0 {
        return 0;
    }
    let items = unsafe { std::slice::from_raw_parts(barriers, n as usize) };
    let mut out = std::mem::take(&mut s.barriers);
    out.clear();
    let mut global = false;
    for b in items {
        let mut h = *b;
        match b.kind {
            0 | 2 => match s.resource(b.a) {
                Some(r) => h.a = r,
                None => {
                    global = true;
                    continue;
                }
            },
            1 => {
                h.a = s.resource(b.a).unwrap_or(NULL);
                h.b = s.resource(b.b as Raw).unwrap_or(NULL) as u64;
            }
            _ => {
                s.stats.refused += 1;
                continue;
            }
        }
        // SAFETY: the same layout (asserted above).
        out.push(unsafe { std::mem::transmute::<ConsoleBarrier, D3D12_RESOURCE_BARRIER>(h) });
    }
    if global {
        out.push(D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() });
    }
    if !out.is_empty() {
        call!(s.ResourceBarrier(out.len() as u32, out.as_ptr()));
        s.work_since_barrier = false;
        s.seg_work = true;
    }
    // The host barriers borrow their resources: forget them without a Release.
    unsafe { out.set_len(0) };
    s.barriers = out;
    0
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConsoleGroup {
    /// 0 global, 1 texture, 2 buffer (D3D12_BARRIER_TYPE)
    kind: u32,
    count: u32,
    items: *const u8,
}

const _: () = {
    assert!(size_of::<ConsoleGroup>() == size_of::<D3D12_BARRIER_GROUP>());
    assert!(size_of::<D3D12_GLOBAL_BARRIER>() == 16);
    assert!(size_of::<D3D12_TEXTURE_BARRIER>() == 64);
    assert!(size_of::<D3D12_BUFFER_BARRIER>() == 40);
    assert!(offset_of!(D3D12_TEXTURE_BARRIER, pResource) == 24);
    assert!(offset_of!(D3D12_BUFFER_BARRIER, pResource) == 16);
};

/// 0x490 the enhanced barrier groups `(group count, groups)` (ID3D12GraphicsCommandList7::Barrier
/// with the console's layout, which is PC's: 16-byte groups, 40-byte buffer barriers, 64-byte
/// texture barriers). With host enhanced barriers they go through with resources mapped; a
/// barrier on an unknown resource keeps its sync and access as a global barrier. Without them,
/// one barrier on every shader write keeps the ordering (layouts are then the host's own).
unsafe extern "system" fn barrier_groups(this: Raw, count: u32, groups: *const ConsoleGroup) {
    let s = enter(this);
    if groups.is_null() || count == 0 {
        return;
    }
    let l7 = s.current().l7.clone();
    let (true, Some(l7)) = (s.enhanced_barriers, l7) else {
        s.global_barrier();
        return;
    };
    let groups = unsafe { std::slice::from_raw_parts(groups, count as usize) };
    let (mut globals, mut textures, mut buffers) = (std::mem::take(&mut s.globals), std::mem::take(&mut s.textures), std::mem::take(&mut s.buffers));
    globals.clear();
    textures.clear();
    buffers.clear();
    for g in groups {
        if g.items.is_null() {
            continue;
        }
        for i in 0..g.count as usize {
            unsafe {
                match g.kind {
                    0 => globals.push(g.items.add(i * 16).cast::<[u8; 16]>().read_unaligned()),
                    1 => {
                        let mut t = g.items.add(i * 64).cast::<[u8; 64]>().read_unaligned();
                        match s.resource(t.as_ptr().add(24).cast::<Raw>().read_unaligned()) {
                            Some(r) => {
                                t.as_mut_ptr().add(24).cast::<Raw>().write_unaligned(r);
                                textures.push(t);
                            }
                            None => globals.push(t[..16].try_into().expect("16 bytes")),
                        }
                    }
                    2 => {
                        let mut b = g.items.add(i * 40).cast::<[u8; 40]>().read_unaligned();
                        match s.resource(b.as_ptr().add(16).cast::<Raw>().read_unaligned()) {
                            Some(r) => {
                                b.as_mut_ptr().add(16).cast::<Raw>().write_unaligned(r);
                                buffers.push(b);
                            }
                            None => globals.push(b[..16].try_into().expect("16 bytes")),
                        }
                    }
                    _ => s.stats.refused += 1,
                }
            }
        }
    }
    let mut out = [ConsoleGroup { kind: 0, count: 0, items: std::ptr::null() }; 3];
    let mut n = 0;
    for (kind, len, items) in [
        (0, globals.len(), globals.as_ptr().cast::<u8>()),
        (1, textures.len(), textures.as_ptr().cast::<u8>()),
        (2, buffers.len(), buffers.as_ptr().cast::<u8>()),
    ] {
        if len > 0 {
            out[n] = ConsoleGroup { kind, count: len as u32, items };
            n += 1;
        }
    }
    if n > 0 {
        unsafe { (l7.vtable().Barrier)(l7.as_raw(), n as u32, out.as_ptr().cast()) };
        s.work_since_barrier = false;
        s.seg_work = true;
    }
    (s.globals, s.textures, s.buffers) = (globals, textures, buffers);
}

/// 0x418 / 0x420 pipeline syncs `(flags, control, address, size)`: wait for earlier work and
/// flush caches, at the top or bottom of the pipe. On the host that is a barrier on every
/// shader write, and none is needed if nothing ran since the last barrier.
unsafe extern "system" fn pipe_sync(this: Raw, _flags: u32, _control: u32, _address: u64, _size: u64) {
    let s = enter(this);
    if s.work_since_barrier {
        s.global_barrier();
    } else {
        s.stats.elided += 1;
    }
}

// ---- descriptor heaps and root arguments ----

/// 0x100 SetDescriptorHeaps `(count, heaps)`.
unsafe extern "system" fn descriptor_heaps(this: Raw, n: u32, heaps: *const Raw) {
    let s = enter(this);
    let given = if heaps.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(heaps, n.min(2) as usize) } };
    if given.len() == s.sh.heap_count as usize && given == &s.sh.heaps_console[..given.len()] {
        s.stats.elided += 1;
        return;
    }
    let mut count = 0;
    s.heaps = [None; 2];
    for &h in given {
        match s.host.descriptor_heap(h) {
            Some(b) => {
                s.heaps[count] = Some(b);
                s.sh.heaps_console[count] = h;
                s.sh.heaps_host[count] = b.heap;
                count += 1;
            }
            None => s.stats.refused += 1,
        }
    }
    s.sh.heap_count = count as u32;
    call!(s.SetDescriptorHeaps(count as u32, s.sh.heaps_host.as_ptr()));
}

/// 0x108 / 0x110 Set{Compute,Graphics}RootSignature.
unsafe extern "system" fn root_signature<const K: usize>(this: Raw, root: Raw) {
    let s = enter(this);
    match s.host.root_signature(root) {
        Some(r) => s.set_root(K, r),
        // the pipeline brings its own
        None => s.stats.elided += 1,
    }
}

unsafe extern "system" fn root_table<const K: usize>(this: Raw, slot: u32, handle: u64) {
    let s = enter(this);
    let i = slot as usize & 63;
    let a = s.sh.args[K][i];
    if a.kind == 1 && a.console == handle {
        s.stats.elided += 1;
        return;
    }
    let Some(h) = s.table(handle) else {
        s.stats.refused += 1;
        return;
    };
    s.sh.args[K][i] = Arg { kind: 1, console: handle, host: h };
    let h = D3D12_GPU_DESCRIPTOR_HANDLE { ptr: h };
    if K == 0 {
        call!(s.SetGraphicsRootDescriptorTable(slot, h));
    } else {
        call!(s.SetComputeRootDescriptorTable(slot, h));
    }
}

/// Root CBV (`V` = 2), SRV (3) and UAV (4) at a console GPU address.
unsafe extern "system" fn root_view<const K: usize, const V: u8>(this: Raw, slot: u32, va: u64) {
    let s = enter(this);
    let i = slot as usize & 63;
    let a = s.sh.args[K][i];
    if a.kind == V && a.console == va {
        s.stats.elided += 1;
        return;
    }
    let Some(h) = s.va(va) else {
        s.stats.refused += 1;
        return;
    };
    s.sh.args[K][i] = Arg { kind: V, console: va, host: h };
    match (K, V) {
        (0, 2) => call!(s.SetGraphicsRootConstantBufferView(slot, h)),
        (1, 2) => call!(s.SetComputeRootConstantBufferView(slot, h)),
        (0, 3) => call!(s.SetGraphicsRootShaderResourceView(slot, h)),
        (1, 3) => call!(s.SetComputeRootShaderResourceView(slot, h)),
        (0, _) => call!(s.SetGraphicsRootUnorderedAccessView(slot, h)),
        _ => call!(s.SetComputeRootUnorderedAccessView(slot, h)),
    }
}

impl ListState {
    #[inline]
    fn keep_consts(&mut self, k: usize, slot: u32, values: &[u32], offset: u32) {
        let c = self.sh.consts[k].get_or_insert_with(|| Box::new(Consts { v: [[0; 64]; 64], mask: [0; 64] }));
        let slot = slot as usize & 63;
        for (i, &v) in values.iter().enumerate() {
            let o = offset as usize + i;
            if o < 64 {
                c.v[slot][o] = v;
                c.mask[slot] |= 1 << o;
            }
        }
    }
}

unsafe extern "system" fn root_constant<const K: usize>(this: Raw, slot: u32, value: u32, offset: u32) {
    let s = enter(this);
    s.keep_consts(K, slot, &[value], offset);
    if K == 0 {
        call!(s.SetGraphicsRoot32BitConstant(slot, value, offset));
    } else {
        call!(s.SetComputeRoot32BitConstant(slot, value, offset));
    }
}

unsafe extern "system" fn root_constants<const K: usize>(this: Raw, slot: u32, n: u32, data: *const u32, offset: u32) {
    let s = enter(this);
    if data.is_null() {
        s.stats.refused += 1;
        return;
    }
    s.keep_consts(K, slot, unsafe { std::slice::from_raw_parts(data, n.min(64) as usize) }, offset);
    if K == 0 {
        call!(s.SetGraphicsRoot32BitConstants(slot, n, data.cast(), offset));
    } else {
        call!(s.SetComputeRoot32BitConstants(slot, n, data.cast(), offset));
    }
}

// ---- input assembler, targets, clears ----

unsafe extern "system" fn index_buffer(this: Raw, view: *const D3D12_INDEX_BUFFER_VIEW) {
    let s = enter(this);
    if view.is_null() {
        s.sh.ib_console = None;
        call!(s.IASetIndexBuffer(std::ptr::null()));
        return;
    }
    let v = unsafe { *view };
    let key = (v.BufferLocation, v.SizeInBytes, v.Format.0 as u32);
    if s.sh.ib_console == Some(key) {
        s.stats.elided += 1;
        return;
    }
    let Some(va) = s.va(v.BufferLocation) else {
        s.stats.refused += 1;
        return;
    };
    s.sh.ib_console = Some(key);
    s.sh.ib = D3D12_INDEX_BUFFER_VIEW { BufferLocation: va, ..v };
    call!(s.IASetIndexBuffer(&s.sh.ib));
}

unsafe extern "system" fn vertex_buffers(this: Raw, start: u32, n: u32, views: *const D3D12_VERTEX_BUFFER_VIEW) {
    let s = enter(this);
    let start = start.min(32);
    let n = n.min(32 - start);
    let mut changed = false;
    for i in 0..n as usize {
        let slot = start as usize + i;
        let v = if views.is_null() { D3D12_VERTEX_BUFFER_VIEW::default() } else { unsafe { *views.add(i) } };
        let key = (v.BufferLocation, v.SizeInBytes, v.StrideInBytes);
        if s.sh.vb_console[slot] == key {
            continue;
        }
        let va = match s.va(v.BufferLocation) {
            Some(va) => va,
            None => {
                s.stats.refused += 1;
                0
            }
        };
        s.sh.vb_console[slot] = key;
        s.sh.vbs[slot] = D3D12_VERTEX_BUFFER_VIEW { BufferLocation: va, ..v };
        changed = true;
    }
    if !changed {
        s.stats.elided += 1;
        return;
    }
    s.sh.vb_used = s.sh.vb_used.max(start + n);
    call!(s.IASetVertexBuffers(start, n, s.sh.vbs.as_ptr().add(start as usize)));
}

/// 0x190 OMSetRenderTargets `(count, handles, single range, depth)`: each console CPU handle to
/// the host's (a single range steps by the console's 128-byte increment).
unsafe extern "system" fn render_targets(this: Raw, n: u32, rts: *const D3D12_CPU_DESCRIPTOR_HANDLE, single: BOOL, dsv: *const D3D12_CPU_DESCRIPTOR_HANDLE) {
    let s = enter(this);
    let n = n.min(8);
    for i in 0..n as usize {
        let console = if rts.is_null() {
            0
        } else if single.as_bool() {
            unsafe { (*rts).ptr + i * CONSOLE_RTV_INCREMENT }
        } else {
            unsafe { (*rts.add(i)).ptr }
        };
        s.sh.rts[i].ptr = match s.host.cpu_descriptor(console) {
            Some(h) => h,
            None => {
                s.stats.refused += 1;
                0
            }
        };
    }
    s.sh.rt_count = n;
    s.sh.dsv = if dsv.is_null() {
        None
    } else {
        match s.host.cpu_descriptor(unsafe { (*dsv).ptr }) {
            Some(h) => Some(D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h }),
            None => {
                s.stats.refused += 1;
                None
            }
        }
    };
    s.sh.rts_set = true;
    let d = s.sh.dsv.as_ref().map_or(std::ptr::null(), |d| d as *const _);
    call!(s.OMSetRenderTargets(n, s.sh.rts.as_ptr(), BOOL(0), d));
}

unsafe extern "system" fn clear_dsv(this: Raw, dsv: D3D12_CPU_DESCRIPTOR_HANDLE, flags: D3D12_CLEAR_FLAGS, depth: f32, stencil: u8, n: u32, rects: *const RECT) {
    let s = enter(this);
    s.work();
    match s.host.cpu_descriptor(dsv.ptr) {
        Some(h) => call!(s.ClearDepthStencilView(D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h }, flags, depth, stencil, n, rects)),
        None => s.stats.refused += 1,
    }
}

unsafe extern "system" fn clear_rtv(this: Raw, rtv: D3D12_CPU_DESCRIPTOR_HANDLE, color: *const f32, n: u32, rects: *const RECT) {
    let s = enter(this);
    s.work();
    match s.host.cpu_descriptor(rtv.ptr) {
        Some(h) => call!(s.ClearRenderTargetView(D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h }, color, n, rects)),
        None => s.stats.refused += 1,
    }
}

/// The three halves of a UAV clear: the shader-visible handle, the CPU handle, the resource.
fn uav_clear(s: &mut ListState, gpu: u64, cpu: usize, resource: Raw) -> Option<(D3D12_GPU_DESCRIPTOR_HANDLE, D3D12_CPU_DESCRIPTOR_HANDLE, Raw)> {
    s.work();
    let g = s.table(gpu);
    let c = s.host.cpu_descriptor(cpu);
    let r = s.resource(resource);
    match (g, c, r) {
        (Some(g), Some(c), Some(r)) => Some((D3D12_GPU_DESCRIPTOR_HANDLE { ptr: g }, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: c }, r)),
        _ => {
            s.stats.refused += 1;
            None
        }
    }
}

unsafe extern "system" fn clear_uav_uint(this: Raw, gpu: u64, cpu: usize, resource: Raw, values: *const u32, n: u32, rects: *const RECT) {
    let s = enter(this);
    if let Some((g, c, r)) = uav_clear(s, gpu, cpu, resource) {
        call!(s.ClearUnorderedAccessViewUint(g, c, r, values, n, rects));
    }
}

unsafe extern "system" fn clear_uav_float(this: Raw, gpu: u64, cpu: usize, resource: Raw, values: *const f32, n: u32, rects: *const RECT) {
    let s = enter(this);
    if let Some((g, c, r)) = uav_clear(s, gpu, cpu, resource) {
        call!(s.ClearUnorderedAccessViewFloat(g, c, r, values, n, rects));
    }
}

// ---- console: labels, memory waits, upload memory, hints ----

fn label(this: Raw, address: u64, value: LabelValue, bytes: u32, flush: u32, flags: u32) {
    let s = enter(this);
    s.work();
    let host_va = s.va(address).filter(|_| address != 0);
    let w = LabelWrite { address, value, bytes, flush, flags };
    let done = match &s.current().l2 {
        Some(l2) => s.host.label(l2, &w, host_va),
        None => false,
    };
    if !done {
        s.stats.refused += 1;
    }
}

/// 0x238 a 32-bit value at the bottom of the pipe `(address, value, flush, flags)`.
unsafe extern "system" fn write32(this: Raw, address: u64, value: u32, flush: u32, flags: u32) {
    label(this, address, LabelValue::Value(value as u64), 4, flush, flags);
}

/// 0x240 a 64-bit value `(address, value, flush, flags)`: flag 1 writes the GPU timestamp
/// instead, flag 2 the system timestamp.
unsafe extern "system" fn write64(this: Raw, address: u64, value: u64, flush: u32, flags: u32) {
    let v = match flags {
        1 => LabelValue::GpuTimestamp,
        2 => LabelValue::SystemTimestamp,
        _ => LabelValue::Value(value),
    };
    label(this, address, v, 8, flush, flags);
}

fn memory_wait(this: Raw, address: u64, comparison: u32, reference: u64, wide: bool) {
    let s = enter(this);
    let w = MemoryWait { address, comparison, reference, wide };
    match s.host.memory_wait(&w) {
        WaitAction::Split => s.split(w),
        WaitAction::Drop => s.stats.refused += 1,
    }
}

/// 0x248 wait for a 32-bit memory value `(address, comparison, reference)`.
unsafe extern "system" fn wait32(this: Raw, address: u64, comparison: u32, reference: u64) {
    memory_wait(this, address, comparison, reference & 0xFFFF_FFFF, false);
}

/// 0x250 the 64-bit form.
unsafe extern "system" fn wait64(this: Raw, address: u64, comparison: u32, reference: u64) {
    memory_wait(this, address, comparison, reference, true);
}

/// 0x4a0 `(size, alignment) -> pointer`: upload memory owned by the list's allocator, or null.
unsafe extern "system" fn allocate_upload(this: Raw, size: u64, alignment: u64) -> *mut u8 {
    let s = enter(this);
    match s.allocator() {
        Some(a) => a.alloc(size, alignment),
        None => std::ptr::null_mut(),
    }
}

/// 0x328 DriverHintX `(feature, value) -> 0`.
unsafe extern "system" fn driver_hint(this: Raw, _feature: u64, _value: u64) -> u64 {
    enter(this);
    0
}

/// Calls with nothing to do on the host (markers; 0x258 pixel shader depth order; 0x470, which
/// the title's state flush calls with its result unused): counted, 0 returned.
unsafe extern "system" fn nothing(this: Raw) -> u64 {
    enter(this);
    0
}

/// Every slot we implement, at this title's offsets ("newer" lists: every 2020 slot from 0x1d8
/// on moved +0x10). Every other slot of the 160 is a trap ([`super::list_slots`]).
pub(crate) static IMPLEMENTED: &[(usize, &str, Fp)] = &[
    (0x000, "QueryInterface", Fp(query as *const ())),
    (0x008, "AddRef", Fp(add_ref as *const ())),
    (0x010, "Release", Fp(release as *const ())),
    (0x018, "GetPrivateData", Fp(get_private_data as *const ())),
    (0x020, "SetPrivateData", Fp(set_private_data as *const ())),
    (0x030, "SetName", Fp(set_name as *const ())),
    (0x038, "GetDevice", Fp(get_device as *const ())),
    (0x040, "GetType", Fp(get_type as *const ())),
    (0x048, "Close", Fp(close as *const ())),
    (0x050, "Reset", Fp(reset as *const ())),
    (0x060, "DrawInstanced", Fp(draw_instanced as *const ())),
    (0x068, "DrawIndexedInstanced", Fp(draw_indexed as *const ())),
    (0x070, "Dispatch", Fp(dispatch as *const ())),
    (0x078, "CopyBufferRegion", Fp(copy_buffer_region as *const ())),
    (0x088, "CopyResource", Fp(copy_resource as *const ())),
    (0x0a0, "IASetPrimitiveTopology", Fp(topology as *const ())),
    (0x0a8, "RSSetViewports", Fp(viewports as *const ())),
    (0x0b0, "RSSetScissorRects", Fp(scissors as *const ())),
    (0x0b8, "OMSetBlendFactor", Fp(blend_factor as *const ())),
    (0x0c0, "OMSetStencilRef", Fp(stencil_ref as *const ())),
    (0x0c8, "SetPipelineState", Fp(set_pipeline as *const ())),
    (0x0d0, "ResourceBarrier", Fp(resource_barrier as *const ())),
    (0x100, "SetDescriptorHeaps", Fp(descriptor_heaps as *const ())),
    (0x108, "SetComputeRootSignature", Fp(root_signature::<1> as *const ())),
    (0x110, "SetGraphicsRootSignature", Fp(root_signature::<0> as *const ())),
    (0x118, "SetComputeRootDescriptorTable", Fp(root_table::<1> as *const ())),
    (0x120, "SetGraphicsRootDescriptorTable", Fp(root_table::<0> as *const ())),
    (0x128, "SetComputeRoot32BitConstant", Fp(root_constant::<1> as *const ())),
    (0x130, "SetGraphicsRoot32BitConstant", Fp(root_constant::<0> as *const ())),
    (0x138, "SetComputeRoot32BitConstants", Fp(root_constants::<1> as *const ())),
    (0x140, "SetGraphicsRoot32BitConstants", Fp(root_constants::<0> as *const ())),
    (0x148, "SetComputeRootConstantBufferView", Fp(root_view::<1, 2> as *const ())),
    (0x150, "SetGraphicsRootConstantBufferView", Fp(root_view::<0, 2> as *const ())),
    (0x158, "SetComputeRootShaderResourceView", Fp(root_view::<1, 3> as *const ())),
    (0x160, "SetGraphicsRootShaderResourceView", Fp(root_view::<0, 3> as *const ())),
    (0x168, "SetComputeRootUnorderedAccessView", Fp(root_view::<1, 4> as *const ())),
    (0x170, "SetGraphicsRootUnorderedAccessView", Fp(root_view::<0, 4> as *const ())),
    (0x178, "IASetIndexBuffer", Fp(index_buffer as *const ())),
    (0x180, "IASetVertexBuffers", Fp(vertex_buffers as *const ())),
    (0x190, "OMSetRenderTargets", Fp(render_targets as *const ())),
    (0x198, "ClearDepthStencilView", Fp(clear_dsv as *const ())),
    (0x1a0, "ClearRenderTargetView", Fp(clear_rtv as *const ())),
    (0x1a8, "ClearUnorderedAccessViewUint", Fp(clear_uav_uint as *const ())),
    (0x1b0, "ClearUnorderedAccessViewFloat", Fp(clear_uav_float as *const ())),
    (0x1b8, "DiscardResource", Fp(discard as *const ())),
    (0x1c0, "SetMarker (ignored)", Fp(nothing as *const ())),
    (0x1c8, "BeginEvent (ignored)", Fp(nothing as *const ())),
    (0x1d0, "EndEvent (ignored)", Fp(nothing as *const ())),
    (0x1e8, "ExecuteIndirect", Fp(execute_indirect as *const ())),
    (0x1f0, "CopyMemoryToMemoryX", Fp(copy_memory as *const ())),
    (0x230, "OMSetDepthBounds", Fp(depth_bounds as *const ())),
    (0x238, "bottom-of-pipe write 32 (label)", Fp(write32 as *const ())),
    (0x240, "bottom-of-pipe write 64 (label)", Fp(write64 as *const ())),
    (0x248, "wait for memory value 32 (GPU wait)", Fp(wait32 as *const ())),
    (0x250, "wait for memory value 64 (GPU wait)", Fp(wait64 as *const ())),
    (0x258, "pixel shader depth order (ignored)", Fp(nothing as *const ())),
    (0x280, "DispatchX", Fp(dispatch_x as *const ())),
    (0x2b0, "CopyResourceX", Fp(copy_resource_x as *const ())),
    (0x328, "DriverHintX", Fp(driver_hint as *const ())),
    (0x408, "DispatchMesh", Fp(dispatch_mesh as *const ())),
    (0x418, "pipeline sync, top of pipe", Fp(pipe_sync as *const ())),
    (0x420, "pipeline sync, bottom of pipe", Fp(pipe_sync as *const ())),
    (0x458, "ResetX", Fp(reset_x as *const ())),
    (0x470, "unknown (u64), result unused (ignored)", Fp(nothing as *const ())),
    (0x490, "Barrier (enhanced barrier groups)", Fp(barrier_groups as *const ())),
    (0x4a0, "allocate list-owned upload memory", Fp(allocate_upload as *const ())),
];
