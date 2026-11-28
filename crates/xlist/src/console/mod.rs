//! The console's D3D12.X command list and command allocator, as objects title code calls.
//!
//! The device (the lead's, `crates/runtime`) hands these out from its slots 0x48
//! (CreateCommandAllocator), 0x60 (CreateCommandList) and 0x248 (CreateCommandListX):
//!
//! ```text
//! create_allocator(kind, &host_device, host)  -> *mut c_void   the title-visible allocator
//! create_list(kind, allocator, pipeline)      -> *mut c_void   the title-visible list, open
//! segments(list) / host_list(list)                             what ExecuteCommandLists submits
//! ```
//!
//! **Memory as the title reads it.** A list is one allocation: the first [`TITLE_BYTES`] (0x960)
//! bytes are the console layout (`agents/d3d12x-object-layouts.csv`): the vtable at +0x00, the
//! list type at +0x10, the put pointer and the four packet limits at +0x18..+0x40 **all zero**, so
//! the title's inline packet writer always falls back to a call, and the title's own fields (root
//! headers, the pipeline shadow at +0x544) are left to it. Our recording state starts at +0x960.
//!
//! **Every call is translated as it arrives**, on the thread that records (plan decision 3), into a
//! host `ID3D12GraphicsCommandList`: console objects become host objects through [`ListHost`] (the
//! lead's resources, heaps and pipelines), console addresses become host GPU addresses through
//! [`MemoryRange`]s (one cached range, so the common case is a subtract and a compare), and a set
//! that repeats what is bound is dropped, as in [`crate::translate`]. Arguments that point into the
//! title's stack (barrier arrays, viewports) are consumed during the call, so nothing is copied.
//!
//! **Every slot of the 160 exists.** Slots we implement call into the translator; every other
//! slot is a trap that counts calls, records the first call's time and tells
//! [`ListHost::first_trap`] once, then fails with the slot's own return kind: `E_NOTIMPL` for a
//! method that returns an `HRESULT`, 0 (null, false) for one that returns a value or nothing.
//! [`list_slots`] and [`allocator_slots`] say which is which.
//!
//! **A list waiting for a memory value is cut there** (slots 0x248/0x250): the host list so far is
//! closed and a new one opened, and the wait is attached to the new [`Segment`]. The queue then
//! makes it a GPU-side wait between the two parts (the fork held whole submissions on the CPU and
//! lost 2.4-2.9 s a frame; `docs/research/d3d12x-device.md` §5.6).

#![cfg(windows)]

mod alloc;
pub mod indirect;
mod list;
pub mod pages;
pub mod rect;
mod slots;
pub mod stamps;
pub mod twin;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::ffi::c_void;
use std::sync::Arc;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

pub use alloc::{create_allocator, host_allocator, upload_range};
pub use list::{create_list, TWIN_COPIES, CLOSE_FAILED, INDIRECT, INDIRECT_REFUSED, NO_HEAP_TABLES, NO_SHADOW, RECTS_EXPANDED, RECTS_DROPPED, STAMP_LABELS, forget_bound_state, host_list, segments, skip, stats, sync_descriptors, trace, HostList, ListStats, Segment};
pub use twin::UavTwin;
pub use slots::{allocator_slots, list_slots, traps, Returns, SlotInfo, SlotKind, Trap, ALLOCATOR_SLOTS, LIST_SLOTS};

/// A COM or console object pointer, borrowed: never released through this.
pub type Raw = *mut c_void;

/// Bytes of a list that belong to the console layout; our state starts here.
pub const TITLE_BYTES: usize = 0x960;

/// The console's RTV and DSV descriptor increment (`GetDescriptorHandleIncrementSize` at device
/// 0x78), used to step through a single-range `OMSetRenderTargets`.
pub const CONSOLE_RTV_INCREMENT: usize = 128;

