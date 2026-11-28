//! Build 5: queue 0x100 `PresentX`: the title's frame on the screen.
//!
//! **The call** (docs/research/d3d12x-device.md §2.2, `agents/d3d12x-child-slots.csv`; the fork's
//! `queue.rs:1117-1123, 1288-1395`):
//!
//! ```text
//! HRESULT PresentX(queue, u32 planes, const Plane* planes, const void* parameters)
//! Plane = { u64 frame token, u32 resource count, ID3D12Resource* const* resources, ... }   // the first plane only
//! ```
//!
//! The image is plane 0's first resource, a console resource whose host texture comes from crates/d3d12x (through
//! the lists' `ListHost::resource`). The fork read the call the same way and showed frames with it (run 28: one
//! plane). What follows the first three fields of a plane, the plane stride, and `parameters` are unread: with one
//! plane none of them is needed. The title has always passed one plane.
//!
//! **The window and the swap chain.** On the first present: a borderless window covering the primary display (its
//! own thread pumps its messages, so the title's threads never do), and a flip-model swap chain on the host queue
//! that backs the presenting console queue, at the host display's mode (`dxgi::host_mode`), three buffers, the
//! frame-latency waitable at 2, and `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` when the factory reports tearing support
//! (`IDXGIFactory5::CheckFeatureSupport`; anything that resizes the buffers must pass the same flags,
//! the presenter's `flags`; nothing resizes them yet). Its format follows the back buffer's family (RGBA8, BGRA8, RGB10A2, RGBA16F;
//! anything else RGBA8).
//!
//! **Each present** is one small host list, recorded once per (frame slot, swap-chain buffer, source, size) and
//! executed again while those stay the same (no allocator reset, no recording; nothing allocated):
//!
//! - **copy** when the back buffer is of the swap chain's format family and fits: `CopyTextureRegion` into the
//!   top-left corner, and the swap chain's source size set to the back buffer's (`IDXGISwapChain2::SetSourceSize`),
//!   so the compositor or the display scales it to the screen: no shader, no extra pass;
//! - **blit** otherwise (another format family, larger than the screen): a full-screen triangle samples the back
//!   buffer (linear) into the swap-chain buffer; the sampler converts the format.
//!
//! The back buffer is read in the state the title left it in for presenting (`PRESENT` = `COMMON`; the copy or the
//! shader read promotes it implicitly and it decays back after). Then `Present`, with vsync as set now
//! ([`crate::frame::vsync`], `REBUILD_VSYNC`): on = `Present(1, 0)`; off = `Present(0, ALLOW_TEARING)` when the
//! swap chain allows tearing, else `Present(0, 0)`. Pacing is [`crate::frame`]'s. **The title's thread is never blocked for the display**: if the waitable says the display
//! queue is full, or the slot's last copy has not finished, the frame is not shown and is counted `late`.
//!
//! **GPU time.** Each frame slot has two timestamps on the swap chain's queue: one before the first submission of
//! the frame (a list recorded once per slot, put in front by [`crate::submit`]), one at the end of our copy. Their
//! difference, read when the slot comes round again, is the frame line's `gpu`.
//!
//! **Capture.** [`request_capture`] (the frame-line thread calls it when `capture.request` appears in the session
//! folder), or `REBUILD_CAPTURE_FRAMES=2,30` (presented frame numbers, from 1): the shown image (the swap chain's
//! buffer, the back buffer's size) is copied to a readback buffer in the same list, and a thread of ours writes it
//! as `captures/frame-<n>.png` in the session folder (`REBUILD_SESSION_DIR`, else the current folder) once the GPU
//! is done, then deletes `capture.request`.
//!
//! **The in-game menu** ([`crate::overlay`], task L5): while it (or the stats readout) is shown, a second small list
//! per frame draws it over the swap chain's buffer after the title's image, executed with the present list. It
//! never touches the title's image, and a capture is copied before it unless `REBUILD_CAPTURE_OVERLAY=1`. While
//! nothing is shown its cost is one relaxed load a present.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::queue::Queue;

/// Frame slots (allocators, lists, timestamps) and swap-chain buffers.
const FRAMES: usize = 3;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;

/// The start of a present plane.
#[repr(C)]
pub struct Plane {
    pub token: u64,
    pub count: u32,
    pub resources: *const *mut c_void,
}

// ------------------------------------------------------------------------------------- shared with submission

/// The swap chain's queue (a `&'static Queue` as usize; 0 before the first present).
static PRESENT_QUEUE: AtomicUsize = AtomicUsize::new(0);
/// The slot the next present uses, and each slot's "frame begins" timestamp list (raw `ID3D12GraphicsCommandList`).
static SLOT: AtomicUsize = AtomicUsize::new(0);
static BEGIN: [AtomicUsize; FRAMES] = [const { AtomicUsize::new(0) }; FRAMES];
/// The present fence (raw `ID3D12Fence`) and the value each slot's last frame signals: a slot's begin list may
/// only go out again once that frame is done (re-executing a list still in flight is invalid; a card can hang on it).
static FENCE: AtomicUsize = AtomicUsize::new(0);
static DONE: [std::sync::atomic::AtomicU64; FRAMES] = [const { std::sync::atomic::AtomicU64::new(0) }; FRAMES];

/// Whether the swap chain allows tearing (`None` before the first present).
static TEARING: AtomicUsize = AtomicUsize::new(0);

pub fn tearing() -> Option<bool> {
    match TEARING.load(Ordering::Relaxed) {
        0 => None,
        1 => Some(false),
        _ => Some(true),
    }
}

