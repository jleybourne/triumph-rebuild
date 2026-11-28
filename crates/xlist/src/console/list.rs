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
use super::indirect;
use super::slots::{self, Fp};
use super::{Backend, HeapBinding, Header, LabelValue, LabelWrite, ListHost, MemoryRange, MemoryWait, Raw, UserMap, UserSource, WaitAction, CONSOLE_RTV_INCREMENT, TITLE_BYTES, USER_DWORDS};

const DXGI_ERROR_NOT_FOUND: HRESULT = HRESULT(0x887A_0002_u32 as i32);
const DXGI_ERROR_MORE_DATA: HRESULT = HRESULT(0x887A_0003_u32 as i32);

/// A host list and the newer interfaces it answers (asked once, when it is made).
pub struct HostList {
    pub list: ID3D12GraphicsCommandList,
    l1: Option<ID3D12GraphicsCommandList1>,
    l2: Option<ID3D12GraphicsCommandList2>,
    l6: Option<ID3D12GraphicsCommandList6>,
    l7: Option<ID3D12GraphicsCommandList7>,
    /// The submitting queue (an address) and its progress value after this list's last execution, for the lead's
    /// submission: a host list may not run again before that execution is done (the console lets titles do it).
    pub executed_on: std::sync::atomic::AtomicUsize,
    pub executed_at: std::sync::atomic::AtomicU64,
    /// Its host Close failed (a call the host refused while recording): executing it would remove the device.
    pub bad: std::sync::atomic::AtomicBool,
    /// Its timing pair in the device's timestamp ring ([`super::stamps`]): the query at its first command, the one
    /// after its last at +1. [`super::stamps::NONE`] for a list that is not timed.
    pub stamp: std::sync::atomic::AtomicU32,
}

impl HostList {
    fn new(list: ID3D12GraphicsCommandList) -> HostList {
        HostList {
            l1: list.cast().ok(),
            l2: list.cast().ok(),
            l6: list.cast().ok(),
            l7: list.cast().ok(),
            list,
            executed_on: std::sync::atomic::AtomicUsize::new(0),
            executed_at: std::sync::atomic::AtomicU64::new(0),
            bad: std::sync::atomic::AtomicBool::new(false),
            stamp: std::sync::atomic::AtomicU32::new(super::stamps::NONE),
        }
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
    /// draws and dispatches dropped because their pipeline has no host object (never run with the one before)
    pub skipped: u32,
    /// user-data writes in the program library's root layout (root constants, one call per stage that changed)
    pub user_sets: u32,
    /// descriptor shadow tables bound (`t0, space101`)
    pub shadow_sets: u32,
    /// user-data dwords made from a root argument the title had not set, or a table outside the bound heaps (0)
    pub user_unset: u32,
    /// of `skipped`: no host pipeline bound (not in the library, or no user map)
    pub skipped_pipeline: u32,
    /// of `skipped`: the draw's topology is outside the bound pipeline's type
    pub skipped_topology: u32,
    /// render-target and depth clears recorded
    pub clears: u32,
    /// copies recorded (buffer, resource, memory)
    pub copies: u32,
    /// OMSetRenderTargets calls, and render targets in them the host had no view for (bound as nothing)
    pub rt_sets: u32,
    pub rt_unmapped: u32,
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
    /// the console pipeline set last has no host pipeline: draws and dispatches are dropped until one does
    pso_missing: bool,
    /// the bound PSO's topology type (0: not known)
    pso_topology: u8,
    /// the bound PSO draws rectangle lists through its geometry stage ([`HostPipeline::rects`])
    pso_rects: bool,
    pso: Raw,
    /// the side (0 graphics, 1 compute) of the pipeline set last: what ExecuteIndirect runs
    pso_side: usize,
    /// host root signatures, 0 graphics, 1 compute
    root: [Raw; 2],
    /// the title's root signatures (console objects): its arguments live until it changes
    title_root: [Raw; 2],
    /// the title's root arguments by its parameter numbers: on the host list too while `user` is `None`,
    /// console state only while it is `Some`
    args: [[Arg; 64]; 2],
    consts: [Option<Box<Consts>>; 2],
    /// the bound translated pipeline's user-data map, per side (`docs/design/root-seam.md`)
    user: [Option<&'static UserMap>; 2],
    /// the library layout of a side may be out of date: written at its next draw or dispatch
    user_dirty: [bool; 2],
    /// the user-data words last written per side and stage, and whether the host list holds them
    user_words: [[[u32; USER_DWORDS]; 2]; 2],
    user_valid: [[bool; 2]; 2],
    /// the shadow table address bound per side (0: none)
    shadow_va: [u64; 2],
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
            pso_missing: false,
            pso_topology: 0,
            pso_rects: false,
            pso: NULL,
            pso_side: 0,
            root: [NULL; 2],
            title_root: [NULL; 2],
            args: [[Arg::default(); 64]; 2],
            consts: [None, None],
            user: [None; 2],
            user_dirty: [false; 2],
            user_words: [[[0; USER_DWORDS]; 2]; 2],
            user_valid: [[false; 2]; 2],
            shadow_va: [0; 2],
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

    /// The title's root arguments of side `k` are gone (its root signature changed).
    fn title_args_cleared(&mut self, k: usize) {
        self.args[k] = [Arg::default(); 64];
        if let Some(c) = self.consts[k].as_mut() {
            c.mask = [0; 64];
        }
        self.user_dirty[k] = true;
    }

    /// What the host list holds in the library layout of side `k` is to be written again (a new host root
    /// signature, a fresh host list).
    fn library_layout_lost(&mut self, k: usize) {
        self.user_valid[k] = [false; 2];
        self.shadow_va[k] = 0;
        self.user_dirty[k] = true;
    }
}

/// Descriptor memory a list's translated programs read through tables, per heap: synced before submission.
#[derive(Clone, Copy, Default)]
struct Span {
    heap: u64,
    lo: u64,
    hi: u64,
}

const SPANS: usize = 4;

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
    /// tables the translated programs read since the reset ([`sync_descriptors`])
    spans: [Span; SPANS],
    /// the device's timestamp rings (none on a stand-in backend)
    stamps: Option<Arc<super::stamps::Stamps>>,
    /// timestamp labels of the current host list, resolved at its end: (query, title buffer, its offset)
    stamp_labels: Vec<(u32, Raw, u64)>,
}

// SAFETY: a list is recorded by one thread at a time (D3D12's rule, and the title's).
unsafe impl Send for ListState {}

macro_rules! call {
    ($s:ident . $m:ident ( $($a:expr),* $(,)? )) => {
        unsafe { ((*$s.vt).$m)($s.raw, $($a),*) }
    };
}

// ---- tracing ----

/// `trace::ON` (set by the runtime for a range of frames): each list call that matters for what reaches the
/// screen is printed (pipelines, topology, draws and why they were dropped, targets, clears, refusals).
pub mod trace {
    use std::sync::atomic::{AtomicBool, Ordering};
    pub static ON: AtomicBool = AtomicBool::new(false);