/// The host objects for a console pipeline (the pointer the lead's device returned from 0x50, 0x58,
/// 0x1f0 or 0x2b0).
#[derive(Clone, Copy, Debug)]
pub struct HostPipeline {
    /// `ID3D12PipelineState`, borrowed.
    pub pso: Raw,
    /// The root signature it was built with (`ID3D12RootSignature`), or null to keep whatever the
    /// title set through 0x108/0x110.
    pub root_signature: Raw,
    /// Compute pipeline (root arguments go to the compute root).
    pub compute: bool,
    /// How its programs' user data is made from the title's root arguments (`docs/design/root-seam.md`).
    /// `Some`: `root_signature` is the program library's root, and the title's root arguments are kept as
    /// console state and written in the library's layout at each draw or dispatch that follows a change.
    /// `None`: the title's root arguments go to the host list by the title's parameter numbers.
    pub user: Option<&'static UserMap>,
    /// The PSO's primitive topology type (1 point, 2 line, 3 triangle, 4 patch), or 0 when not known (no check):
    /// a draw whose topology is outside it is refused by the host (debug #611, the list's Close fails), so dropped.
    pub topology: u8,
    /// A rectangle-list pipeline built with the geometry stage that completes each rectangle (`rebuild_pipelines::
    /// rectgs`): its draws of any kind (direct, indexed, indirect) go out as triangle lists with the title's counts,
    /// three vertices a rectangle. `false` for a rectangle-list pipeline without one: the list expands its
    /// non-indexed draws from vertex buffers on the CPU ([`rect`]) and drops the others.
    pub rects: bool,
}

/// What a command signature's arguments do, read from its description ([`IndirectShape::of`]). The list runs a
/// signature only under a root it fits: one that changes root arguments names the title's root parameters,
/// while a translated pipeline is bound with the program library's root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndirectShape {
    /// The work argument (the last): one of the `WORK_*` values.
    pub work: u8,
    /// Some argument is `CONSTANT` (root constants by the title's parameter numbers).
    pub constants: bool,
    /// Some argument is a root `CONSTANT_BUFFER_VIEW`, `SHADER_RESOURCE_VIEW` or `UNORDERED_ACCESS_VIEW` (a
    /// console GPU address in the argument buffer).
    pub root_views: bool,
    /// Some argument is a `VERTEX_BUFFER_VIEW` or `INDEX_BUFFER_VIEW` (console GPU addresses in the argument
    /// buffer, which the host would read as its own).
    pub buffer_views: bool,
    /// Some argument is of a type the host does not know (a console-only one).
    pub other: bool,
}

impl IndirectShape {
    pub const WORK_NONE: u8 = 0;
    pub const WORK_DRAW: u8 = 1;
    pub const WORK_DRAW_INDEXED: u8 = 2;
    pub const WORK_DISPATCH: u8 = 3;
    pub const WORK_MESH: u8 = 4;
    /// Rays, two work arguments, or one that is not last.
    pub const WORK_OTHER: u8 = 5;

    pub fn of(args: &[D3D12_INDIRECT_ARGUMENT_DESC]) -> IndirectShape {
        let mut s = IndirectShape::default();
        for (i, a) in args.iter().enumerate() {
            let work = match a.Type {
                D3D12_INDIRECT_ARGUMENT_TYPE_DRAW => IndirectShape::WORK_DRAW,
                D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED => IndirectShape::WORK_DRAW_INDEXED,
                D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH => IndirectShape::WORK_DISPATCH,
                D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH => IndirectShape::WORK_MESH,
                D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_RAYS => IndirectShape::WORK_OTHER,
                D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT => {
                    s.constants = true;
                    0
                }
                D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW
                | D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW
                | D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW => {
                    s.root_views = true;
                    0
                }
                D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW | D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW => {
                    s.buffer_views = true;
                    0
                }
                _ => {
                    s.other = true;
                    0
                }
            };
            if work != 0 {
                s.work = if i + 1 == args.len() && s.work == 0 { work } else { IndirectShape::WORK_OTHER };
            }
        }
        s
    }

    /// The root side the work runs on (0 graphics, 1 compute), or `None` for no or unknown work.
    pub fn side(&self) -> Option<usize> {
        match self.work {
            IndirectShape::WORK_DRAW | IndirectShape::WORK_DRAW_INDEXED | IndirectShape::WORK_MESH => Some(0),
            IndirectShape::WORK_DISPATCH => Some(1),
            _ => None,
        }
    }

    /// Some argument changes a root parameter (the host signature was made against the title's root).
    pub fn changes_root(&self) -> bool {
        self.constants || self.root_views
    }
}

/// A console command signature (device 0x148) on the host.
#[derive(Clone, Copy, Debug)]
pub struct HostSignature {
    /// `ID3D12CommandSignature`, borrowed.
    pub signature: Raw,
    /// The host root signature it was made against (`ID3D12RootSignature`, borrowed): the title's, when an
    /// argument changes root parameters; null when none does (then it runs under any root).
    pub root: Raw,
    pub shape: IndirectShape,
}