/// Wait (until `deadline` at most) for the frame slot the next present uses to be off the GPU: with pacing off
/// ([`crate::frame`]), this keeps the title's thread within the present's [`FRAMES`] slots of the GPU. `true` at
/// once when there is no swap chain yet or the slot is free; `false` when the deadline passed first.
pub fn wait_slot(deadline: Instant) -> bool {
    if PRESENT_QUEUE.load(Ordering::Acquire) == 0 {
        return true;
    }
    let f = FENCE.load(Ordering::Relaxed);
    if f == 0 {
        return true;
    }
    let want = DONE[SLOT.load(Ordering::Relaxed)].load(Ordering::Relaxed);
    // SAFETY: the present's fence, alive while the swap chain is (cleared at shutdown)
    let raw = f as *mut c_void;
    let Some(fence) = (unsafe { ID3D12Fence::from_raw_borrowed(&raw) }) else { return true };
    if unsafe { fence.GetCompletedValue() } >= want {
        return true;
    }
    thread_local! {
        static EVENT: HANDLE = unsafe { CreateEventW(None, false, false, None) }.unwrap_or_default();
    }
    let event = EVENT.with(|e| *e);
    if event.is_invalid() || unsafe { fence.SetEventOnCompletion(want, event) }.is_err() {
        return true;
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if unsafe { fence.GetCompletedValue() } >= want {
            return true;
        }
        if left.is_zero() {
            return false;
        }
        // an earlier wait's late signal may wake this one: the loop checks the fence again
        let ms = u32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(u32::MAX - 1);
        unsafe { WaitForSingleObject(event, ms) };
    }
}

/// The first submission of a frame on the swap chain's queue goes behind the frame's start timestamp: the list
/// to put in front of it, if this is that submission.
#[inline]
pub fn frame_begin(q: &'static Queue) -> Option<*mut c_void> {
    if PRESENT_QUEUE.load(Ordering::Relaxed) != q as *const Queue as usize || q.frame_started.swap(true, Ordering::AcqRel) {
        return None;
    }
    let slot = SLOT.load(Ordering::Relaxed);
    let l = BEGIN[slot].load(Ordering::Relaxed);
    if l == 0 {
        return None;
    }
    let f = FENCE.load(Ordering::Relaxed);
    if f != 0 {
        // SAFETY: the present's fence, alive for the run (stored once the swap chain exists)
        let raw = f as *mut c_void;
        let fence = unsafe { ID3D12Fence::from_raw_borrowed(&raw) };
        if fence.is_some_and(|fence| unsafe { fence.GetCompletedValue() } < DONE[slot].load(Ordering::Relaxed)) {
            return None; // still in flight: this frame goes without its start timestamp
        }
    }
    Some(l as *mut c_void)
}

// ------------------------------------------------------------------------------------- capture requests

static CAPTURE: AtomicBool = AtomicBool::new(false);
static CAPTURE_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
static CAPTURE_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static CAPTURES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Capture the next shown frame. `request`: the session's `capture.request`, deleted once the PNG is written.
pub fn request_capture(request: Option<PathBuf>) {
    if CAPTURE.load(Ordering::Acquire) {
        return;
    }
    *CAPTURE_FILE.lock().unwrap_or_else(|e| e.into_inner()) = request;
    CAPTURE.store(true, Ordering::Release);
}

/// Where captures go (default: `<REBUILD_SESSION_DIR>/captures`, else `./captures`).
pub fn set_capture_dir(dir: PathBuf) {
    *CAPTURE_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

/// The PNGs written so far.
pub fn captures() -> Vec<PathBuf> {
    CAPTURES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Make the window an off-screen tool window that never takes focus (tests, headless checks). Before the first
/// present only.
pub fn hide_window() {
    WINDOW_HIDDEN.store(true, Ordering::Relaxed);
}

fn trace_calls() -> Option<(u64, u64)> {
    static R: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *R.get_or_init(|| {
        let v = std::env::var("REBUILD_TRACE_CALLS").ok()?;
        let mut it = v.split('-').map(|x| x.trim().parse::<u64>().ok());
        let a = it.next()??;
        let b = it.next().flatten().unwrap_or(a);
        if a <= 1 {
            rebuild_xlist::console::trace::ON.store(true, Ordering::Relaxed);
        }
        Some((a, b))
    })
}

fn capture_frames() -> &'static [u64] {
    static F: OnceLock<Vec<u64>> = OnceLock::new();
    F.get_or_init(|| std::env::var("REBUILD_CAPTURE_FRAMES").map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default())
}

// ------------------------------------------------------------------------------------- formats

/// The swap chain format for a back buffer format (by family: a copy between them is valid).
pub fn family(format: u32) -> Option<DXGI_FORMAT> {
    Some(match format {
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        _ => return None,
    })
}

/// A typed format to read a (possibly typeless) texture through. sRGB data is read as UNORM, so its bytes reach the
/// (UNORM) swap chain unchanged.
pub fn typed(format: u32) -> DXGI_FORMAT {
    DXGI_FORMAT(match format {
        9 => 10,
        23 => 24,
        27 | 29 => 28,
        90 | 91 => 87,
        92 | 93 => 88,
        f => f,
    } as i32)
}

fn bytes_per_pixel(f: DXGI_FORMAT) -> u32 {
    if f == DXGI_FORMAT_R16G16B16A16_FLOAT {
        8
    } else {
        4
    }
}

// ------------------------------------------------------------------------------------- the presenter

/// A present list for one (slot, swap-chain buffer), kept recorded while the source and its size stay the same:
/// a present is then one `ExecuteCommandLists` of a list recorded before (no allocator reset, no recording).
struct Cached {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    /// what it was recorded for (the source held, so its pointer cannot be reused while cached); `None`: record
    key: Option<(ID3D12Resource, u32, u32, bool)>,
}

struct Slot {
    cache: Vec<Cached>,
    /// the slot's "frame begins" timestamp, recorded once
    _begin: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    /// fence value of its last use
    done: u64,
    /// its timestamps belong to a shown frame
    timed: bool,
}

struct Blit {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

struct Presenter {
    device: ID3D12Device,
    queue: &'static Queue,
    swap: IDXGISwapChain3,
    waitable: HANDLE,
    _window: HWND,
    format: DXGI_FORMAT,
    /// the swap chain's creation flags: `ResizeBuffers` must pass the same (nothing resizes yet)
    #[allow(dead_code)]
    flags: u32,
    /// `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` is among them
    tearing: bool,
    size: (u32, u32),
    buffers: Vec<ID3D12Resource>,
    rtv: ID3D12DescriptorHeap,
    rtv_step: usize,
    srv: ID3D12DescriptorHeap,
    srv_step: u64,
    slots: Vec<Slot>,
    fence: ID3D12Fence,
    value: u64,
    queries: ID3D12QueryHeap,
    stamps: ID3D12Resource,
    stamp_cells: *const u64,
    frequency: u64,
    blit: Option<Blit>,
    /// the source size the swap chain shows (`SetSourceSize`), and whether it can
    shown: (u32, u32),
    source_size_works: bool,
    shown_frames: u64,
}

// SAFETY: D3D12 and DXGI objects are free-threaded; the presenter is used under its lock.
unsafe impl Send for Presenter {}

static PRESENTER: Mutex<Option<Presenter>> = Mutex::new(None);
/// Time inside DXGI's `Present` (ns, summed over shown frames): the part of PresentX that is the driver's.
pub static DXGI_NS: AtomicU64 = AtomicU64::new(0);
/// Presents that found the presenter busy (two threads presenting at once).
static BUSY: AtomicU64 = AtomicU64::new(0);

fn borrowed(r: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    // SAFETY: a borrowed pointer in a field D3D12 only reads; ManuallyDrop never releases it.
    unsafe { std::mem::transmute_copy(r) }
}

fn transition(r: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER { pResource: borrowed(r), Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, StateBefore: before, StateAfter: after }),
        },
    }
}