    #[inline]
    pub fn on() -> bool {
        ON.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn refused(what: &str) {
        if on() {
            eprintln!("xcall     refused in {what}");
        }
    }
}

macro_rules! tr {
    ($($a:tt)*) => {
        if trace::on() {
            eprintln!("xcall     {}", format_args!($($a)*));
        }
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
            { self.stats.refused += 1; trace::refused("resource") };
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
        self.sh.root[k] = root;
        self.sh.library_layout_lost(k);
        // the title's arguments were on the host list in the title's layout: gone with that root signature
        if self.sh.user[k].is_none() {
            self.sh.title_args_cleared(k);
        }
    }

    fn set_pipeline(&mut self, console: Raw) {
        if console == self.sh.console_pipeline {
            self.stats.elided += 1;
            return;
        }
        self.sh.console_pipeline = console;
        let found = self.host.pipeline(console);
        tr!(
            "pipeline {console:p}: {}",
            match &found {
                Some(p) => format!("host {:p}, {} topology type {}{}, user map {}", p.pso, if p.compute { "compute" } else { "graphics" }, p.topology, if p.rects { " (rectangles, geometry stage)" } else { "" }, p.user.is_some()),
                None => "no host pipeline".to_string(),
            }
        );
        let Some(p) = found else {
            { self.stats.refused += 1; trace::refused("set_pipeline") };
            self.sh.pso_missing = true;
            return;
        };
        self.sh.pso_missing = false;
        self.sh.pso_topology = p.topology;
        self.sh.pso_rects = p.rects;
        let k = p.compute as usize;
        self.sh.pso_side = k;
        if self.sh.user[k] != p.user {
            self.sh.user[k] = p.user;
            self.sh.user_dirty[k] = true;
        }
        if !p.root_signature.is_null() {
            self.set_root(k, p.root_signature);
        }
        if p.pso != self.sh.pso {
            call!(self.SetPipelineState(p.pso));
            self.sh.pso = p.pso;
        }
    }

    /// Before a draw (`k` 0) or dispatch (`k` 1): if the title's root arguments, the pipeline, the heaps or the
    /// host list changed since the last one, write the bound translated pipeline's user data and shadow table
    /// in the library's layout. Only stages whose words changed are written. No lock, no allocation.
    /// False when the pipeline's programs read a shadow table and no bound heap has one: its root SRV would be
    /// address 0 (WARP faults on the read, a card page-faults and hangs), so the work is dropped (counted).
    #[inline]
    fn bind_library(&mut self, k: usize) -> bool {
        if self.sh.user_dirty[k] {
            self.write_library(k);
        }
        if self.sh.user[k].is_some() && self.sh.shadow_va[k] == 0 {
            NO_SHADOW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }
        true
    }

    fn write_library(&mut self, k: usize) {
        self.sh.user_dirty[k] = false;
        let Some(m) = self.sh.user[k] else { return };
        for st in 0..(m.stage_count as usize).min(2) {
            let stage = &m.stages[st];
            let n = (stage.dwords as usize).min(USER_DWORDS);
            if n == 0 {
                continue;
            }
            let mut w = [0u32; USER_DWORDS];
            for (i, src) in stage.src[..n].iter().enumerate() {
                w[i] = self.user_word(k, *src);
            }
            if self.sh.user_valid[k][st] && self.sh.user_words[k][st][..n] == w[..n] {
                continue;
            }
            self.sh.user_words[k][st] = w;
            self.sh.user_valid[k][st] = true;
            self.stats.user_sets += 1;
            if k == 0 {
                call!(self.SetGraphicsRoot32BitConstants(stage.param, n as u32, w.as_ptr().cast(), 0));
            } else {
                call!(self.SetComputeRoot32BitConstants(stage.param, n as u32, w.as_ptr().cast(), 0));
            }
        }
        // the shadow table of the bound view heap
        let va = self.heaps.iter().flatten().map(|h| h.shadow_va).find(|&va| va != 0).unwrap_or(0);
        if va != 0 && va != self.sh.shadow_va[k] {
            self.sh.shadow_va[k] = va;
            self.stats.shadow_sets += 1;
            if k == 0 {
                call!(self.SetGraphicsRootShaderResourceView(m.shadow_param, va));
            } else {
                call!(self.SetComputeRootShaderResourceView(m.shadow_param, va));
            }
        }
    }

    /// One user-data dword from the title's root arguments of side `k`.
    #[inline]
    fn user_word(&mut self, k: usize, src: UserSource) -> u32 {
        match src {
            UserSource::Zero => 0,
            UserSource::Constant { slot, dword } => match &self.sh.consts[k] {
                Some(c) if (dword as usize) < 64 && c.mask[slot as usize & 63] >> dword & 1 != 0 => c.v[slot as usize & 63][dword as usize],
                _ => {
                    tr!("user data side {k} constant slot {slot} dword {dword}: unset, 0");
                    self.stats.user_unset += 1;
                    0
                }
            },
            UserSource::Table { slot, bytes } => {
                let a = self.sh.args[k][slot as usize & 63];
                let heap = if a.kind == 1 { self.heaps.iter().flatten().find(|h| a.console.wrapping_sub(h.console_base) < h.console_bytes).copied() } else { None };
                match heap {
                    Some(h) => {
                        self.note_span(h.console_base, a.console, u64::from(bytes.max(h.console_stride)));
                        tr!("user data side {k} table slot {slot}: {:#x} in heap {:#x}: offset {:#x} (index {})", a.console, h.console_base, a.console - h.console_base, (a.console - h.console_base) / u64::from(h.console_stride.max(1)));
                        if trace::on() && h.console_stride == 32 {
                            // the table's first descriptors as the title's memory holds them now (records: 'XD' tag)
                            // SAFETY: inside the heap's bytes (the lookup above), which are the title's memory or ours
                            let w = unsafe { std::slice::from_raw_parts(a.console as *const u32, 16) };
                            tr!("  table words {:08x?}", w);
                        }
                        (a.console - h.console_base) as u32
                    }
                    None => {
                        tr!("user data side {k} table slot {slot}: {:#x} (kind {}) in no bound heap: 0", a.console, a.kind);
                        self.stats.user_unset += 1;
                        0
                    }
                }
            }
            UserSource::AddressLo { slot } | UserSource::AddressHi { slot } => {
                let a = self.sh.args[k][slot as usize & 63];
                if !(2..=4).contains(&a.kind) {
                    self.stats.user_unset += 1;
                    return 0;
                }
                if matches!(src, UserSource::AddressLo { .. }) {
                    a.console as u32
                } else {
                    (a.console >> 32) as u32
                }
            }
        }
    }

    /// Descriptors `at..at + bytes` of the heap at `heap` are read by this list: synced before submission.
    #[inline]
    fn note_span(&mut self, heap: u64, at: u64, bytes: u64) {
        let end = at + bytes;
        for s in &mut self.spans {
            if s.heap == heap && s.hi > s.lo {
                s.lo = s.lo.min(at);
                s.hi = s.hi.max(end);
                return;
            }
            if s.hi == s.lo {
                *s = Span { heap, lo: at, hi: end };
                return;
            }
        }
        // more heaps than spans: the last one grows (still exact for the slots it covers)
        let s = &mut self.spans[SPANS - 1];
        s.lo = s.lo.min(at);
        s.hi = s.hi.max(end);
    }

    /// A barrier that orders every earlier shader write before every later access.
    fn global_barrier(&mut self) {
        let b = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() };
        call!(self.ResourceBarrier(1, &b));
        self.work_since_barrier = false;
        self.seg_work = true;
    }

