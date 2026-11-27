//! Build 5: the D3D12.X device the title gets from `D3D12XboxCreateDevice` (docs/research/d3d12x-device.md).
//!
//! One static object, as on the console: 0x58 bytes, the method table first, byte +0x18 non-zero (else the
//! title inlines device work instead of calling). Behind it, the host `ID3D12Device` on the first adapter.
//! The title asks for `ID3D12Device11`, which selects the newer child method tables.
//!
//! Every slot not yet written is a trap: it counts its calls, keeps the time of the first one, and fails. A run
//! then names, in order, what to write next ([`traps`]). Slots that return a value or a pointer must get their
//! own implementation before the title relies on them; the trap returns `E_NOTIMPL` as a number for all.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_1;
use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device};

/// Slots in the device's method table (0x000-0x4f8).
pub const SLOTS: usize = 160;
const OBJECT_SIZE: usize = 0x58;
const E_NOTIMPL: u64 = 0x8000_4001;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;

/// The host device and the console object the title holds.
pub struct Device {
    pub host: ID3D12Device,
    /// The title-visible object (static for the run).
    pub object: usize,
    pub created: Instant,
}

// SAFETY: ID3D12Device is free-threaded.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

static DEVICE: OnceLock<Device> = OnceLock::new();
static CALLS: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
/// Microseconds after device creation of each slot's first call, plus one (0: never called).
static FIRST: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static REFS: AtomicU32 = AtomicU32::new(1);
/// What the command lists see: resources and descriptors from crates/d3d12x, pipelines from [`Pipelines`].
static LIST_HOST: OnceLock<std::sync::Arc<dyn rebuild_xlist::console::ListHost>> = OnceLock::new();

/// Events from the resources crate and the lists, printed as they happen (first traps, unknown interfaces,
/// first occurrences of things worth a line).
pub struct Log;
impl rebuild_d3d12x::Events for Log {
    fn first_trap(&self, object: &'static str, offset: usize) {
        eprintln!("trap      {object} {offset:#05x} (first call)");
    }
    fn unknown_iid(&self, object: &'static str, iid: &windows::core::GUID) {
        eprintln!("iid       {object} asked for {iid:?}");
    }
    fn note(&self, what: &'static str, value: u64) {
        eprintln!("note      {what} {value:#x}");
    }
}

/// Pipelines for the lists. Until the title creates one (device 0x50 / 0x58 / 0x1f0 / 0x2b0, served from the
/// pipeline library and the pool in crates/pipelines), there are none.
pub struct Pipelines;
impl rebuild_xlist::console::PipelineSource for Pipelines {
    #[inline]
    fn pipeline(&self, console: rebuild_xlist::console::Raw) -> Option<rebuild_xlist::console::HostPipeline> {
        crate::pipeline::host(console)
    }
}
/// Slots with an implementation (ours or another crate's), by index.
static WRITTEN: [std::sync::atomic::AtomicBool; SLOTS] = [const { std::sync::atomic::AtomicBool::new(false) }; SLOTS];

pub fn get() -> Option<&'static Device> {
    DEVICE.get()
}