/// PresentX calls so far (shown or not).
pub static PRESENT_CALLS: AtomicU64 = AtomicU64::new(0);

/// `PresentX(planes, *planes, *parameters)`: see the module notes.
pub unsafe extern "system" fn PresentX(this: *mut c_void, planes: u32, plane: *const Plane, _parameters: *const c_void) -> i32 {
    let started = Instant::now();
    let n = PRESENT_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    // REBUILD_TRACE_CALLS=a-b: the list calls recorded for frames a..=b (between presents a-1 and b)
    if let Some((a, b)) = trace_calls() {
        rebuild_xlist::console::trace::ON.store((a..=b).contains(&(n + 1)), Ordering::Relaxed);
    }
    let Some(q) = crate::queue::find(this as usize) else { return E_INVALIDARG };
    // SAFETY: the title passes `planes` planes; the first is read.
    let Some(p) = (planes > 0).then(|| unsafe { plane.as_ref() }).flatten() else { return E_INVALIDARG };
    if p.count == 0 || p.resources.is_null() {
        return E_INVALIDARG;
    }
    let console = unsafe { *p.resources };
    // REBUILD_TRACE_PLANES: the first presents' planes, as raw qwords (planes past the first are not shown yet)
    if std::env::var_os("REBUILD_TRACE_PLANES").is_some() && n <= 3 {
        let words = unsafe { std::slice::from_raw_parts(plane as *const u64, (planes as usize).min(4) * 12) };
        eprintln!("present   {n}: {planes} planes, plane 0 count {} resources {:?}; qwords {:x?}", p.count, unsafe { std::slice::from_raw_parts(p.resources, p.count.min(4) as usize) }, words);
        for (k, w) in words.iter().enumerate() {
            if let Some(h) = crate::device::list_host().and_then(|h| h.resource(*w as *mut c_void)) {
                eprintln!("present   qword {k} {w:#x} is a resource (host {h:p})");
            }
        }
    }
    let Some(raw) = crate::device::list_host().and_then(|h| h.resource(console)) else {
        static ONCE: AtomicBool = AtomicBool::new(false);
        if !ONCE.swap(true, Ordering::Relaxed) {
            eprintln!("present   plane 0's resource {console:p} has no host texture: not shown");
        }
        return 0;
    };
    // SAFETY: a live host resource the console resource holds.
    let Some(source) = (unsafe { ID3D12Resource::from_raw_borrowed(&raw) }) else { return 0 };
    let Ok(mut guard) = PRESENTER.try_lock() else {
        BUSY.fetch_add(1, Ordering::Relaxed);
        crate::framelog::late();
        return 0;
    };
    let desc = unsafe { source.GetDesc() };
    if guard.is_none() {
        eprintln!(
            "present   the image: console {console:p}, host {raw:p}, {}x{} format {} flags {:#x}, {} mips, {} samples",
            desc.Width, desc.Height, desc.Format.0, desc.Flags.0, desc.MipLevels, desc.SampleDesc.Count
        );
        match Presenter::new(q, &desc) {
            Ok(p) => *guard = Some(p),
            Err(e) => {
                static ONCE: AtomicBool = AtomicBool::new(false);
                if !ONCE.swap(true, Ordering::Relaxed) {
                    eprintln!("present   no swap chain: {e}");
                }
                return 0;
            }
        }
    }
    let pr = guard.as_mut().expect("presenter");
    pr.present(q, source, &desc);
    let output = pr.size;
    drop(guard);
    crate::framelog::presented(started.elapsed(), (desc.Width as u32, desc.Height), output);
    0
}