    /// A host list was opened: its first query of the timing pair (direct and compute lists).
    fn open_stamp(&mut self) {
        let mut at = super::stamps::NONE;
        if super::stamps::list_timing() && self.kind != 3 {
            if let Some((heap, i)) = self.stamps.as_ref().and_then(|s| s.take(self.kind, 2)) {
                call!(self.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, i));
                at = i;
            }
        }
        self.current().stamp.store(at, Ordering::Relaxed);
    }

    /// Before the host list closes: the timing pair's second query, then the timestamp labels into the title's
    /// memory (after the pair, so the list's time is the title's work alone).
    fn close_stamps(&mut self) {
        let at = self.current().stamp.load(Ordering::Relaxed);
        let Some(st) = self.stamps.clone() else { return };
        if at != super::stamps::NONE {
            call!(self.EndQuery(st.heap.as_raw(), D3D12_QUERY_TYPE_TIMESTAMP, at + 1));
        }
        if self.stamp_labels.is_empty() {
            return;
        }
        let heap = if self.kind == 3 { st.copy.as_ref().map(|h| h.as_raw()) } else { Some(st.heap.as_raw()) };
        let labels = std::mem::take(&mut self.stamp_labels);
        if let Some(heap) = heap {
            for &(i, resource, offset) in &labels {
                call!(self.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 1, resource, offset));
            }
            STAMP_LABELS.fetch_add(labels.len() as u64, Ordering::Relaxed);
        }
        self.stamp_labels = labels;
        self.stamp_labels.clear();
    }

    /// A GPU timestamp label at console `address`: a query now, resolved into the title's memory at the host list's
    /// end ([`close_stamps`](Self::close_stamps)). False when it cannot be (no ring for the list's type, an address
    /// no buffer holds, or one not 8-byte aligned, which the resolve requires). The range is the host's own (the
    /// title's memory), never the allocator's upload memory, which a resolve may not write.
    fn stamp_label(&mut self, address: u64) -> bool {
        if address == 0 || address % 8 != 0 || self.stamps.is_none() {
            return false;
        }
        let Some((resource, offset)) = self.host.cpu_memory(address).and_then(|r| r.place(address, 8)) else { return false };
        if offset % 8 != 0 {
            return false;
        }
        let Some((heap, i)) = self.stamps.as_ref().and_then(|s| s.take(self.kind, 1)) else { return false };
        call!(self.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, i));
        self.stamp_labels.push((i, resource, offset));
        true
    }

    fn close_current(&mut self) -> HRESULT {
        if !self.open {
            return E_FAIL;
        }
        self.open = false;
        self.close_stamps();
        let hr = call!(self.Close());
        // a list the host refused is never executed (the submit skips it); the title sees the console's S_OK
        let failed = hr.is_err();
        if let Some(seg) = self.segments.last() {
            seg.list.bad.store(failed, std::sync::atomic::Ordering::Relaxed);
        }
        if failed {
            CLOSE_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // the host allocator it recorded into is free for another list now
        let me = self as *const Self as usize;
        if let Some(a) = self.allocator() {
            a.release(me);
        }
        if failed { S_OK } else { hr }
    }

    /// A non-indexed rectangle-list draw, its per-vertex buffers expanded to two triangles a rectangle into the
    /// allocator's upload memory ([`super::rect`]). False (nothing recorded) when it cannot be: no input layout,
    /// vertices not from buffers, a buffer the CPU cannot read, no upload memory, or no shadow table.
    fn rect_draw(&mut self, vertices: u32, instances: u32, first_vertex: u32, first_instance: u32) -> bool {
        if vertices == 0 || vertices % 3 != 0 {
            return false;
        }
        let Some(layout) = self.host.input_layout(self.sh.console_pipeline) else { return false };
        let mut views = self.sh.vbs;
        let mut top = self.sh.vb_used as usize;
        let mut slots: Vec<u32> = layout.iter().filter(|e| !e.3).map(|e| e.0).collect();
        slots.sort_unstable();
        slots.dedup();
        if slots.is_empty() {
            return false; // positions from the vertex number: the geometry stage's work
        }
        let rects = u64::from(vertices / 3);
        for slot in slots {
            let i = slot as usize & 31;
            let (va, size, stride) = self.sh.vb_console[i];
            let (start, len) = (u64::from(first_vertex) * u64::from(stride), u64::from(vertices) * u64::from(stride));
            if stride == 0 || va == 0 || va == u64::MAX || start + len > u64::from(size) {
                return false;
            }
            // the bytes must lie in memory a range holds (console addresses are CPU addresses)
            if self.va(va + start).is_none() || self.va(va + start + len - 1).is_none() {
                return false;
            }
            let out = rects * 6 * u64::from(stride);
            let Some(a) = self.allocator() else { return false };
            let dst = a.alloc(out, 256);
            let Some(host) = (!dst.is_null()).then(|| a.find(dst as u64).and_then(|r| r.map(dst as u64))).flatten() else { return false };
            let elements: Vec<(u32, u32)> = layout.iter().filter(|e| e.0 == slot && !e.3).map(|e| (e.1, e.2)).collect();
            // SAFETY: `len` bytes of mapped memory (checked above), and `out` bytes of upload memory just taken
            let (src, dst) = unsafe { (std::slice::from_raw_parts((va + start) as *const u8, len as usize), std::slice::from_raw_parts_mut(dst, out as usize)) };
            super::rect::expand(src, dst, stride as usize, &elements);
            views[i] = D3D12_VERTEX_BUFFER_VIEW { BufferLocation: host, SizeInBytes: out as u32, StrideInBytes: stride };
            top = top.max(i + 1);
        }
        if !self.bind_library(0) {
            return false;
        }
        self.work();
        self.stats.draws += 1;
        RECTS_EXPANDED.fetch_add(1, Ordering::Relaxed);
        tr!("rectangle draw expanded: {rects} rectangles, {instances} instances");
        call!(self.IASetVertexBuffers(0, top as u32, views.as_ptr()));
        call!(self.DrawInstanced(rects as u32 * 6, instances, 0, first_instance));
        // the title's buffers again for what follows
        call!(self.IASetVertexBuffers(0, top as u32, self.sh.vbs.as_ptr()));
        true
    }

    /// A draw not recorded: counted, with its reason.
    fn skip_draw(&mut self) {
        tr!("draw dropped: pipeline missing {}, topology {:?} vs pipeline type {}", self.sh.pso_missing, self.sh.topology, self.sh.pso_topology);
        self.stats.skipped += 1;
        if self.sh.pso_missing {
            self.stats.skipped_pipeline += 1;
        } else if self.topology_mismatch() {
            self.stats.skipped_topology += 1;
        }
    }

    /// A draw's topology outside the bound PSO's type: the host refuses it, so it is dropped. A rectangle list
    /// fits a pipeline with the rectangle geometry stage (the host draws it as the triangle list it is set as),
    /// and only rectangle lists do.
    #[inline]
    fn topology_mismatch(&self) -> bool {
        if self.sh.pso_rects {
            return self.sh.topology.is_some_and(|t| t != super::rect::RECTLIST);
        }
        let want = self.sh.pso_topology;
        want != 0 && self.sh.topology.is_some_and(|t| topology_type(t) != want)
    }

    /// Cut the list at a memory wait: the work so far is one host list, the rest another that
    /// runs once `wait` holds.
    fn split(&mut self, wait: MemoryWait) {
        if !self.seg_work {
            self.segments.last_mut().expect("a segment").waits.push(wait);
            return;
        }
        if self.allocator().is_none() {
            { self.stats.refused += 1; trace::refused("split") };
            return;
        }
        let _ = self.close_current();
        let me = self as *const Self as usize;
        let Some(allocator) = self.allocator().and_then(|a| a.take(me).ok()) else {
            { self.stats.refused += 1; trace::refused("split") };
            return;
        };
        let next = match self.spare.pop() {
            Some(h) => unsafe { h.list.Reset(&allocator, None) }.map(|_| h),
            None => self.backend.list(self.kind, &allocator).map(HostList::new),
        };
        match next {
            Ok(h) => {
                self.segments.push(Segment { list: h, waits: vec![wait] });
                self.open = true;
                self.point_at_current();
                self.open_stamp();
                self.stats.splits += 1;
                self.replay();
                self.sh.library_layout_lost(0);
                self.sh.library_layout_lost(1);
                self.seg_work = false;
                self.work_since_barrier = false;
            }
            // Device lost or out of memory: the list stays closed; the queue's submit reports it.
            Err(_) => { self.stats.refused += 1; trace::refused("split") },
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
            if sh.user[k].is_some() {
                // the title's arguments are console state: the library layout is written at the next draw
                continue;
            }
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
    // a new list starts recording: it holds a host allocator of the pool (a temporary holder until it has an address)
    static TEMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX / 2);
    let temp = TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let host_allocator = a.take(temp)?;
    let host_list = match a.backend.list(kind, &host_allocator) {
        Ok(l) => l,
        Err(e) => {
            a.release(temp);
            return Err(e);
        }
    };
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
            spans: [Span::default(); SPANS],
            stamps: a.backend.stamps().cloned(),
            stamp_labels: Vec::new(),
        },
    });
    let p: Raw = Box::into_raw(obj).cast();
    a.rehold(temp, &*enter(p) as *const _ as usize);
    enter(p).open_stamp();
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