/// Names of the slots we know (research CSV), for the trap report.
fn slot_name(off: usize) -> &'static str {
    match off {
        0x000 => "QueryInterface",
        0x008 => "AddRef",
        0x010 => "Release",
        0x018 => "GetPrivateData",
        0x020 => "SetPrivateData",
        0x028 => "SetPrivateDataInterface",
        0x030 => "SetName",
        0x038 => "GetNodeCount",
        0x040 => "CreateCommandQueue",
        0x048 => "CreateCommandAllocator",
        0x050 => "CreateGraphicsPipelineState",
        0x058 => "CreateComputePipelineState",
        0x060 => "CreateCommandList",
        0x068 => "CheckFeatureSupport",
        0x070 => "CreateDescriptorHeap",
        0x078 => "GetDescriptorHandleIncrementSize",
        0x080 => "CreateRootSignature",
        0x088 => "CreateConstantBufferView",
        0x090 => "CreateShaderResourceView",
        0x098 => "CreateUnorderedAccessView",
        0x0a0 => "CreateRenderTargetView",
        0x0a8 => "CreateDepthStencilView",
        0x0b0 => "CreateSampler",
        0x0b8 => "CopyDescriptors",
        0x0c0 => "CopyDescriptorsSimple",
        0x0c8 => "GetResourceAllocationInfo",
        0x0d0 => "GetCustomHeapProperties",
        0x0d8 => "CreateCommittedResource",
        0x0e0 => "CreateHeap",
        0x0e8 => "CreatePlacedResource",
        0x0f0 => "CreateReservedResource",
        0x120 => "CreateFence",
        0x128 => "GetDeviceRemovedReason",
        0x130 => "GetCopyableFootprints",
        0x138 => "CreateQueryHeap",
        0x140 => "SetStablePowerState",
        0x148 => "CreateCommandSignature",
        0x180 => "CreateCommandQueueX",
        0x1c8 => "console pipeline (fails on purpose)",
        0x1e8 => "CreateSamplerX",
        0x1f0 => "CreateComputePipelineStateX",
        0x218 => "console pipeline, FSR (fails on purpose)",
        0x228 => "debug error filter",
        0x230 => "CreatePlacedShaderResourceView",
        0x238 => "CreatePlacedUnorderedAccessView",
        0x248 => "CreateCommandListX",
        0x250 => "RegisterPagePool",
        0x258 => "UnregisterPagePool",
        0x260 => "CreateCommittedResourceX",
        0x270 => "SetFrameIntervalX",
        0x278 => "ScheduleFrameEventX",
        0x280 => "WaitFrameEventX",
        0x288 => "GetFrameStatisticsX",
        0x2b0 => "CreatePipelineState",
        0x360 => "GetResourceAllocationInfo2",
        0x3f8 => "CreateCommittedResource3",
        0x428 => "create resource at a title address",
        _ => "?",
    }
}

fn note(slot: usize) {
    if CALLS[slot].fetch_add(1, Ordering::Relaxed) == 0 {
        let us = DEVICE.get().map(|d| d.created.elapsed().as_micros() as u64).unwrap_or(0);
        FIRST[slot].store(us + 1, Ordering::Relaxed);
    }
}