impl Presenter {
    fn new(q: &'static Queue, source: &D3D12_RESOURCE_DESC) -> Result<Presenter, String> {
        let device = crate::device::get().ok_or("no device")?.host.clone();
        let e = |what: &'static str| move |e: windows::core::Error| format!("{what}: {e}");
        let mode = crate::dxgi::host_mode();
        // the window's client size, and the swap chain's: a window keeps the title's own image size in its swap
        // chain (the plain copy path) and DXGI stretches it into the window at present (`DXGI_SCALING_STRETCH`)
        let (window_size, size) = match window_mode() {
            WindowMode::Borderless => {
                let s = (mode.width.max(1), mode.height.max(1));
                (s, s)
            }
            WindowMode::Windowed(w, h) => ((w, h), ((source.Width as u32).max(1), source.Height.max(1))),
        };
        let format = family(source.Format.0 as u32).unwrap_or(DXGI_FORMAT_R8G8B8A8_UNORM);
        let window = window(window_size, WINDOW_HIDDEN.load(Ordering::Relaxed))?;
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(e("DXGI factory"))?;
            let tearing = factory.cast::<IDXGIFactory5>().is_ok_and(|f5| {
                let mut allow = windows::Win32::Foundation::BOOL(0);
                f5.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &mut allow as *mut _ as *mut c_void, std::mem::size_of_val(&allow) as u32).is_ok() && allow.as_bool()
            });
            let flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32 | if tearing { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 };
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0,
                Height: size.1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: FRAMES as u32,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: flags,
                ..Default::default()
            };
            let swap: IDXGISwapChain1 = factory.CreateSwapChainForHwnd(&q.host, window, &desc, None, None).map_err(e("swap chain"))?;
            let _ = factory.MakeWindowAssociation(window, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES);
            let swap: IDXGISwapChain3 = swap.cast().map_err(e("IDXGISwapChain3"))?;
            swap.SetMaximumFrameLatency(2).map_err(e("frame latency"))?;
            let waitable = swap.GetFrameLatencyWaitableObject();
            if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                let _ = swap.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709);
            }
            let buffers: Vec<ID3D12Resource> = (0..FRAMES as u32).map(|i| swap.GetBuffer(i)).collect::<Result<_, _>>().map_err(e("swap chain buffers"))?;
            let rtv: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: FRAMES as u32, ..Default::default() })
                .map_err(e("RTV heap"))?;
            let rtv_step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
            for (i, b) in buffers.iter().enumerate() {
                let h = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: rtv.GetCPUDescriptorHandleForHeapStart().ptr + i * rtv_step };
                device.CreateRenderTargetView(b, None, h);
            }
            let srv: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: (FRAMES * FRAMES) as u32,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })
                .map_err(e("SRV heap"))?;
            let srv_step = u64::from(device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV));
            let queries: ID3D12QueryHeap = {
                let mut h: Option<ID3D12QueryHeap> = None;
                device
                    .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2 * FRAMES as u32, NodeMask: 0 }, &mut h)
                    .map_err(e("query heap"))?;
                h.ok_or("query heap")?
            };
            let stamps = buffer(&device, D3D12_HEAP_TYPE_READBACK, 16 * FRAMES as u64).map_err(e("timestamp buffer"))?;
            let mut cells: *mut c_void = std::ptr::null_mut();
            stamps.Map(0, None, Some(&mut cells)).map_err(e("timestamp map"))?;
            let frequency = q.host.GetTimestampFrequency().unwrap_or(0);
            let mut slots = Vec::with_capacity(FRAMES);
            for i in 0..FRAMES {
                let mut cache = Vec::with_capacity(FRAMES);
                for _ in 0..FRAMES {
                    let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(e("allocator"))?;
                    let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).map_err(e("list"))?;
                    list.Close().map_err(e("list"))?;
                    cache.push(Cached { allocator, list, key: None });
                }
                let ba: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(e("allocator"))?;
                let bl: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &ba, None).map_err(e("list"))?;
                bl.EndQuery(&queries, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i as u32);
                bl.Close().map_err(e("list"))?;
                BEGIN[i].store(bl.as_raw() as usize, Ordering::Relaxed);
                slots.push(Slot { cache, _begin: (ba, bl), done: 0, timed: false });
            }
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(e("fence"))?;
            TEARING.store(1 + usize::from(tearing), Ordering::Relaxed);
            PRESENT_QUEUE.store(q as *const Queue as usize, Ordering::Release);
            // the in-game menu: its input thread, font and pipeline are made off this thread
            crate::overlay::attach(&device, format, frequency, FRAMES);
            Ok(Presenter {
                device,
                queue: q,
                swap,
                waitable,
                _window: window,
                format,
                flags,
                tearing,
                size,
                buffers,
                rtv,
                rtv_step,
                srv,
                srv_step,
                slots,
                fence,
                value: 0,
                queries,
                stamps,
                stamp_cells: cells as *const u64,
                frequency,
                blit: None,
                shown: size,
                source_size_works: true,
                shown_frames: 0,
            })
        }
    }

    fn present(&mut self, from: &'static Queue, source: &ID3D12Resource, desc: &D3D12_RESOURCE_DESC) {
        let i = SLOT.load(Ordering::Relaxed);
        // the slot's last copy must be done, and the display must take a frame: else the frame is late
        if unsafe { self.fence.GetCompletedValue() } < self.slots[i].done || unsafe { WaitForSingleObject(self.waitable, 0) } != WAIT_OBJECT_0 {
            crate::framelog::late();
            return;
        }
        // the slot's last frame finished: its GPU span
        if self.slots[i].timed && self.frequency > 0 {
            // SAFETY: the mapped readback cells; the GPU wrote them before the fence passed.
            let (a, b) = unsafe { (self.stamp_cells.add(2 * i).read_volatile(), self.stamp_cells.add(2 * i + 1).read_volatile()) };
            if b > a {
                crate::framelog::gpu_span((b - a) * 1_000_000 / self.frequency);
            }
        }
        // work from another queue runs first
        if !std::ptr::eq(from, self.queue) {
            let v = from.progress_next.fetch_add(1, Ordering::AcqRel) + 1;
            unsafe {
                let _ = from.host.Signal(&from.progress, v);
                let _ = self.queue.host.Wait(&from.progress, v);
            }
        }
        let started = self.queue.frame_started.swap(false, Ordering::AcqRel);
        let frame = self.shown_frames + 1;
        let capture = CAPTURE.swap(false, Ordering::AcqRel) || capture_frames().contains(&frame);
        if capture {
            // REBUILD_TRACE_ENTRY: whether the traced textures' console memory holds texel data by now
            for (i, a) in rebuild_d3d12x::heap::traced_entries() {
                eprintln!("entry     {i} at frame {frame}: texture at {a:#x}: {}", rebuild_d3d12x::heap::nonzero(a, 64 << 10));
            }
        }
        // the overlay draws after the title's image; a capture is taken before it unless REBUILD_CAPTURE_OVERLAY=1
        let overlay = crate::overlay::visible();
        let capture_overlay = capture && overlay && crate::overlay::capture_overlay();
        let capture_main = capture && !capture_overlay;
        let (w, h) = (desc.Width as u32, desc.Height);
        let copy = family(desc.Format.0 as u32) == Some(self.format)
            && w <= self.size.0
            && h <= self.size.1
            && desc.SampleDesc.Count <= 1
            && desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
            && self.source_size_works;
        let back = unsafe { self.swap.GetCurrentBackBufferIndex() } as usize;
        let target = self.buffers[back].clone();
        let (allocator, list) = (self.slots[i].cache[back].allocator.clone(), self.slots[i].cache[back].list.clone());
        let list = &list;
        let mut readback = None;
        let shown_size = if copy { (w, h) } else { self.size };
        let cached = !capture_main && self.slots[i].cache[back].key.as_ref().is_some_and(|k| k.0 == *source && (k.1, k.2, k.3) == (w, h, copy));
        unsafe {
            if !cached {
                self.slots[i].cache[back].key = None;
                let _ = allocator.Reset();
                let _ = list.Reset(&allocator, None);
                let queries = &self.queries.clone();
                let shown = if copy {
                    list.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_COPY_DEST)]);
                    let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: borrowed(&target), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
                    let src = D3D12_TEXTURE_COPY_LOCATION { pResource: borrowed(source), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
                    let area = D3D12_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
                    list.CopyTextureRegion(&dst, 0, 0, 0, &src, Some(&area));
                    (w, h, D3D12_RESOURCE_STATE_COPY_DEST)
                } else {
                    self.draw(list, i * FRAMES + back, back, source, desc, &target);
                    (self.size.0, self.size.1, D3D12_RESOURCE_STATE_RENDER_TARGET)
                };
                if capture_main {
                    list.ResourceBarrier(&[transition(&target, shown.2, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    readback = self.capture(list, &target, (shown.0, shown.1));
                    list.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_PRESENT)]);
                } else {
                    list.ResourceBarrier(&[transition(&target, shown.2, D3D12_RESOURCE_STATE_PRESENT)]);
                }
                list.EndQuery(queries, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i as u32 + 1);
                list.ResolveQueryData(queries, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i as u32, 2, &self.stamps, 16 * i as u64);
                if list.Close().is_err() {
                    return;
                }
                if !capture_main {
                    self.slots[i].cache[back].key = Some((source.clone(), w, h, copy));
                }
            }
            // the in-game menu over the shown image: its own list, recorded only while something is shown
            crate::overlay::set_target(shown_size.0, shown_size.1);
            let mut over = None;
            if overlay {
                let rtv = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.rtv.GetCPUDescriptorHandleForHeapStart().ptr + back * self.rtv_step };
                if let Some(l) = crate::overlay::record(i, &target, D3D12_RESOURCE_STATE_PRESENT, rtv, self.fence.GetCompletedValue(), self.value + 1) {
                    if capture_overlay {
                        l.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                        readback = self.capture(&l, &target, shown_size);
                        l.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_PRESENT)]);
                    } else {
                        l.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT)]);
                    }
                    if l.Close().is_ok() {
                        over = Some(l);
                    }
                }
            }
            if capture_overlay && over.is_none() {
                readback = None;
                CAPTURE.store(true, Ordering::Release); // the overlay was not drawn: capture the next frame instead
            }
            // the frame's start timestamp in front when no submission of this frame put it there
            let begin = BEGIN[i].load(Ordering::Relaxed) as *mut c_void;
            let raw = [begin, list.as_raw(), over.as_ref().map_or(std::ptr::null_mut(), |l| l.as_raw())];
            let first = usize::from(started || begin.is_null());
            let count = if over.is_some() { 3 } else { 2 };
            (Interface::vtable(&self.queue.host).ExecuteCommandLists)(Interface::as_raw(&self.queue.host), (count - first) as u32, raw[first..].as_ptr() as *const _);
            let shown = shown_size;
            if (shown.0, shown.1) != self.shown {
                let s2: Option<IDXGISwapChain2> = self.swap.cast().ok();
                if s2.is_some_and(|s| s.SetSourceSize(shown.0, shown.1).is_ok()) {
                    self.shown = (shown.0, shown.1);
                } else if copy {
                    // the swap chain cannot show part of a buffer: scale with the blit from now on
                    self.source_size_works = false;
                }
            }
            let flip = Instant::now();
            let _ = if crate::frame::vsync() {
                self.swap.Present(1, DXGI_PRESENT(0))
            } else {
                self.swap.Present(0, if self.tearing { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) })
            };
            DXGI_NS.fetch_add(flip.elapsed().as_nanos() as u64, Ordering::Relaxed);
            self.value += 1;
            let _ = self.queue.host.Signal(&self.fence, self.value);
        }
        self.slots[i].done = self.value;
        DONE[i].store(self.value, Ordering::Relaxed);
        FENCE.store(self.fence.as_raw() as usize, Ordering::Relaxed);
        self.slots[i].timed = true;
        self.shown_frames = frame;
        SLOT.store((i + 1) % FRAMES, Ordering::Relaxed);
        if let Some((buffer, footprint)) = readback {
            write_capture(self.fence.clone(), self.value, buffer, footprint, self.format, frame);
        }
    }

    /// The blit: the source sampled over the whole buffer.
    unsafe fn draw(&mut self, list: &ID3D12GraphicsCommandList, descriptor: usize, back: usize, source: &ID3D12Resource, desc: &D3D12_RESOURCE_DESC, target: &ID3D12Resource) {
        if self.blit.is_none() {
            match blit(&self.device, self.format) {
                Ok(b) => self.blit = Some(b),
                Err(e) => {
                    static ONCE: AtomicBool = AtomicBool::new(false);
                    if !ONCE.swap(true, Ordering::Relaxed) {
                        eprintln!("present   no blit: {e}");
                    }
                }
            }
        }
        unsafe {
            list.ResourceBarrier(&[transition(target, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
            let Some(b) = &self.blit else { return };
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.srv.GetCPUDescriptorHandleForHeapStart().ptr + descriptor * self.srv_step as usize };
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: typed(desc.Format.0 as u32),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1, ..Default::default() } },
            };
            self.device.CreateShaderResourceView(source, Some(&view), cpu);
            let rtv = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.rtv.GetCPUDescriptorHandleForHeapStart().ptr + back * self.rtv_step };
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.SetDescriptorHeaps(&[Some(self.srv.clone())]);
            list.SetGraphicsRootSignature(&b.root);
            list.SetPipelineState(&b.pso);
            list.SetGraphicsRootDescriptorTable(0, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: self.srv.GetGPUDescriptorHandleForHeapStart().ptr + descriptor as u64 * self.srv_step });
            list.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: self.size.0 as f32, Height: self.size.1 as f32, MinDepth: 0.0, MaxDepth: 1.0 }]);
            list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: self.size.0 as i32, bottom: self.size.1 as i32 }]);
            list.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.DrawInstanced(3, 1, 0, 0);
        }
    }

    /// Copy the shown `size` of `target` (in COPY_SOURCE) to a new readback buffer.
    unsafe fn capture(&self, list: &ID3D12GraphicsCommandList, target: &ID3D12Resource, size: (u32, u32)) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
        let pitch = (size.0 * bytes_per_pixel(self.format)).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
        let buffer = buffer(&self.device, D3D12_HEAP_TYPE_READBACK, u64::from(pitch) * u64::from(size.1)).ok()?;
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: 0, Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: self.format, Width: size.0, Height: size.1, Depth: 1, RowPitch: pitch } };
        let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: borrowed(&buffer), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint } };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: borrowed(target), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
        let area = D3D12_BOX { left: 0, top: 0, front: 0, right: size.0, bottom: size.1, back: 1 };
        unsafe { list.CopyTextureRegion(&dst, 0, 0, 0, &src, Some(&area)) };
        Some((buffer, footprint))
    }
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: u64) -> windows::core::Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut r) }?;
    r.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY))
}