/// After commands recorded into the list's current host list from outside the translator (the
/// FidelityFX upscale, `crates/ffx`, binds its own descriptor heaps, root signature and pipeline):
/// forget what is bound, so the title's next sets are applied instead of dropped as repeats. The
/// title's descriptor heaps are bound again at once (its tables keep mapping through them); its
/// pipeline counts as unbound until it sets one, so a draw before that is dropped rather than run with
/// the outside pipeline. `false` if `list` is not one of ours.
pub fn forget_bound_state(list: Raw) -> bool {
    let Some(s) = state(list) else { return false };
    let (count, console, host) = (s.sh.heap_count, s.sh.heaps_console, s.sh.heaps_host);
    s.sh.clear();
    if count > 0 && s.open {
        call!(s.SetDescriptorHeaps(count, host.as_ptr()));
        s.sh.heap_count = count;
        s.sh.heaps_console = console;
        s.sh.heaps_host = host;
    }
    s.sh.pso_missing = true;
    true
}

/// Before `list` is submitted: the host mirror of every descriptor table its translated programs read is
/// brought up to date with the title's bytes ([`ListHost::sync_descriptors`]). Returns the slots looked at; 0
/// for a list that is not ours or read no tables.
pub fn sync_descriptors(list: Raw) -> u32 {
    let Some(s) = state(list) else { return 0 };
    s.spans.iter().filter(|p| p.hi > p.lo).map(|p| s.host.sync_descriptors(p.lo, p.hi - p.lo)).sum()
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
    if s.open {
        let _ = s.close_current();
    }
    let me = &*s as *const _ as usize;
    let host_allocator = match a.take(me) {
        Ok(h) => h,
        Err(e) => return e.code(),
    };
    while s.segments.len() > 1 {
        let seg = s.segments.pop().expect("more than one");
        s.spare.push(seg.list);
    }
    s.segments[0].waits.clear();
    if let Err(e) = unsafe { s.segments[0].list.list.Reset(&host_allocator, None) } {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
            let akind = unsafe { (*allocator.cast::<super::alloc::AllocatorObject>()).kind };
            eprintln!("xlist     list Reset refused by the host ({:#x}): list kind {}, allocator kind {akind}, allocator {allocator:?}", e.code().0, s.kind);
        }
        return e.code();
    }
    s.allocator = allocator;
    s.point_at_current();
    s.open = true;
    s.open_stamp();
    s.seg_work = false;
    s.work_since_barrier = false;
    s.sh.clear();
    s.heaps = [None; 2];
    s.stats = ListStats::default();
    s.spans = [Span::default(); SPANS];
    if !pipeline.is_null() {
        s.set_pipeline(pipeline);
    }
    S_OK
}