/// A console command signature rewritten into the program library's layout ([`ListHost::library_signature`]).
#[derive(Clone, Copy, Debug)]
pub struct LibrarySignature {
    /// `ID3D12CommandSignature`, borrowed (lives as long as the console signature).
    pub signature: Raw,
    /// Bit per stage (0 VS or CS, 1 PS) whose user-data constants its commands write.
    pub stages: u8,
}

/// User-data dwords a translated program's root constants carry (`rebuild_pipelines::layout::USER_DATA_DWORDS`).
pub const USER_DWORDS: usize = 16;

/// Where one user-data dword of a translated program comes from: one of the title's root arguments, as the
/// console's hardware would have loaded it into the program's user SGPRs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UserSource {
    /// Nothing the title sets: 0 (a dynamic-resource heap base, an immediate sampler, a dword past a parameter).
    #[default]
    Zero,
    /// Root constant `dword` of the title's root parameter `slot`.
    Constant { slot: u8, dword: u8 },
    /// The descriptor table at `slot`: the byte offset of its first descriptor from the start of the bound heap
    /// that holds it (console bytes: 32 a view, 16 a sampler). The program divides by the console's size to
    /// get the heap index it reads with `ResourceDescriptorHeap[]` / `SamplerDescriptorHeap[]`; `bytes` of the
    /// table are what its descriptors span, synced before submission.
    Table { slot: u8, bytes: u32 },
    /// The low or high half of the console GPU address of the root CBV, SRV or UAV at `slot` (console
    /// addresses are the translated program's window addresses, so the address goes in as it is).
    AddressLo { slot: u8 },
    AddressHi { slot: u8 },
}

impl UserSource {
    /// The title's root parameter it reads, if any.
    pub fn slot(&self) -> Option<u8> {
        match *self {
            UserSource::Zero => None,
            UserSource::Constant { slot, .. } | UserSource::Table { slot, .. } | UserSource::AddressLo { slot } | UserSource::AddressHi { slot } => Some(slot),
        }
    }
}

/// One stage's user data: the library root parameter holding it and the source of each dword.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StageUserData {
    /// The library root signature's root-constant parameter (`rebuild_pipelines::layout::param`).
    pub param: u32,
    /// Dwords to write (the highest the program reads, plus one); 0 writes nothing.
    pub dwords: u32,
    pub src: [UserSource; USER_DWORDS],
}

/// A translated pipeline's user data, per stage: compute has one (its CS), graphics two (VS, then PS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct UserMap {
    pub stages: [StageUserData; 2],
    pub stage_count: u32,
    /// The library root signature's root SRV for the descriptor shadow table (`t0, space101`).
    pub shadow_param: u32,
    /// Bit `s`: some stage reads the title's root parameter `s` (a change to any other is not a change).
    pub slots: u64,
}

impl UserMap {
    /// A map with `stages` (one for compute, two for graphics) and the library parameters of each.
    pub fn new(stages: &[StageUserData], shadow_param: u32) -> UserMap {
        let mut m = UserMap { shadow_param, stage_count: stages.len().min(2) as u32, ..Default::default() };
        for (i, s) in stages.iter().take(2).enumerate() {
            m.stages[i] = *s;
            for src in &s.src[..(s.dwords as usize).min(USER_DWORDS)] {
                if let Some(slot) = src.slot() {
                    m.slots |= 1 << (slot & 63);
                }
            }
        }
        m
    }
}

/// What a console pipeline is on the host. The lead's device implements it; the list asks once
/// per *change* of pipeline (a set that repeats the bound pipeline is dropped before the call).
pub trait PipelineSource {
    fn pipeline(&self, console: Raw) -> Option<HostPipeline>;
    /// A graphics pipeline's input layout, as (slot, byte offset, DXGI format, per instance): asked only for a
    /// draw the list must rewrite (a rectangle list, [`rect`]). `None`: not known, and such a draw is dropped.
    fn input_layout(&self, _console: Raw) -> Option<Vec<(u32, u32, u32, bool)>> {
        None
    }
}