// ------------------------------------------------------------------------------------- the blit

const BLIT_HLSL: &str = r"
Texture2D<float4> source : register(t0);
SamplerState linear_clamp : register(s0);
float4 vs(uint id : SV_VertexID, out float2 uv : TEXCOORD0) : SV_Position {
    uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 position : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    return float4(source.SampleLevel(linear_clamp, uv, 0).rgb, 1);
}
";

fn compile(entry: &str, target: &str) -> Result<ID3DBlob, String> {
    let entry = std::ffi::CString::new(entry).map_err(|e| e.to_string())?;
    let target = std::ffi::CString::new(target).map_err(|e| e.to_string())?;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(BLIT_HLSL.as_ptr().cast(), BLIT_HLSL.len(), PCSTR::null(), None, None, PCSTR(entry.as_ptr().cast()), PCSTR(target.as_ptr().cast()), 0, 0, &mut code, Some(&mut errors))
    };
    match (r, code) {
        (Ok(()), Some(c)) => Ok(c),
        (r, _) => {
            let msg = errors.map(|e| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize())).into_owned() });
            Err(format!("compile {target:?}: {r:?} {}", msg.unwrap_or_default()))
        }
    }
}

fn blob(b: &ID3DBlob) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE { pShaderBytecode: unsafe { b.GetBufferPointer() }, BytecodeLength: unsafe { b.GetBufferSize() } }
}