/// Every slot called but not written: (offset, name, calls, first call in ms after device creation), first first.
pub fn traps() -> Vec<(usize, &'static str, u32, f64)> {
    let mut v: Vec<(usize, &'static str, u32, f64)> = (0..SLOTS)
        .filter(|&s| !implemented(s * 8) && CALLS[s].load(Ordering::Relaxed) > 0)
        .map(|s| (s * 8, slot_name(s * 8), CALLS[s].load(Ordering::Relaxed), (FIRST[s].load(Ordering::Relaxed) - 1) as f64 / 1000.0))
        .collect();
    v.sort_by(|a, b| a.3.total_cmp(&b.3));
    v
}

/// Every slot the title called through our own functions (implemented or not), first call first. Slots served by
/// other crates' thunks (resources, views) count in those crates.
pub fn called() -> Vec<(usize, &'static str, u32, f64, bool)> {
    let mut v: Vec<_> = (0..SLOTS)
        .filter(|&s| CALLS[s].load(Ordering::Relaxed) > 0)
        .map(|s| (s * 8, slot_name(s * 8), CALLS[s].load(Ordering::Relaxed), (FIRST[s].load(Ordering::Relaxed) - 1) as f64 / 1000.0, implemented(s * 8)))
        .collect();
    v.sort_by(|a, b| a.3.total_cmp(&b.3));
    v
}

fn implemented(off: usize) -> bool {
    WRITTEN[off / 8].load(Ordering::Relaxed)
}

type Trap = unsafe extern "system" fn(*mut c_void, usize, usize, usize, usize, usize, usize) -> u64;

unsafe extern "system" fn trap<const SLOT: usize>(_this: *mut c_void, _a: usize, _b: usize, _c: usize, _d: usize, _e: usize, _f: usize) -> u64 {
    note(SLOT);
    E_NOTIMPL
}

macro_rules! traps_table {
    ($($n:literal)*) => { [$(trap::<$n> as Trap),*] };
}

fn trap_table() -> [Trap; SLOTS] {
    traps_table!(
        0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39
        40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79
        80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110 111 112 113 114 115 116 117 118 119
        120 121 122 123 124 125 126 127 128 129 130 131 132 133 134 135 136 137 138 139 140 141 142 143 144 145 146 147 148 149 150 151 152 153 154 155 156 157 158 159
    )
}

// ------------------------------------------------------------------------------------- the written slots

/// IIDs the device answers for itself: IUnknown, ID3D12Object, ID3D12DeviceChild-less device levels 0-11.
fn is_device_iid(g: &[u8; 16]) -> bool {
    const PREFIXES: [u32; 14] = [
        0x0000_0000, // IUnknown
        0xc4fe_c28f, // ID3D12Object
        0x1898_19f1, // ID3D12Device
        0x77ac_ce80, // ID3D12Device1
        0x30ba_a41e, // ID3D12Device2
        0x81da_dc15, // ID3D12Device3
        0xe865_df17, // ID3D12Device4
        0x8b4f_173b, // ID3D12Device5
        0xc70b_221b, // ID3D12Device6
        0x5c01_4b53, // ID3D12Device7
        0x9218_e6bb, // ID3D12Device8
        0x4c80_e962, // ID3D12Device9
        0x517f_8718, // ID3D12Device10
        0x5405_c344, // ID3D12Device11
    ];
    let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    PREFIXES.contains(&d1)
}

unsafe extern "system" fn QueryInterface(this: *mut c_void, iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    note(0);
    if iid.is_null() || out.is_null() {
        return 0x8000_4003u32 as i32; // E_POINTER
    }
    if is_device_iid(unsafe { &*iid }) {
        REFS.fetch_add(1, Ordering::Relaxed);
        unsafe { *out = this };
        return 0;
    }
    if crate::dxgi::is_dxgi_device(unsafe { &*iid }) {
        unsafe { *out = crate::dxgi::device() };
        return 0;
    }
    unsafe { *out = std::ptr::null_mut() };
    UNKNOWN_IID.fetch_add(1, Ordering::Relaxed);
    let _ = LAST_IID.set(unsafe { *iid });
    E_NOINTERFACE
}

/// For the DXGI device's `QueryInterface` (it answers for the D3D12 device too).
pub fn is_device_iid_pub(g: &[u8; 16]) -> bool {
    is_device_iid(g)
}

static UNKNOWN_IID: AtomicU32 = AtomicU32::new(0);
static LAST_IID: OnceLock<[u8; 16]> = OnceLock::new();

/// The first interface the title asked the device for that it does not serve, and how many such asks.
pub fn unknown_iid() -> (u32, Option<[u8; 16]>) {
    (UNKNOWN_IID.load(Ordering::Relaxed), LAST_IID.get().copied())
}

unsafe extern "system" fn AddRef(_this: *mut c_void) -> u32 {
    note(1);
    REFS.fetch_add(1, Ordering::Relaxed) + 1
}

/// The device is static for the run: the count is kept for the title's sake, the object never freed.
unsafe extern "system" fn Release(_this: *mut c_void) -> u32 {
    note(2);
    REFS.fetch_sub(1, Ordering::Relaxed).saturating_sub(1)
}

unsafe extern "system" fn GetPrivateData(_this: *mut c_void, _guid: *const c_void, size: *mut u32, _data: *mut c_void) -> i32 {
    note(3);
    if !size.is_null() {
        unsafe { *size = 0 };
    }
    0x887A_0002u32 as i32 // DXGI_ERROR_NOT_FOUND
}

unsafe extern "system" fn SetPrivateData(_this: *mut c_void, _guid: *const c_void, _size: u32, _data: *const c_void) -> i32 {
    note(4);
    0
}

unsafe extern "system" fn SetPrivateDataInterface(_this: *mut c_void, _guid: *const c_void, _data: *mut c_void) -> i32 {
    note(5);
    0
}

unsafe extern "system" fn SetName(_this: *mut c_void, _name: *const u16) -> i32 {
    note(6);
    0
}

unsafe extern "system" fn GetNodeCount(_this: *mut c_void) -> u32 {
    note(7);
    1
}

unsafe extern "system" fn CreateCommandQueue(
    _this: *mut c_void,
    desc: *const windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_DESC,
    _iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> i32 {
    note(0x40 / 8);
    if desc.is_null() || out.is_null() {
        return 0x8007_0057u32 as i32;
    }
    match crate::queue::create(unsafe { &*desc }) {
        Ok(q) => {
            unsafe { *out = q as *mut c_void };
            0
        }
        Err(hr) => hr,
    }
}

unsafe extern "system" fn CreateFence(_this: *mut c_void, initial: u64, _flags: u32, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    note(0x120 / 8);
    if out.is_null() {
        return 0x8007_0057u32 as i32;
    }
    match crate::fence::create(initial) {
        Ok(f) => {
            unsafe { *out = f as *mut c_void };
            0
        }
        Err(hr) => hr,
    }
}

/// `CreateCommandQueueX(*{type, flags, engine or pipe, queue}, iid, out)`: the console's extended description;
/// on the host a queue of that type.
unsafe extern "system" fn CreateCommandQueueX(_this: *mut c_void, desc: *const [u32; 4], _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    note(0x180 / 8);
    if desc.is_null() || out.is_null() {
        return 0x8007_0057u32 as i32;
    }
    let d = unsafe { *desc };
    let host = windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_DESC {
        Type: windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_LIST_TYPE(d[0] as i32),
        ..Default::default()
    };
    match crate::queue::create(&host) {
        Ok(q) => {
            unsafe { *out = q as *mut c_void };
            0
        }
        Err(hr) => hr,
    }
}

fn hr(e: windows::core::Error) -> i32 {
    e.code().0
}

/// `CreateCommandAllocator(type, iid, out)`: crates/xlist's console allocator over the host device.
unsafe extern "system" fn CreateCommandAllocator(_this: *mut c_void, kind: u32, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    note(0x48 / 8);
    let (Some(d), Some(host)) = (DEVICE.get(), LIST_HOST.get()) else { return 0x8000_4005u32 as i32 };
    if out.is_null() {
        return 0x8007_0057u32 as i32;
    }
    match rebuild_xlist::console::create_allocator(kind, &d.host, host.clone()) {
        Ok(a) => {
            unsafe { *out = a };
            0
        }
        Err(e) => hr(e),
    }
}

/// `CreateCommandList(nodemask, type, allocator, pipeline, iid, out)`: an open console list (crates/xlist).
unsafe extern "system" fn CreateCommandList(
    _this: *mut c_void,
    _nodemask: u32,
    kind: u32,
    allocator: *mut c_void,
    pipeline: *mut c_void,
    _iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> i32 {
    note(0x60 / 8);
    if out.is_null() || allocator.is_null() {
        return 0x8007_0057u32 as i32;
    }
    match rebuild_xlist::console::create_list(kind, allocator, pipeline) {
        Ok(l) => {
            unsafe { *out = l };
            0
        }
        Err(e) => hr(e),
    }
}

/// `CreateCommandListX(*{type, flags, nodemask, barrier batch}, allocator, pipeline, iid, out)`.
unsafe extern "system" fn CreateCommandListX(
    _this: *mut c_void,
    desc: *const [u32; 4],
    allocator: *mut c_void,
    pipeline: *mut c_void,
    _iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> i32 {
    note(0x248 / 8);
    if desc.is_null() || out.is_null() || allocator.is_null() {
        return 0x8007_0057u32 as i32;
    }
    let kind = unsafe { (*desc)[0] };
    match rebuild_xlist::console::create_list(kind, allocator, pipeline) {
        Ok(l) => {
            unsafe { *out = l };
            0
        }
        Err(e) => hr(e),
    }
}

// ------------------------------------------------------------------------------------- creation

static VTABLE: OnceLock<Box<[usize; SLOTS]>> = OnceLock::new();

fn vtable() -> &'static [usize; SLOTS] {
    VTABLE.get_or_init(|| {
        let mut t = Box::new([0usize; SLOTS]);
        for (i, f) in trap_table().into_iter().enumerate() {
            t[i] = f as usize;
        }
        t[0] = QueryInterface as *const () as usize;
        t[1] = AddRef as *const () as usize;
        t[2] = Release as *const () as usize;
        t[3] = GetPrivateData as *const () as usize;
        t[4] = SetPrivateData as *const () as usize;
        t[5] = SetPrivateDataInterface as *const () as usize;
        t[6] = SetName as *const () as usize;
        t[7] = GetNodeCount as *const () as usize;
        t[0x40 / 8] = CreateCommandQueue as *const () as usize;
        for s in 0..=7 {
            WRITTEN[s].store(true, Ordering::Relaxed);
        }
        t[0x120 / 8] = CreateFence as *const () as usize;
        t[0x180 / 8] = CreateCommandQueueX as *const () as usize;
        t[0x48 / 8] = CreateCommandAllocator as *const () as usize;
        t[0x60 / 8] = CreateCommandList as *const () as usize;
        t[0x248 / 8] = CreateCommandListX as *const () as usize;
        t[0x270 / 8] = crate::frame::SetFrameIntervalX as *const () as usize;
        t[0x278 / 8] = crate::frame::ScheduleFrameEventX as *const () as usize;
        t[0x280 / 8] = crate::frame::WaitFrameEventX as *const () as usize;
        t[0x288 / 8] = crate::frame::GetFrameStatisticsX as *const () as usize;
        t[0x58 / 8] = crate::pipeline::CreateComputePipelineState as *const () as usize;
        t[0x1f0 / 8] = crate::pipeline::CreateComputePipelineStateX as *const () as usize;
        t[0x50 / 8] = crate::pipeline::CreateGraphicsPipelineState as *const () as usize;
        t[0x2b0 / 8] = crate::pipeline::CreatePipelineState as *const () as usize;
        for off in [0x40, 0x48, 0x50, 0x58, 0x60, 0x120, 0x180, 0x1f0, 0x248, 0x270, 0x278, 0x280, 0x288, 0x2b0] {
            WRITTEN[off / 8].store(true, Ordering::Relaxed);
        }
        // resources, heaps, views (crates/d3d12x)
        for (off, _name, f) in rebuild_d3d12x::thunks::device_slots() {
            t[off / 8] = f.0 as usize;
            WRITTEN[off / 8].store(true, Ordering::Relaxed);
        }
        t
    })
}

/// `D3D12XboxCreateDevice(adapter, params, riid, out)`: the host device on the first adapter and the console
/// object around it. Called once; later calls get the same device.
pub fn create(riid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return 0x8007_0057u32 as i32; // E_INVALIDARG
    }
    if !riid.is_null() && !is_device_iid(unsafe { &*riid }) {
        return E_NOINTERFACE;
    }
    if DEVICE.get().is_none() {
        let mut host: Option<ID3D12Device> = None;
        if let Err(e) = unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_1, &mut host) } {
            eprintln!("device: the host D3D12 device failed: {e}");
            return e.code().0;
        }
        let Some(host) = host else { return 0x8000_4005u32 as i32 };
        // resources, heaps and views behind the device; memory is a host stand-in until the tiles join (Build 1)
        let memory = match rebuild_d3d12x::HostMemory::new(&host, 1 << 30) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("device: resource memory: {e}");
                return e.code().0;
            }
        };
        let memory = Accounted { inner: memory, title: std::sync::Mutex::new(std::collections::HashMap::new()) };
        let resources = match rebuild_d3d12x::Resources::new(&host, std::sync::Arc::new(memory), std::sync::Arc::new(Log)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("device: resources: {e}");
                return e.code().0;
            }
        };
        let resources_ref = resources.clone();
        // pipelines: the program library and the creation pool (workers start now)
        crate::pipeline::start(&host);
        let _ = LIST_HOST.set(std::sync::Arc::new(rebuild_d3d12x::Host { resources: resources.clone(), pipelines: Pipelines }));
        rebuild_d3d12x::thunks::install(resources);
        // the console object: method table first, byte +0x18 = 1 (calls, not inline work)
        let object = Box::leak(Box::new([0u8; OBJECT_SIZE]));
        unsafe {
            (object.as_mut_ptr() as *mut usize).write(vtable().as_ptr() as usize);
            object[0x18] = 1;
        }
        resources_ref.set_device(object.as_ptr() as _);
        let _ = DEVICE.set(Device { host, object: object.as_ptr() as usize, created: Instant::now() });
    }
    let d = DEVICE.get().expect("device");
    REFS.fetch_add(1, Ordering::Relaxed);
    unsafe { *out = d.object as *mut c_void };
    0
}