/// A console descriptor heap as the host sees it: console GPU handles in
/// `console_base..console_base + console_bytes` at `console_stride` bytes a descriptor become
/// `host_base + index * host_stride` in the shader-visible host heap `heap`.
#[derive(Clone, Copy, Debug)]
pub struct HeapBinding {
    /// `ID3D12DescriptorHeap`, borrowed.
    pub heap: Raw,
    pub console_base: u64,
    pub console_bytes: u64,
    /// 32 for views, 16 for samplers (device 0x78); a power of two.
    pub console_stride: u32,
    pub host_base: u64,
    pub host_stride: u32,
    /// GPU address of the heap's descriptor shadow table (16 bytes per host index, bound at
    /// `t0, space101` for translated programs); 0 for a heap without one (samplers).
    pub shadow_va: u64,
    /// Console indices to host ones ([`pages`]); `None`: index for index.
    pub pages: Option<&'static pages::PageMap>,
}

impl HeapBinding {
    /// The host index of console index `i` (a page with no host page yet is given one).
    #[inline]
    fn host_index(&self, i: u32) -> Option<u32> {
        match self.pages {
            Some(p) => p.host_or_map(i),
            None => Some(i),
        }
    }

    #[inline]
    fn map(&self, handle: u64) -> Option<u64> {
        let off = handle.wrapping_sub(self.console_base);
        if off >= self.console_bytes {
            return None;
        }
        let i = self.host_index((off >> self.console_stride.trailing_zeros()) as u32)?;
        Some(self.host_base + u64::from(i) * u64::from(self.host_stride))
    }

    /// The user-data dword of a descriptor table at byte `off` of the console heap, `bytes` long: the byte offset
    /// of its first descriptor in the host heap, in console sizes (translated shaders add console byte offsets to
    /// it and divide by the console stride). A table no host page can hold reads the null bank.
    #[inline]
    pub fn table_offset(&self, off: u64, bytes: u32) -> u32 {
        let sh = self.console_stride.trailing_zeros();
        match self.pages {
            None => off as u32,
            Some(p) => {
                let i = (off >> sh) as u32;
                let n = bytes.div_ceil(self.console_stride.max(1));
                (p.table(i, n) << sh) | (off as u32 & (self.console_stride - 1))
            }
        }
    }
}

impl std::fmt::Debug for pages::PageMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PageMap({} of {} pages of {})", self.taken(), self.host_pages, 1u32 << self.shift)
    }
}

/// Console GPU memory `console..console + bytes` is host GPU memory at `host_va`, inside the host
/// buffer `resource` at `resource_offset` (copies name the resource, root views the address).
#[derive(Clone, Copy, Debug)]
pub struct MemoryRange {
    pub console: u64,
    pub bytes: u64,
    pub host_va: u64,
    /// `ID3D12Resource` (a buffer), borrowed; may be null if nothing copies from this range.
    pub resource: Raw,
    pub resource_offset: u64,
}

impl MemoryRange {
    pub const EMPTY: MemoryRange = MemoryRange { console: 0, bytes: 0, host_va: 0, resource: std::ptr::null_mut(), resource_offset: 0 };

    #[inline]
    pub fn map(&self, va: u64) -> Option<u64> {
        let off = va.wrapping_sub(self.console);
        (off < self.bytes).then_some(self.host_va + off)
    }

    /// `len` bytes at `va` as (resource, offset), if they lie inside this range.
    #[inline]
    pub fn place(&self, va: u64, len: u64) -> Option<(Raw, u64)> {
        let off = va.wrapping_sub(self.console);
        (off < self.bytes && len <= self.bytes - off && !self.resource.is_null()).then_some((self.resource, self.resource_offset + off))
    }
}

/// What a bottom-of-pipe write (list 0x238 / 0x240) stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelValue {
    Value(u64),
    /// 64-bit form, flag 1
    GpuTimestamp,
    /// 64-bit form, flag 2
    SystemTimestamp,
}

/// A bottom-of-pipe write: `bytes` (4 or 8) at console `address` once every command before it
/// is done.
#[derive(Clone, Copy, Debug)]
pub struct LabelWrite {
    pub address: u64,
    pub value: LabelValue,
    pub bytes: u32,
    pub flush: u32,
    pub flags: u32,
}

/// A list's wait for a memory value (list 0x248 / 0x250): the commands after it run once the 32
/// or 64 bits at `address` compare with `reference` as `comparison` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryWait {
    pub address: u64,
    pub comparison: u32,
    pub reference: u64,
    pub wide: bool,
}