fn blit(device: &ID3D12Device, format: DXGI_FORMAT) -> Result<Blit, String> {
    let vs = compile("vs", "vs_5_0")?;
    let ps = compile("ps", "ps_5_0")?;
    unsafe {
        let range = D3D12_DESCRIPTOR_RANGE { RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV, NumDescriptors: 1, ..Default::default() };
        let params = [D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 { DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE { NumDescriptorRanges: 1, pDescriptorRanges: &range } },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        }];
        let sampler = D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxLOD: D3D12_FLOAT32_MAX,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            ..Default::default()
        };
        let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: 1, pParameters: params.as_ptr(), NumStaticSamplers: 1, pStaticSamplers: &sampler, Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE };
        let mut serialized: Option<ID3DBlob> = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut serialized, None).map_err(|e| format!("root signature: {e}"))?;
        let serialized = serialized.ok_or("root signature")?;
        let root: ID3D12RootSignature = device
            .CreateRootSignature(0, std::slice::from_raw_parts(serialized.GetBufferPointer() as *const u8, serialized.GetBufferSize()))
            .map_err(|e| format!("root signature: {e}"))?;
        let mut pso = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            VS: blob(&vs),
            PS: blob(&ps),
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC { FillMode: D3D12_FILL_MODE_SOLID, CullMode: D3D12_CULL_MODE_NONE, DepthClipEnable: true.into(), ..Default::default() },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        pso.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        pso.RTVFormats[0] = format;
        let state: ID3D12PipelineState = device.CreateGraphicsPipelineState(&pso).map_err(|e| format!("blit pipeline: {e}"))?;
        Ok(Blit { root, pso: state })
    }
}

// ------------------------------------------------------------------------------------- capture to PNG