/// 0x458 ResetX `(allocator, pipeline, flags)`: the flags tune the console's command memory.
unsafe extern "system" fn reset_x(this: Raw, allocator: Raw, pipeline: Raw, _flags: u32) -> HRESULT {
    unsafe { reset(this, allocator, pipeline) }
}

// ---- GPU safe mode ----

/// `REBUILD_GPU_SKIP`: kinds of the title's GPU work to drop, for finding work that hangs the GPU:
/// `draw`, `dispatch`, `indirect`, `mesh`, `rect` (comma-separated; `submit` is the queue's). Read once.
pub mod skip {
    pub const DRAW: u32 = 1;
    pub const DISPATCH: u32 = 2;
    pub const INDIRECT: u32 = 4;
    pub const MESH: u32 = 8;
    pub const SUBMIT: u32 = 16;
    /// rectangle-list draws expanded to triangles ([`super::super::rect`]): dropped instead
    pub const RECT: u32 = 32;
    /// The largest `ExecuteIndirect` max count let through (a garbage count must not become billions of draws).
    pub const MAX_INDIRECT: u32 = 1 << 16;

    pub fn mask() -> u32 {
        static M: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *M.get_or_init(|| {
            let v = std::env::var("REBUILD_GPU_SKIP").unwrap_or_default().to_ascii_lowercase();
            let mut m = 0;
            for t in v.split(',').map(str::trim) {
                m |= match t {
                    "draw" => DRAW,
                    "dispatch" => DISPATCH,
                    "indirect" => INDIRECT,
                    "mesh" => MESH,
                    "submit" => SUBMIT,
                    "rect" => RECT,
                    "all" => DRAW | DISPATCH | INDIRECT | MESH | SUBMIT,
                    _ => 0,
                };
            }
            if m != 0 {
                eprintln!("gpusafe   skipping {v}");
            }
            m
        })
    }

    #[inline]
    pub fn on(kind: u32) -> bool {
        mask() & kind != 0
    }
}

// ---- draws, dispatches, copies ----