impl MemoryWait {
    /// Whether `value` (read at `address`) meets the wait. The comparison codes are taken to be
    /// D3D12's comparison functions (1 never .. 8 always, value against reference), as the fork
    /// read them; the title's first wait is comparison 7 (`>=`), reference 1.
    pub fn holds_for(&self, value: u64) -> bool {
        let (v, r) = if self.wide { (value, self.reference) } else { (value & 0xFFFF_FFFF, self.reference & 0xFFFF_FFFF) };
        match self.comparison {
            1 => false,
            2 => v < r,
            3 => v == r,
            4 => v <= r,
            5 => v > r,
            6 => v != r,
            7 => v >= r,
            _ => true,
        }
    }
}

/// What the list does at a memory wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitAction {
    /// Cut the list here; the wait stands before the next [`Segment`] (the default).
    Split,
    /// Drop the wait (the lead knows it is met by construction). Counted in [`ListStats::refused`]
    /// only if the lead says so by returning this.
    Drop,
}

/// The lead's side of the list: console objects and addresses to host ones, and the two things
/// that need the queue's or the memory model's knowledge (labels and memory waits).
///
/// Every `Raw` returned is a borrowed host COM pointer that must stay alive until the GPU has run
/// every list that recorded it (D3D12's own rule).
pub trait ListHost: PipelineSource + Send + Sync {
    /// The device object the title holds; `GetDevice` (0x38) answers through its `QueryInterface`.
    fn device(&self) -> Raw {
        std::ptr::null_mut()
    }
    /// A console root signature (device 0x80) as an `ID3D12RootSignature`; `None` ignores the set
    /// (the pipeline brings its own).
    fn root_signature(&self, _console: Raw) -> Option<Raw> {
        None
    }
    /// A console resource as an `ID3D12Resource`.
    fn resource(&self, console: Raw) -> Option<Raw>;
    /// A console texture whose host resource lost the unordered access the console gave it (a depth target):
    /// its host depth texture and the twin its UAVs write ([`twin`]). `None` for every other resource.
    fn uav_twin(&self, _console: Raw) -> Option<UavTwin> {
        None
    }
    /// The host resource the UAV at console CPU handle `cpu` views, when it is not `console`'s own host resource
    /// (a twin, [`twin`]): a UAV clear must name it. `None`: the resource's own.
    fn uav_resource(&self, _console: Raw, _cpu: usize) -> Option<Raw> {
        None
    }
    /// A console descriptor heap (device 0x70) bound by `SetDescriptorHeaps`.
    fn descriptor_heap(&self, console: Raw) -> Option<HeapBinding>;
    /// A console CPU descriptor handle (RTV, DSV, or the CPU side of a UAV clear) as a host CPU
    /// handle.
    fn cpu_descriptor(&self, console: usize) -> Option<usize>;
    /// A console command signature (device 0x148) on the host; `None` when the host refused it.
    fn command_signature(&self, _console: Raw) -> Option<HostSignature> {
        None
    }
    /// Console signature `console` (one whose arguments change root parameters) rewritten into the program
    /// library's layout of `map` ([`indirect::library_arguments`]) and made against `root`, the library root
    /// bound. Made once per signature and map. `None` when it cannot be mapped or made: the list refuses it.
    fn library_signature(&self, _console: Raw, _root: Raw, _map: &'static UserMap) -> Option<LibrarySignature> {
        None
    }
    /// The range of console GPU memory holding `va`. Asked only when the list's cached range
    /// misses, so a range as large as the lead can make it is fastest.
    fn memory(&self, va: u64) -> Option<MemoryRange>;
    /// The range holding `va` as the **CPU** sees it, for what the GPU writes and the title's CPU polls (labels,
    /// GPU timestamps): where GPU-read memory has a copy in card memory (the memory model's tiles), the title's
    /// own bytes are here, not there. Defaults to [`ListHost::memory`].
    fn cpu_memory(&self, va: u64) -> Option<MemoryRange> {
        self.memory(va)
    }
    /// Record a bottom-of-pipe write into `list`. `host_va` is the host GPU address of `address`
    /// if a [`MemoryRange`] covers it. The default writes values with `WriteBufferImmediate` in
    /// `MARKER_OUT` mode (Build 2's labels: the value lands once the work before it is done); a
    /// 64-bit value is written low word first, so a reader polling for `>=` never sees a value
    /// larger than the one written. GPU timestamps (flag 1) never come here: the list writes them
    /// itself ([`stamps`]: a query at the label's point, resolved into the title's memory at the host
    /// list's end); it asks the host only when it cannot (no query heap, an address it cannot place).
    /// The system timestamp (flag 2) is refused by default (return `false`).
    fn label(&self, list: &ID3D12GraphicsCommandList2, write: &LabelWrite, host_va: Option<u64>) -> bool {
        let (LabelValue::Value(v), Some(va)) = (write.value, host_va) else {
            return false;
        };
        let p = [
            D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: va, Value: v as u32 },
            D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: va + 4, Value: (v >> 32) as u32 },
        ];
        let m = [D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT; 2];
        let n = if write.bytes == 8 { 2 } else { 1 };
        unsafe { (list.vtable().WriteBufferImmediate)(list.as_raw(), n, p.as_ptr(), m.as_ptr()) };
        true
    }
    /// Bring the host mirror of console descriptor memory `start..start + len` up to date with its bytes (the
    /// title copies descriptor records as plain bytes), before a submission reads it. Returns the slots looked
    /// at. Called by [`sync_descriptors`] over the tables a list's translated programs read.
    fn sync_descriptors(&self, _start: u64, _len: u64) -> u32 {
        0
    }
    /// A memory wait was recorded (once per call, at record time).
    fn memory_wait(&self, _wait: &MemoryWait) -> WaitAction {
        WaitAction::Split
    }
    /// The first call of a trapped slot (`object` is "list" or "allocator"). Later calls only count.
    fn first_trap(&self, _object: &'static str, _offset: usize) {}
    /// `QueryInterface` was asked for an interface we do not answer.
    fn unknown_iid(&self, _object: &'static str, _iid: &windows::core::GUID) {}
}

/// Object header every console object starts with.
#[repr(C)]
pub(crate) struct Header {
    pub vtable: *const slots::Fp,
    /// device index bits 0-2 and bookkeeping; 0
    pub bits: u32,
    pub refs: std::sync::atomic::AtomicU32,
}

/// The device behind the objects: makes host allocators, lists and upload memory. A trait so the
/// unit tests can run the whole path on a counting stand-in with no GPU.
pub(crate) trait Backend: Send + Sync {
    fn allocator(&self, kind: u32) -> windows::core::Result<ID3D12CommandAllocator>;
    fn list(&self, kind: u32, allocator: &ID3D12CommandAllocator) -> windows::core::Result<ID3D12GraphicsCommandList>;
    fn upload(&self, bytes: u64) -> windows::core::Result<alloc::UploadChunk>;
    /// The host takes `ID3D12GraphicsCommandList7::Barrier` (D3D12_OPTIONS12).
    fn enhanced_barriers(&self) -> bool;
    /// The device's timestamp rings ([`stamps`]): timestamp labels and per-list timing. `None`: neither.
    fn stamps(&self) -> Option<&Arc<stamps::Stamps>> {
        None
    }
}

pub(crate) struct DeviceBackend {
    device: ID3D12Device,
    enhanced: bool,
    stamps: Option<Arc<stamps::Stamps>>,
}
// SAFETY: D3D12 devices are free-threaded.
unsafe impl Send for DeviceBackend {}
unsafe impl Sync for DeviceBackend {}

impl Backend for DeviceBackend {
    fn allocator(&self, kind: u32) -> windows::core::Result<ID3D12CommandAllocator> {
        unsafe { self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE(kind as i32)) }
    }
    fn list(&self, kind: u32, allocator: &ID3D12CommandAllocator) -> windows::core::Result<ID3D12GraphicsCommandList> {
        unsafe { self.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE(kind as i32), allocator, None) }
    }
    fn upload(&self, bytes: u64) -> windows::core::Result<alloc::UploadChunk> {
        alloc::UploadChunk::on_device(&self.device, bytes)
    }
    fn enhanced_barriers(&self) -> bool {
        self.enhanced
    }
    fn stamps(&self) -> Option<&Arc<stamps::Stamps>> {
        self.stamps.as_ref()
    }
}

pub(crate) fn backend_of(device: &ID3D12Device) -> Arc<dyn Backend> {
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
    let enhanced = unsafe {
        device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS12, (&mut o as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS12).cast(), size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS12>() as u32)
    }
    .is_ok()
        && o.EnhancedBarriersSupported.as_bool();
    Arc::new(DeviceBackend { device: device.clone(), enhanced, stamps: stamps::for_device(device) })
}