fn write_capture(fence: ID3D12Fence, value: u64, buffer: ID3D12Resource, footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT, format: DXGI_FORMAT, frame: u64) {
    struct Job(ID3D12Fence, ID3D12Resource);
    // SAFETY: free-threaded COM objects.
    unsafe impl Send for Job {}
    let job = Job(fence, buffer);
    let _ = std::thread::Builder::new().name("capture".into()).spawn(move || {
        let job = job;
        let (fence, buffer) = (&job.0, &job.1);
        unsafe {
            if let Ok(event) = CreateEventW(None, false, false, None) {
                // bounded: a GPU that never gets there loses the capture, not a thread
                let done = fence.SetEventOnCompletion(value, event).is_ok() && WaitForSingleObject(event, 5000) == WAIT_OBJECT_0;
                let _ = windows::Win32::Foundation::CloseHandle(event);
                if !done {
                    eprintln!("capture   frame {frame}: the GPU did not finish within 5 s; not written");
                    return;
                }
            }
        }
        let f = footprint.Footprint;
        let mut mapped: *mut c_void = std::ptr::null_mut();
        if unsafe { buffer.Map(0, None, Some(&mut mapped)) }.is_err() {
            return;
        }
        // SAFETY: the readback buffer holds RowPitch * Height bytes.
        let bytes = unsafe { std::slice::from_raw_parts(mapped as *const u8, f.RowPitch as usize * f.Height as usize) };
        let rgba = to_rgba8(bytes, f.Width, f.Height, f.RowPitch, format);
        unsafe { buffer.Unmap(0, None) };
        let dir = CAPTURE_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| {
            std::env::var_os("REBUILD_SESSION_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("captures")
        });
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("frame-{frame}.png"));
        match write_png(&path, f.Width, f.Height, &rgba) {
            Ok(()) => {
                eprintln!("capture   frame {frame} -> {}", path.display());
                CAPTURES.lock().unwrap_or_else(|e| e.into_inner()).push(path);
                if let Some(r) = CAPTURE_FILE.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = std::fs::remove_file(r);
                }
            }
            Err(e) => eprintln!("capture   frame {frame}: {e}"),
        }
    });
}

fn half(h: u16) -> f32 {
    let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = (h >> 10) & 0x1f;
    let m = f32::from(h & 0x3ff);
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m / 1024.0) * 2f32.powi(i32::from(e) - 15),
    }
}

fn srgb(linear: f32) -> u8 {
    let l = linear.clamp(0.0, 1.0);
    let v = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (v * 255.0 + 0.5) as u8
}

/// The shown pixels as 8-bit RGBA (alpha 255: the display ignores it).
pub fn to_rgba8(bytes: &[u8], w: u32, h: u32, pitch: u32, format: DXGI_FORMAT) -> Vec<u8> {
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = &bytes[y * pitch as usize..];
        for x in 0..w as usize {
            let px: [u8; 4] = if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                let c = |k: usize| half(u16::from_le_bytes([row[x * 8 + 2 * k], row[x * 8 + 2 * k + 1]]));
                [srgb(c(0)), srgb(c(1)), srgb(c(2)), 255]
            } else {
                let v = &row[x * 4..x * 4 + 4];
                if format == DXGI_FORMAT_B8G8R8A8_UNORM {
                    [v[2], v[1], v[0], 255]
                } else if format == DXGI_FORMAT_R10G10B10A2_UNORM {
                    let u = u32::from_le_bytes([v[0], v[1], v[2], v[3]]);
                    [((u & 0x3ff) >> 2) as u8, (((u >> 10) & 0x3ff) >> 2) as u8, (((u >> 20) & 0x3ff) >> 2) as u8, 255]
                } else {
                    [v[0], v[1], v[2], 255]
                }
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())
}

// ------------------------------------------------------------------------------------- the window