/// The host device as a raw COM pointer, for crates that take one.
/// The device object the title holds.
pub fn title_object() -> Option<*mut c_void> {
    DEVICE.get().map(|d| d.object as *mut c_void)
}

pub fn host_raw() -> Option<*mut c_void> {
    DEVICE.get().map(|d| d.host.as_raw())
}

/// XALLOC attributes of the console D3D runtime's own memory: allocator ID 0x80 (D3D, bits 16-23), page-size
/// code 2 (bits 14-15). The title's allocation hooks route these (its render setup serves the back buffers
/// from an arena it opens for allocator 0x80 and asserts the arena ends up full).
pub const D3D_ATTRIBUTES: u64 = (0x80 << 16) | (2 << 14);

/// Resource memory as the console D3D runtime takes it: every committed resource or heap is also an
/// `XMemAlloc` through the title's hooks with [`D3D_ATTRIBUTES`] (so the title's budgets and arenas see it),
/// freed with `XMemFree` when the resource goes. The bytes the GPU uses come from `inner` until the tiles
/// join; the title only keeps the hooked address for its bookkeeping.
struct Accounted<M> {
    inner: M,
    /// our address -> the title's
    title: std::sync::Mutex<std::collections::HashMap<u64, u64>>,
}

impl<M: rebuild_d3d12x::Memory> rebuild_d3d12x::Memory for Accounted<M> {
    fn allocate(&self, bytes: u64, alignment: u64, heap: &rebuild_d3d12x::format::HeapProperties) -> Option<u64> {
        let at = self.inner.allocate(bytes, alignment, heap)?;
        let t = crate::exports::XMemAlloc(bytes as usize, D3D_ATTRIBUTES) as u64;
        if t != 0 {
            self.title.lock().unwrap_or_else(|e| e.into_inner()).insert(at, t);
        }
        Some(at)
    }
    fn free(&self, address: u64, bytes: u64) {
        if let Some(t) = self.title.lock().unwrap_or_else(|e| e.into_inner()).remove(&address) {
            crate::exports::XMemFree(t as *mut c_void, D3D_ATTRIBUTES);
        }
        self.inner.free(address, bytes);
    }
    fn range(&self, va: u64) -> Option<rebuild_d3d12x::MemoryRange> {
        self.inner.range(va)
    }
    fn buffer(&self, address: u64, bytes: u64) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
        self.inner.buffer(address, bytes)
    }
    fn texture(&self, address: u64, desc: &windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DESC) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> {
        self.inner.texture(address, desc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_object_calls_through_its_table_and_untouched_slots_trap() {
        // a device on the card: never while the lead is running or timing the title
        if std::path::Path::new(r"D:\Developer\triumph-rebuild\agents\MEASURING").exists() {
            eprintln!("skipped: agents\\MEASURING is present");
            return;
        }
        let mut out: *mut c_void = std::ptr::null_mut();
        // ID3D12Device11
        let iid: [u8; 16] = [0x44, 0xc3, 0x05, 0x54, 0x57, 0xd4, 0x4e, 0x44, 0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39];
        if create(&iid, &mut out) != 0 {
            eprintln!("skipped: no D3D12 device");
            return;
        }
        let obj = out as *const u8;
        assert_eq!(unsafe { *obj.add(0x18) }, 1, "the call-don't-inline byte");
        let table = unsafe { *(obj as *const *const usize) };
        let node: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*table.add(7)) };
        assert_eq!(unsafe { node(out) }, 1);
        // 0x1c8 stays a trap on purpose (the console pipeline path the research says fails)
        let console_pipeline: Trap = unsafe { std::mem::transmute(*table.add(0x1c8 / 8)) };
        assert_eq!(unsafe { console_pipeline(out, 0, 0, 0, 0, 0, 0) }, E_NOTIMPL);
        assert!(traps().iter().any(|t| t.0 == 0x1c8));
        // the same device for a second ask
        let mut again: *mut c_void = std::ptr::null_mut();
        assert_eq!(create(&iid, &mut again), 0);
        assert_eq!(again, out);
    }
}