unsafe extern "system" fn draw_instanced(this: Raw, vertices: u32, instances: u32, first_vertex: u32, first_instance: u32) {
    let s = enter(this);
    // a rectangle list with no geometry stage bound: expanded on the CPU when its vertices come from buffers
    if s.sh.topology == Some(super::rect::RECTLIST) && !s.sh.pso_missing && !s.sh.pso_rects && s.sh.pso_topology == 3 && !skip::on(skip::DRAW | skip::RECT) {
        if s.rect_draw(vertices, instances, first_vertex, first_instance) {
            return;
        }
        RECTS_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    tr!("draw ({vertices}, {instances}, {first_vertex}, {first_instance}) vb0 {:x?}", s.sh.vb_console[0]);
    if trace::on() && s.sh.vb_console[0].0 != 0 && s.va(s.sh.vb_console[0].0).is_some() {
        let (va, _, stride) = s.sh.vb_console[0];
        let at = va + u64::from(first_vertex) * u64::from(stride);
        // SAFETY: memory a range holds; console addresses are CPU addresses
        let w: Vec<f32> = (0..(stride as usize / 4) * 3.min(vertices as usize)).map(|i| unsafe { *(at as *const f32).add(i) }).collect();
        tr!("vb0 first vertices as f32: {w:?}");
    }
    if s.sh.pso_missing || skip::on(skip::DRAW) || s.topology_mismatch() {
        s.skip_draw();
        return;
    }
    if !s.bind_library(0) {
        s.stats.skipped += 1;
        return;
    }
    s.work();
    s.stats.draws += 1;
    tr!("draw {vertices} vertices x{instances}");
    call!(s.DrawInstanced(vertices, instances, first_vertex, first_instance));
}

unsafe extern "system" fn draw_indexed(this: Raw, indices: u32, instances: u32, first_index: u32, base_vertex: i32, first_instance: u32) {
    let s = enter(this);
    tr!("draw indexed ({indices}, {instances}, {first_index}, {base_vertex}, {first_instance})");
    if s.sh.pso_missing || skip::on(skip::DRAW) || s.topology_mismatch() {
        s.skip_draw();
        return;
    }
    if !s.bind_library(0) {
        s.stats.skipped += 1;
        return;
    }
    s.work();
    s.stats.draws += 1;
    tr!("draw indexed {indices} indices x{instances}");
    call!(s.DrawIndexedInstanced(indices, instances, first_index, base_vertex, first_instance));
}

unsafe extern "system" fn dispatch(this: Raw, x: u32, y: u32, z: u32) {
    let s = enter(this);
    // more than 65,535 groups in a dimension is invalid in D3D12: never sent
    if s.sh.pso_missing || skip::on(skip::DISPATCH) || x > 65535 || y > 65535 || z > 65535 {
        s.stats.skipped += 1;
        s.stats.skipped_pipeline += u32::from(s.sh.pso_missing);
        return;
    }
    if !s.bind_library(1) {
        s.stats.skipped += 1;
        return;
    }
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
    if s.sh.pso_missing || skip::on(skip::MESH) || x > 65535 || y > 65535 || z > 65535 {
        s.stats.skipped += 1;
        s.stats.skipped_pipeline += u32::from(s.sh.pso_missing);
        return;
    }
    if !s.bind_library(0) {
        s.stats.skipped += 1;
        return;
    }
    s.work();
    s.stats.dispatches += 1;
    match &s.current().l6 {
        Some(l6) => unsafe { l6.DispatchMesh(x, y, z) },
        None => { s.stats.refused += 1; trace::refused("dispatch_mesh") },
    }
}

/// 0x1e8 ExecuteIndirect `(signature, max count, args, args offset, count buffer, count offset)`.
unsafe extern "system" fn execute_indirect(this: Raw, signature: Raw, max: u32, args: Raw, args_offset: u64, count: Raw, count_offset: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let s = enter(this);
    INDIRECT[indirect::CALLS].fetch_add(1, Relaxed);
    if s.sh.pso_missing || skip::on(skip::INDIRECT) {
        s.stats.skipped += 1;
        INDIRECT[indirect::NO_PIPELINE].fetch_add(1, Relaxed);
        s.stats.skipped_pipeline += u32::from(s.sh.pso_missing);
        return;
    }
    let Some(host) = s.host.command_signature(signature) else {
        s.stats.refused += 1;
        // which pipeline side such a signature runs with (a console-only argument type: dispatch or mesh?)
        INDIRECT_REFUSED[s.sh.pso_side.min(1)].fetch_add(1, Relaxed);
        return;
    };
    let shape = host.shape;
    INDIRECT[usize::from(shape.work.min(5))].fetch_add(1, Relaxed);
    // the side its work runs on must be the bound pipeline's (one pipeline state on the host)
    let Some(k) = shape.side().filter(|&k| k == s.sh.pso_side) else {
        s.stats.refused += 1;
        INDIRECT[indirect::SIDE].fetch_add(1, Relaxed);
        return;
    };
    if k == 0 && s.topology_mismatch() {
        s.stats.skipped += 1;
        return;
    }
    // console GPU addresses in the arguments: the host would read them as its own
    if shape.buffer_views {
        { s.stats.refused += 1; trace::refused("execute_indirect") };
        INDIRECT[indirect::BUFFER_VIEWS].fetch_add(1, Relaxed);
        return;
    }
    // a signature that changes root parameters runs only under the root they belong to
    let mut library_stages = 0;
    let sig = if !shape.changes_root() {
        host.signature
    } else if let Some(map) = s.sh.user[k] {
        // the library root is bound: the arguments rewritten into its user-data constants
        match s.host.library_signature(signature, s.sh.root[k], map) {
            Some(l) => {
                INDIRECT[indirect::MAPPED].fetch_add(1, Relaxed);
                library_stages = l.stages;
                l.signature
            }
            None => {
                s.stats.refused += 1;
                INDIRECT[indirect::UNMAPPABLE].fetch_add(1, Relaxed);
                return;
            }
        }
    } else if shape.root_views || host.root.is_null() || host.root != s.sh.root[k] {
        // the title's layout, but a root view is a console address, or another root is bound
        s.stats.refused += 1;
        INDIRECT[indirect::WRONG_ROOT].fetch_add(1, Relaxed);
        return;
    } else {
        host.signature
    };
    if !s.bind_library(k) {
        s.stats.skipped += 1;
        return;
    }
    let Some(a) = s.resource(args) else {
        { s.stats.refused += 1; trace::refused("execute_indirect") };
        return;
    };
    // the count buffer is optional (null: exactly `max`); one we cannot map is refused, never guessed
    let c = if count.is_null() {
        std::ptr::null_mut()
    } else {
        match s.resource(count) {
            Some(c) => c,
            None => {
                { s.stats.refused += 1; trace::refused("execute_indirect") };
                return;
            }
        }
    };
    s.work();
    s.stats.draws += 1;
    call!(s.ExecuteIndirect(sig, max.min(skip::MAX_INDIRECT), a, args_offset, c, count_offset));
    // the commands left their own words in the stages they wrote: the next direct draw writes the title's again
    for st in 0..2 {
        if library_stages >> st & 1 != 0 {
            s.sh.user_valid[k][st] = false;
            s.sh.user_dirty[k] = true;
        }
    }
}

unsafe extern "system" fn copy_buffer_region(this: Raw, dst: Raw, dst_offset: u64, src: Raw, src_offset: u64, bytes: u64) {
    let s = enter(this);
    s.work();
    s.stats.copies += 1;
    let (Some(d), Some(r)) = (s.resource(dst), s.resource(src)) else { return };
    call!(s.CopyBufferRegion(d, dst_offset, r, src_offset, bytes));
}

unsafe extern "system" fn copy_resource(this: Raw, dst: Raw, src: Raw) {
    let s = enter(this);
    s.work();
    s.stats.copies += 1;
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
    s.stats.copies += 1;
    let Some((d, d_off)) = s.place(dst, size) else {
        { s.stats.refused += 1; trace::refused("copy_memory") };
        return;
    };
    let Some((r, r_off)) = s.place(src, size) else {
        { s.stats.refused += 1; trace::refused("copy_memory") };
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

/// ExecuteIndirect calls with a host signature: 0-5 by its work argument ([`super::IndirectShape`] `WORK_*`), then
/// those mapped into the library layout and those refused, at the indices in [`indirect`].
pub static INDIRECT: [std::sync::atomic::AtomicU64; indirect::COUNTERS] = [const { std::sync::atomic::AtomicU64::new(0) }; indirect::COUNTERS];

/// ExecuteIndirect calls refused for want of a host signature, by the bound pipeline's side (graphics, compute).
pub static INDIRECT_REFUSED: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// Rectangle-list draws expanded to triangles on the CPU, and those that could not be (dropped).
pub static RECTS_EXPANDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static RECTS_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// GPU timestamp labels written into the title's memory (resolved at their host list's end).
pub static STAMP_LABELS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Draws and dispatches dropped: a library pipeline with no shadow table bound.
pub static NO_SHADOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Host lists whose Close the host refused (never executed).
pub static CLOSE_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The PSO topology type of a `D3D_PRIMITIVE_TOPOLOGY` (0 when undefined).
fn topology_type(t: i32) -> u8 {
    match t {
        1 => 1,
        2 | 3 | 10 | 11 => 2,
        4 | 5 | 12 | 13 => 3,
        33..=64 => 4,
        _ => 0,
    }
}

unsafe extern "system" fn topology(this: Raw, t: i32) {
    let s = enter(this);
    if s.sh.topology == Some(t) {
        s.stats.elided += 1;
        return;
    }
    s.sh.topology = Some(t);
    tr!("topology {t}");
    // the host has no rectangle list: its draws go out as triangles ([`super::rect`])
    let host = if t == super::rect::RECTLIST { 4 } else { t };
    call!(s.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host)));
}

unsafe extern "system" fn viewports(this: Raw, n: u32, v: *const D3D12_VIEWPORT) {
    let s = enter(this);
    let n = n.min(16);
    if !v.is_null() {
        unsafe { s.sh.viewports.as_mut_ptr().copy_from_nonoverlapping(v, n as usize) };
        s.sh.viewport_count = n;
        tr!("viewports {:?}", s.sh.viewports[..n as usize].iter().map(|v| (v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth)).collect::<Vec<_>>());
    }
    call!(s.RSSetViewports(n, v));
}

unsafe extern "system" fn scissors(this: Raw, n: u32, r: *const RECT) {
    let s = enter(this);
    let n = n.min(16);
    if !r.is_null() {
        unsafe { s.sh.scissors.as_mut_ptr().copy_from_nonoverlapping(r, n as usize) };
        s.sh.scissor_count = n;
        tr!("scissors {:?}", s.sh.scissors[..n as usize].iter().map(|r| (r.left, r.top, r.right, r.bottom)).collect::<Vec<_>>());
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
        None => { s.stats.refused += 1; trace::refused("depth_bounds") },
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
                { s.stats.refused += 1; trace::refused("resource_barrier") };
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
                                host_layouts(&mut t);
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
                                whole_buffer(&mut b);
                                buffers.push(b);
                            }
                            None => globals.push(b[..16].try_into().expect("16 bytes")),
                        }
                    }
                    _ => { s.stats.refused += 1; trace::refused("barrier_groups") },
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