/// Posted by [`shutdown`]: the window thread destroys the window and ends.
const WM_GOODBYE: u32 = WM_APP + 1;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        crate::framelog::write_summary("window closed");
        shutdown();
        std::process::exit(0);
    }
    if msg == WM_GOODBYE {
        unsafe {
            let _ = DestroyWindow(hwnd);
            PostQuitMessage(0);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// How the title's window is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMode {
    /// A normal window with a frame, of this client size, shown without taking focus (the default).
    Windowed(u32, u32),
    /// A borderless window over the whole primary display.
    Borderless,
}

/// `REBUILD_WINDOW=windowed|borderless` (default windowed) and `REBUILD_WINDOW_SIZE=WxH` (default 1280x720).
pub fn window_mode() -> WindowMode {
    parse_window_mode(std::env::var("REBUILD_WINDOW").ok().as_deref(), std::env::var("REBUILD_WINDOW_SIZE").ok().as_deref())
}

fn parse_window_mode(mode: Option<&str>, size: Option<&str>) -> WindowMode {
    if mode.is_some_and(|m| m.trim().eq_ignore_ascii_case("borderless") || m.trim().eq_ignore_ascii_case("fullscreen")) {
        return WindowMode::Borderless;
    }
    let (w, h) = size
        .and_then(|s| {
            let (w, h) = s.trim().split_once(['x', 'X'])?;
            Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
        })
        .filter(|&(w, h)| (160..=16384).contains(&w) && (90..=16384).contains(&h))
        .unwrap_or((1280, 720));
    WindowMode::Windowed(w, h)
}

/// The title's window (borderless over the display, a framed window of the chosen client size, or a hidden one),
/// on its own thread that pumps its messages. A visible window is shown without taking focus.
fn window(size: (u32, u32), hidden: bool) -> Result<HWND, String> {
    let windowed = matches!(window_mode(), WindowMode::Windowed(..));
    let (tx, rx) = std::sync::mpsc::channel::<Result<usize, String>>();
    std::thread::Builder::new()
        .name("window".into())
        .stack_size(256 << 10)
        .spawn(move || unsafe {
            let class: Vec<u16> = "triumph-rebuild\0".encode_utf16().collect();
            let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let (ex, style, x, y, w, h) = if hidden {
                (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, WS_POPUP, -32000, -32000, 64, 64)
            } else if windowed {
                // a framed window whose client area is `size`; created hidden, then shown without activation
                let style = WS_OVERLAPPEDWINDOW;
                let mut r = RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
                let _ = AdjustWindowRectEx(&mut r, style, false, WS_EX_APPWINDOW);
                (WS_EX_APPWINDOW, style, CW_USEDEFAULT, CW_USEDEFAULT, r.right - r.left, r.bottom - r.top)
            } else {
                (WS_EX_APPWINDOW, WS_POPUP, 0, 0, size.0 as i32, size.1 as i32)
            };
            let title: Vec<u16> = "triumph-rebuild\0".encode_utf16().collect();
            let hwnd = CreateWindowExW(ex, PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr()), style, x, y, w, h, None, None, instance, None);
            match hwnd {
                Ok(hwnd) => {
                    // every visible kind is shown without stealing focus from what the user is doing
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    let _ = tx.send(Ok(hwnd.0 as usize));
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("window: {e}")));
                    return;
                }
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .map_err(|e| format!("window thread: {e}"))?;
    let hwnd = rx.recv().map_err(|e| format!("window thread: {e}"))??;
    Ok(HWND(hwnd as *mut c_void))
}

/// Before the process ends: release every memory wait the bridge holds (so no queue is left waiting), wait (at
/// most 5 s) for the present queue to finish, then release the swap chain and destroy the window. Presents after
/// this make a new swap chain.
pub fn shutdown() {
    crate::submit::release_all();
    let p = PRESENTER.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(p) = p else { return };
    PRESENT_QUEUE.store(0, Ordering::Release);
    FENCE.store(0, Ordering::Relaxed);
    for b in &BEGIN {
        b.store(0, Ordering::Relaxed);
    }
    unsafe {
        let v = p.value + 1;
        if p.queue.host.Signal(&p.fence, v).is_ok() {
            if let Ok(event) = CreateEventW(None, false, false, None) {
                if p.fence.SetEventOnCompletion(v, event).is_ok() && WaitForSingleObject(event, 5000) != WAIT_OBJECT_0 {
                    eprintln!("present   the GPU did not finish within 5 s at shutdown");
                }
                let _ = windows::Win32::Foundation::CloseHandle(event);
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(p.waitable);
        let window = p._window;
        drop(p);
        let _ = PostMessageW(window, WM_GOODBYE, WPARAM(0), LPARAM(0));
    }
}

/// Presents so far: shown, late, found the presenter busy.
pub fn counts() -> (u64, u64) {
    let shown = PRESENTER.lock().ok().and_then(|p| p.as_ref().map(|p| p.shown_frames)).unwrap_or(0);
    (shown, BUSY.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_pick_a_swap_chain_of_their_family() {
        assert_eq!(family(28), Some(DXGI_FORMAT_R8G8B8A8_UNORM));
        assert_eq!(family(29), Some(DXGI_FORMAT_R8G8B8A8_UNORM), "sRGB bytes copy unchanged");
        assert_eq!(family(27), Some(DXGI_FORMAT_R8G8B8A8_UNORM), "the host texture is typeless");
        assert_eq!(family(91), Some(DXGI_FORMAT_B8G8R8A8_UNORM));
        assert_eq!(family(24), Some(DXGI_FORMAT_R10G10B10A2_UNORM));
        assert_eq!(family(10), Some(DXGI_FORMAT_R16G16B16A16_FLOAT));
        assert_eq!(family(26), None, "R11G11B10: the blit");
        assert_eq!(typed(27), DXGI_FORMAT_R8G8B8A8_UNORM);
        assert_eq!(typed(26), DXGI_FORMAT(26));
    }

    #[test]
    fn pixels_convert_to_rgba8() {
        let px = |f, b: &[u8]| to_rgba8(b, 1, 1, 256, f)[..4].to_vec();
        let mut row = vec![0u8; 256];
        row[..4].copy_from_slice(&[10, 20, 30, 40]);
        assert_eq!(px(DXGI_FORMAT_R8G8B8A8_UNORM, &row), [10, 20, 30, 255]);
        assert_eq!(px(DXGI_FORMAT_B8G8R8A8_UNORM, &row), [30, 20, 10, 255]);
        row[..4].copy_from_slice(&(1023u32 | (512 << 10) | (0 << 20)).to_le_bytes());
        assert_eq!(px(DXGI_FORMAT_R10G10B10A2_UNORM, &row), [255, 128, 0, 255]);
        // 1.0, 0.5 linear (sRGB 188), 0.0
        row[..8].copy_from_slice(&[0x00, 0x3c, 0x00, 0x38, 0x00, 0x00, 0x00, 0x3c]);
        assert_eq!(px(DXGI_FORMAT_R16G16B16A16_FLOAT, &row), [255, 188, 0, 255]);
        assert_eq!(half(0x3c00), 1.0);
        assert_eq!(half(0xc000), -2.0);
    }
}

#[cfg(test)]
mod window_mode_tests {
    use super::{parse_window_mode, WindowMode};

    #[test]
    fn the_window_is_a_720p_window_unless_asked_otherwise() {
        assert_eq!(parse_window_mode(None, None), WindowMode::Windowed(1280, 720));
        assert_eq!(parse_window_mode(Some("windowed"), Some("1600x900")), WindowMode::Windowed(1600, 900));
        assert_eq!(parse_window_mode(Some("Borderless"), Some("1600x900")), WindowMode::Borderless);
        assert_eq!(parse_window_mode(Some("fullscreen"), None), WindowMode::Borderless);
        // unreadable or absurd sizes fall back to 720p
        assert_eq!(parse_window_mode(None, Some("huge")), WindowMode::Windowed(1280, 720));
        assert_eq!(parse_window_mode(None, Some("10x10")), WindowMode::Windowed(1280, 720));
        assert_eq!(parse_window_mode(None, Some(" 1920 X 1080 ")), WindowMode::Windowed(1920, 1080));
    }
}