/// A buffer barrier over its whole buffer (`Offset` at +24 = 0, `Size` at +32 = `UINT64_MAX`). The
/// Enhanced Barriers spec: "For now, Offset must be zero and Size must be either the buffer size in
/// bytes or UINT64_MAX" (microsoft.github.io/DirectX-Specs/d3d/D3D12EnhancedBarriers.html). A title
/// range is legal on the console but not here, and our host buffer need not be the console's size;
/// the whole buffer is always at least as strong.
fn whole_buffer(b: &mut [u8; 40]) {
    b[24..32].copy_from_slice(&0u64.to_le_bytes());
    b[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
}

/// The console's barrier layouts that a host runtime may not know, as host layouts (`LayoutBefore` at +16,
/// `LayoutAfter` at +20 of a texture barrier). 31 is `DIRECT_QUEUE_GENERIC_READ_COMPUTE_QUEUE_ACCESSIBLE`
/// (Agility SDK 1.611): a host without it rejects the barrier and the whole list errors, so its `Close` and the
/// title's next `Reset` fail (the title asserts). `GENERIC_READ` (1) is the same promise on every queue type.
fn host_layouts(t: &mut [u8; 64]) {
    const GENERIC_READ: u32 = 1;
    const NEWEST_HOST: u32 = 30; // D3D12_BARRIER_LAYOUT_VIDEO_QUEUE_COMMON
    for at in [16usize, 20] {
        let v = u32::from_le_bytes(t[at..at + 4].try_into().expect("4 bytes"));
        if v == 31 {
            t[at..at + 4].copy_from_slice(&GENERIC_READ.to_le_bytes());
        } else if v != u32::MAX && v > NEWEST_HOST {
            // a layout newer still: COMMON keeps it legal (and correct where the resource is only read)
            t[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        }
    }
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
            None => { s.stats.refused += 1; trace::refused("descriptor_heaps") },
        }
    }
    s.sh.heap_count = count as u32;
    // table offsets and the shadow table follow the heaps
    s.sh.user_dirty = [true; 2];
    call!(s.SetDescriptorHeaps(count as u32, s.sh.heaps_host.as_ptr()));
}

/// 0x108 / 0x110 Set{Compute,Graphics}RootSignature. While a translated pipeline is bound on that side, the
/// host keeps the program library's root signature: the title's only ends its arguments' lifetime.
unsafe extern "system" fn root_signature<const K: usize>(this: Raw, root: Raw) {
    let s = enter(this);
    if s.sh.user[K].is_some() {
        if s.sh.title_root[K] == root {
            s.stats.elided += 1;
        } else {
            s.sh.title_root[K] = root;
            s.sh.title_args_cleared(K);
        }
        return;
    }
    s.sh.title_root[K] = root;
    match s.host.root_signature(root) {
        Some(r) => s.set_root(K, r),
        // the pipeline brings its own
        None => s.stats.elided += 1,
    }
}

impl ListState {
    /// A title root argument at `slot` of side `k` changed while a translated pipeline is bound there: kept as
    /// console state, and the library layout is written at the next draw or dispatch if the pipeline reads it.
    #[inline]
    fn library_arg(&mut self, k: usize, slot: u32) {
        if let Some(m) = self.sh.user[k] {
            if m.slots >> (slot & 63) & 1 != 0 {
                self.sh.user_dirty[k] = true;
            }
        }
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
    if s.sh.user[K].is_some() {
        s.sh.args[K][i] = Arg { kind: 1, console: handle, host: 0 };
        s.library_arg(K, slot);
        return;
    }
    let Some(h) = s.table(handle) else {
        { s.stats.refused += 1; trace::refused("root_table") };
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
    if s.sh.user[K].is_some() {
        s.sh.args[K][i] = Arg { kind: V, console: va, host: 0 };
        s.library_arg(K, slot);
        return;
    }
    let Some(h) = s.va(va) else {
        { s.stats.refused += 1; trace::refused("root_view") };
        tr!("root view kind {V} side {K} slot {slot}: address {va:#x} unknown");
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
    /// Keep the title's root constants; with a translated pipeline bound on side `k`, that is all (true: the
    /// caller is done).
    #[inline]
    fn keep_consts(&mut self, k: usize, slot: u32, values: &[u32], offset: u32) -> bool {
        let c = self.sh.consts[k].get_or_insert_with(|| Box::new(Consts { v: [[0; 64]; 64], mask: [0; 64] }));
        let i = slot as usize & 63;
        for (j, &v) in values.iter().enumerate() {
            let o = offset as usize + j;
            if o < 64 {
                c.v[i][o] = v;
                c.mask[i] |= 1 << o;
            }
        }
        if self.sh.user[k].is_some() {
            self.library_arg(k, slot);
            return true;
        }
        false
    }
}

unsafe extern "system" fn root_constant<const K: usize>(this: Raw, slot: u32, value: u32, offset: u32) {
    let s = enter(this);
    if s.keep_consts(K, slot, &[value], offset) {
        return;
    }
    if K == 0 {
        call!(s.SetGraphicsRoot32BitConstant(slot, value, offset));
    } else {
        call!(s.SetComputeRoot32BitConstant(slot, value, offset));
    }
}

unsafe extern "system" fn root_constants<const K: usize>(this: Raw, slot: u32, n: u32, data: *const u32, offset: u32) {
    let s = enter(this);
    if data.is_null() {
        { s.stats.refused += 1; trace::refused("root_constants") };
        return;
    }
    if s.keep_consts(K, slot, unsafe { std::slice::from_raw_parts(data, n.min(64) as usize) }, offset) {
        return;
    }
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
    let Some(va) = s.va(v.BufferLocation).filter(|&va| va != 0) else {
        // unmapped: unbind rather than leave the last index buffer bound for the next indexed draw
        { s.stats.refused += 1; trace::refused("index_buffer") };
        s.sh.ib_console = Some(key);
        s.sh.ib = D3D12_INDEX_BUFFER_VIEW::default();
        call!(s.IASetIndexBuffer(std::ptr::null()));
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
        // an address we cannot map is an empty slot (reads give 0), never address 0 with a size: a card faults on
        // that (a device removal), WARP only complains
        let view = match s.va(v.BufferLocation) {
            Some(va) if va != 0 => D3D12_VERTEX_BUFFER_VIEW { BufferLocation: va, ..v },
            _ => {
                if v.BufferLocation != 0 || v.SizeInBytes != 0 {
                    { s.stats.refused += 1; trace::refused("vertex_buffers") };
                    tr!("vertex buffer {slot} refused: {:#x} {} bytes stride {}", v.BufferLocation, v.SizeInBytes, v.StrideInBytes);
                }
                D3D12_VERTEX_BUFFER_VIEW::default()
            }
        };
        s.sh.vb_console[slot] = key;
        s.sh.vbs[slot] = view;
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
                { s.stats.refused += 1; trace::refused("render_targets") };
                s.stats.rt_unmapped += 1;
                0
            }
        };
    }
    s.stats.rt_sets += 1;
    tr!("render targets {n}: {:x?} depth {:?}", &s.sh.rts[..n as usize].iter().map(|h| h.ptr).collect::<Vec<_>>(), s.sh.dsv.map(|d| d.ptr as *const u8));
    s.sh.rt_count = n;
    s.sh.dsv = if dsv.is_null() {
        None
    } else {
        match s.host.cpu_descriptor(unsafe { (*dsv).ptr }) {
            Some(h) => Some(D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h }),
            None => {
                { s.stats.refused += 1; trace::refused("render_targets") };
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
    s.stats.clears += 1;
    match s.host.cpu_descriptor(dsv.ptr) {
        Some(h) => call!(s.ClearDepthStencilView(D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h }, flags, depth, stencil, n, rects)),
        None => { s.stats.refused += 1; trace::refused("clear_dsv") },
    }
}

/// `REBUILD_PROBE_CLEAR=r,g,b[,a]`: every render-target clear the title records clears to this colour instead (a
/// probe: a capture showing it proves the present reads what the title's lists wrote, after they ran).
fn probe_clear() -> &'static Option<[f32; 4]> {
    static C: std::sync::OnceLock<Option<[f32; 4]>> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let v = std::env::var("REBUILD_PROBE_CLEAR").ok()?;
        let f: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        (f.len() >= 3).then(|| [f[0], f[1], f[2], f.get(3).copied().unwrap_or(1.0)])
    })
}

unsafe extern "system" fn clear_rtv(this: Raw, rtv: D3D12_CPU_DESCRIPTOR_HANDLE, color: *const f32, n: u32, rects: *const RECT) {
    let s = enter(this);
    s.work();
    s.stats.clears += 1;
    tr!("clear rtv {:#x} -> {:?} colour {:?} rects {n}", rtv.ptr, s.host.cpu_descriptor(rtv.ptr).map(|h| h as *const u8), (!color.is_null()).then(|| unsafe { *color.cast::<[f32; 4]>() }));
    let color = probe_clear().as_ref().map_or(color, |c| c.as_ptr());
    match s.host.cpu_descriptor(rtv.ptr) {
        Some(h) => call!(s.ClearRenderTargetView(D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h }, color, n, rects)),
        None => { s.stats.refused += 1; trace::refused("clear_rtv") },
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
            { s.stats.refused += 1; trace::refused("uav_clear") };
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
    if value == LabelValue::GpuTimestamp && s.stamp_label(address) {
        return;
    }
    // the CPU's copy: the title polls it (not the card's copy of memory the GPU reads)
    let host_va = s.host.cpu_memory(address).and_then(|r| r.map(address)).filter(|_| address != 0);
    let w = LabelWrite { address, value, bytes, flush, flags };
    let done = match &s.current().l2 {
        Some(l2) => s.host.label(l2, &w, host_va),
        None => false,
    };
    if !done {
        { s.stats.refused += 1; trace::refused("label") };
        tr!("label refused: address {address:#x} (host {host_va:x?}) {bytes} bytes, value {value:?}, flush {flush:#x}, flags {flags:#x}, host list has l2 {}", s.current().l2.is_some());
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
        WaitAction::Drop => { s.stats.refused += 1; trace::refused("memory_wait") },
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

#[cfg(test)]
mod layout_tests {
    #[test]
    fn newer_console_layouts_become_host_ones() {
        let mut t = [0u8; 64];
        t[16..20].copy_from_slice(&31u32.to_le_bytes());
        t[20..24].copy_from_slice(&6u32.to_le_bytes());
        super::host_layouts(&mut t);
        assert_eq!(u32::from_le_bytes(t[16..20].try_into().unwrap()), 1, "31 -> GENERIC_READ");
        assert_eq!(u32::from_le_bytes(t[20..24].try_into().unwrap()), 6, "known layouts stay");
        t[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        super::host_layouts(&mut t);
        assert_eq!(u32::from_le_bytes(t[20..24].try_into().unwrap()), u32::MAX, "UNDEFINED stays");
    }

    #[test]
    fn buffer_barriers_cover_the_whole_buffer() {
        let mut b = [0xabu8; 40];
        b[24..32].copy_from_slice(&0x100u64.to_le_bytes());
        b[32..40].copy_from_slice(&0x40u64.to_le_bytes());
        super::whole_buffer(&mut b);
        assert_eq!(&b[..24], &[0xab; 24], "sync, access and resource untouched");
        assert_eq!(u64::from_le_bytes(b[24..32].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), u64::MAX);
    }
}
