//! Our implementation of the FidelityFX API the title imports (Build 7).
//!
//! The title's exe imports `ffxCreateContext`, `ffxDestroyContext`, `ffxDispatch` and `ffxQuery` from
//! `amd_fidelityfx_loader_xbox.dll`; that loader hands every upscale descriptor to
//! `amd_fidelityfx_upscaler_xbox.dll`, AMD's FSR 4 for Xbox, whose pipelines are console code (device
//! slot 0x218, which fails on purpose). The runtime serves both DLL names from here instead
//! (`crates/runtime/src/exports.rs`, `CONSOLE_DLLS`), so the title's temporal upscale arrives with every
//! input named: colour, depth, motion vectors, exposure, output, jitter, motion-vector scale, render and
//! output size, reset, near/far/fov. Research and layouts: `docs/research/ffx.md`.
//!
//! ```text
//! ffxCreateContext(UPSCALE)   a Context: the creation flags and sizes, and an Upscaler from the Host
//!                             (DLSS through Streamline on NVIDIA, FSR 3.1 through the PC FidelityFX SDK)
//! ffxDispatch(UPSCALE)        translate(): FFX's conventions to rebuild-upscale's Inputs; the console
//!                             resources to host ones (Host::resource, i.e. d3d12x host_resource); the
//!                             upscale recorded into the host list the console list records into now
//!                             (Host::record), with the barriers to and from the states the title gave
//! ffxQuery                    jitter phase count and offsets (FSR's rules exactly), render size per
//!                             quality mode, GPU memory, versions
//! ffxDestroyContext           the Context and its Upscaler go
//! ffxConfigure                debug callback and key/values: accepted, nothing to set
//! ```
//!
//! Nothing here runs the title's or AMD's Xbox code. The Host is installed by the runtime when the
//! device exists ([`install`]); a create before that fails with `FFX_API_RETURN_ERROR_RUNTIME_ERROR`.

#![cfg(windows)]

pub mod api;
pub mod backends;
pub mod host;

#[cfg(test)]
mod tests;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebuild_upscale::{Inputs, Mode, Signs, Size};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

pub use host::{Backend, Host, Wanted};

/// A pointer the title hands us, borrowed.
pub type Raw = *mut c_void;

/// The DLL names we serve (the title's own copies are not mapped: `REPLACED_TITLE_DLLS` in the runtime).
pub const DLLS: [&str; 2] = ["amd_fidelityfx_loader_xbox.dll", "amd_fidelityfx_upscaler_xbox.dll"];

static HOST: RwLock<Option<Arc<dyn Host>>> = RwLock::new(None);

/// The runtime's side, once the device exists. A later call replaces it (contexts keep the one they
/// were created with).
pub fn install(host: Arc<dyn Host>) {
    *HOST.write().unwrap_or_else(|e| e.into_inner()) = Some(host);
}

fn current_host() -> Option<Arc<dyn Host>> {
    HOST.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Whether the runtime should serve the FFX DLLs from here. `REBUILD_FFX=title` keeps the title's own
/// DLLs (for comparison); anything else, or nothing, serves ours.
pub fn enabled() -> bool {
    !matches!(std::env::var("REBUILD_FFX").as_deref(), Ok("title"))
}

/// The address of our implementation of `name`, for either DLL name.
pub fn export(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "ffxCreateContext" => ffxCreateContext as *const (),
        "ffxDestroyContext" => ffxDestroyContext as *const (),
        "ffxConfigure" => ffxConfigure as *const (),
        "ffxQuery" => ffxQuery as *const (),
        "ffxDispatch" => ffxDispatch as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

// ------------------------------------------------------------------------------------------ counts

/// Counts over the run, for the frame line and the tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub contexts: u64,
    pub live: u64,
    pub dispatches: u64,
    /// dispatches recorded into a host list
    pub recorded: u64,
    pub queries: u64,
    /// calls answered with an error code
    pub refused: u64,
}

static CONTEXTS: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static DISPATCHES: AtomicU64 = AtomicU64::new(0);
static RECORDED: AtomicU64 = AtomicU64::new(0);
static QUERIES: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static FIRST_DISPATCH_LOGGED: AtomicBool = AtomicBool::new(false);

pub fn stats() -> Stats {
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    Stats { contexts: r(&CONTEXTS), live: r(&LIVE), dispatches: r(&DISPATCHES), recorded: r(&RECORDED), queries: r(&QUERIES), refused: r(&REFUSED) }
}

/// One log line per distinct problem (by key), then silence: the title calls these every frame.
fn once(key: &'static str, msg: impl FnOnce() -> String) {
    static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&key) {
        seen.push(key);
        eprintln!("ffx: {}", msg());
    }
}

fn refuse(code: u32) -> u32 {
    REFUSED.fetch_add(1, Ordering::Relaxed);
    code
}

// ------------------------------------------------------------------------------------- the context

const MAGIC: u64 = u64::from_le_bytes(*b"rbffxctx");

/// What the title asked for at creation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Created {
    pub flags: u32,
    pub max_render: api::Dim,
    pub max_upscale: api::Dim,
    /// `ffxCreateContextDescUpscaleVersion::version` (the title sends 4.1.1), 0 if absent.
    pub version: u32,
    /// the backend description's type and device (the title's console device), for the log
    pub backend: u64,
}

/// The object behind an `ffxContext` we hand out.
pub struct Context {
    magic: u64,
    pub created: Created,
    host: Arc<dyn Host>,
    upscaler: Backend,
    message: api::Message,
}

unsafe fn context<'a>(c: *mut Raw) -> Option<&'a mut Context> {
    if c.is_null() {
        return None;
    }
    let p = unsafe { *c } as *mut Context;
    // SAFETY: only pointers we handed out carry the magic; anything else is refused.
    (!p.is_null() && unsafe { (*p).magic } == MAGIC).then(|| unsafe { &mut *p })
}

/// Walk a descriptor chain.
unsafe fn chain(mut h: *const api::Header) -> impl Iterator<Item = *const api::Header> {
    std::iter::from_fn(move || {
        if h.is_null() {
            return None;
        }
        let now = h;
        h = unsafe { (*h).next };
        Some(now)
    })
}

/// The creation chain, read (pure: the unit tests feed it hand-built chains).
pub unsafe fn read_create(desc: *const api::Header) -> Result<(Created, api::Message), (u32, String)> {
    let mut upscale = None;
    let mut c = Created::default();
    for h in unsafe { chain(desc) } {
        let kind = unsafe { (*h).kind };
        match kind {
            api::CREATE_UPSCALE => {
                let d = unsafe { &*(h as *const api::CreateUpscale) };
                upscale = Some(d.message);
                c.flags = d.flags;
                c.max_render = d.max_render;
                c.max_upscale = d.max_upscale;
            }
            api::CREATE_UPSCALE_VERSION => c.version = unsafe { (*(h as *const api::CreateUpscaleVersion)).version },
            k if k & !api::BACKEND_MASK == api::BACKEND_DEVICE_SUB => c.backend = k,
            k if k & !api::BACKEND_MASK == api::BACKEND_ALLOCATION_SUB || k == api::DESC_OVERRIDE_VERSION => {}
            k => once("create-unknown", || format!("ffxCreateContext: descriptor type {k:#x} in the chain ignored")),
        }
    }
    let Some(message) = upscale else {
        let first = if desc.is_null() { 0 } else { unsafe { (*desc).kind } };
        return Err((api::ERROR_UNKNOWN_DESCTYPE, format!("ffxCreateContext: no upscale description (first type {first:#x})")));
    };
    if c.max_upscale.width == 0 || c.max_upscale.height == 0 || c.max_render.width == 0 || c.max_render.height == 0 {
        return Err((api::ERROR_PARAMETER, format!("ffxCreateContext: zero size (render {:?}, upscale {:?})", c.max_render, c.max_upscale)));
    }
    Ok((c, message))
}

// --------------------------------------------------------------------------------- the translation

/// One resource of a dispatch: the title's pointer and the state it says the resource is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Res {
    pub resource: Raw,
    /// FFX state bits (`api::STATE_*`)
    pub state: u32,
    pub format: u32,
}

impl Res {
    fn of(r: &api::Resource) -> Res {
        Res { resource: r.resource, state: r.state, format: r.description.format }
    }
}

/// What the dispatch descriptor says, in `rebuild-upscale`'s convention (pure).
#[derive(Clone, Debug, PartialEq)]
pub struct Translated {
    pub list: Raw,
    pub colour: Res,
    pub depth: Res,
    pub motion: Res,
    pub exposure: Option<Res>,
    pub output: Res,
    pub render: Size,
    pub output_size: Size,
    pub mode: Mode,
    /// the layer's jitter: the sample offset from the pixel centre, +y down (FFX's jitterOffset is its
    /// negation: `Signs::FIDELITYFX`, measured)
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub depth_reversed: bool,
    pub hdr: bool,
    pub reset: bool,
    pub pre_exposure: f32,
    pub frame_ms: f32,
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
    /// inputs the title gave that the Upscaler interface does not carry yet (`DROPPED_*`)
    pub dropped: u32,
}

pub const DROPPED_REACTIVE: u32 = 1 << 0;
pub const DROPPED_TRANSPARENCY: u32 = 1 << 1;
pub const DROPPED_SHARPENING: u32 = 1 << 2;
pub const DROPPED_AUTO_EXPOSURE: u32 = 1 << 3;
pub const DROPPED_DISPLAY_RES_MV: u32 = 1 << 4;
pub const DROPPED_JITTERED_MV: u32 = 1 << 5;

/// The quality mode whose ratio is nearest output/render (the backends take a mode; DLSS sets its
/// options from it).
pub fn mode_for(render: Size, output: Size) -> Mode {
    let r = output.width as f32 / render.width.max(1) as f32;
    Mode::ALL.into_iter().min_by(|a, b| (a.ratio() - r).abs().total_cmp(&(b.ratio() - r).abs())).unwrap_or(Mode::Quality)
}

/// A dispatch descriptor in the layer's terms.
pub fn translate(d: &api::DispatchUpscale, c: &Created) -> Result<Translated, (u32, String)> {
    for (name, r) in [("color", &d.color), ("depth", &d.depth), ("motionVectors", &d.motion_vectors), ("output", &d.output)] {
        if r.is_null() {
            return Err((api::ERROR_PARAMETER, format!("ffxDispatch: {name} is null")));
        }
    }
    if d.command_list.is_null() {
        return Err((api::ERROR_PARAMETER, "ffxDispatch: commandList is null".into()));
    }
    let render = Size::new(d.render_size.width, d.render_size.height);
    let up = if d.upscale_size.width == 0 || d.upscale_size.height == 0 { c.max_upscale } else { d.upscale_size };
    let output_size = Size::new(up.width, up.height);
    if render.width == 0 || render.height == 0 {
        return Err((api::ERROR_PARAMETER, "ffxDispatch: renderSize has a zero dimension".into()));
    }
    let s = Signs::FIDELITYFX;
    let mut dropped = 0;
    if !d.reactive.is_null() {
        dropped |= DROPPED_REACTIVE;
    }
    if !d.transparency_and_composition.is_null() {
        dropped |= DROPPED_TRANSPARENCY;
    }
    if d.enable_sharpening && d.sharpness > 0.0 {
        dropped |= DROPPED_SHARPENING;
    }
    if c.flags & api::ENABLE_AUTO_EXPOSURE != 0 {
        dropped |= DROPPED_AUTO_EXPOSURE;
    }
    if c.flags & api::ENABLE_DISPLAY_RES_MV != 0 {
        dropped |= DROPPED_DISPLAY_RES_MV;
    }
    if c.flags & api::ENABLE_MV_JITTER_CANCELLATION != 0 {
        dropped |= DROPPED_JITTERED_MV;
    }
    Ok(Translated {
        list: d.command_list,
        colour: Res::of(&d.color),
        depth: Res::of(&d.depth),
        motion: Res::of(&d.motion_vectors),
        // with auto exposure the title passes none; an exposure resource always wins
        exposure: (!d.exposure.is_null()).then(|| Res::of(&d.exposure)),
        output: Res::of(&d.output),
        render,
        output_size,
        mode: mode_for(render, output_size),
        jitter: [d.jitter_offset.x / s.jitter[0], d.jitter_offset.y / s.jitter[1]],
        motion_scale: [d.motion_vector_scale.x / s.motion[0], d.motion_vector_scale.y / s.motion[1]],
        depth_reversed: c.flags & api::ENABLE_DEPTH_INVERTED != 0,
        hdr: c.flags & api::ENABLE_HDR != 0,
        reset: d.reset,
        pre_exposure: if d.pre_exposure > 0.0 && d.pre_exposure.is_finite() { d.pre_exposure } else { 1.0 },
        frame_ms: if d.frame_time_delta.is_finite() { d.frame_time_delta } else { 16.7 },
        near: d.camera_near,
        far: d.camera_far,
        fov_y: d.camera_fov_angle_vertical,
        dropped,
    })
}

/// FFX resource state bits as a D3D12 state.
pub fn d3d_state(ffx: u32) -> D3D12_RESOURCE_STATES {
    let mut s = D3D12_RESOURCE_STATE_COMMON;
    let mut add = |bit: u32, d: D3D12_RESOURCE_STATES| {
        if ffx & bit != 0 {
            s |= d;
        }
    };
    add(api::STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    add(api::STATE_COMPUTE_READ, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    add(api::STATE_PIXEL_READ, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE);
    add(api::STATE_COPY_SRC, D3D12_RESOURCE_STATE_COPY_SOURCE);
    add(api::STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_DEST);
    add(api::STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    add(api::STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_RENDER_TARGET);
    add(api::STATE_DEPTH_ATTACHMENT, D3D12_RESOURCE_STATE_DEPTH_WRITE);
    // COMMON and PRESENT are both 0 in D3D12
    s
}

/// What the Upscaler interface wants: inputs readable by any shader, the output unordered access.
pub const SRV: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0);

fn transition(r: &ID3D12Resource, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: borrowed for the call; the ManuallyDrop never releases it
                pResource: unsafe { std::mem::transmute_copy(r) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: from,
                StateAfter: to,
            }),
        },
    }
}

/// Record one translated dispatch into `list` with `upscaler`: barriers to the interface's states, the
/// upscale, barriers back to the states the title gave.
pub fn record(list: &ID3D12GraphicsCommandList, upscaler: &mut Backend, t: &Translated, res: &[ID3D12Resource; 5]) -> Result<(), String> {
    let [colour, depth, motion, output, exposure] = res;
    let mut ins: Vec<(&ID3D12Resource, D3D12_RESOURCE_STATES, D3D12_RESOURCE_STATES)> = vec![
        (colour, d3d_state(t.colour.state), SRV),
        (depth, d3d_state(t.depth.state), SRV),
        (motion, d3d_state(t.motion.state), SRV),
        (output, d3d_state(t.output.state), D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
    ];
    if let Some(e) = t.exposure {
        ins.push((exposure, d3d_state(e.state), SRV));
    }
    let to: Vec<D3D12_RESOURCE_BARRIER> = ins.iter().filter(|(_, a, b)| a != b).map(|(r, a, b)| transition(r, *a, *b)).collect();
    let back: Vec<D3D12_RESOURCE_BARRIER> = ins.iter().filter(|(_, a, b)| a != b).map(|(r, a, b)| transition(r, *b, *a)).collect();
    if !to.is_empty() {
        unsafe { list.ResourceBarrier(&to) };
    }
    let i = Inputs {
        colour,
        depth,
        motion,
        exposure: t.exposure.map(|_| exposure),
        output,
        render: t.render,
        output_size: t.output_size,
        jitter: t.jitter,
        motion_scale: t.motion_scale,
        depth_reversed: t.depth_reversed,
        reset: t.reset,
        hdr: t.hdr,
        pre_exposure: t.pre_exposure,
        frame_ms: t.frame_ms,
        near: t.near,
        far: t.far,
        fov_y: t.fov_y,
    };
    let r = upscaler.evaluate(list, t.mode, &i);
    if !back.is_empty() {
        unsafe { list.ResourceBarrier(&back) };
    }
    r
}

fn describe(d: &api::DispatchUpscale, c: &Created, ours: Option<bool>) -> String {
    let r = |name: &str, r: &api::Resource| {
        if r.is_null() {
            format!("{name} -")
        } else {
            format!("{name} {}x{} fmt {} state {:#x}", r.description.width, r.description.height, r.description.format, r.state)
        }
    };
    format!(
        "first dispatch: list {:p} (a console list of ours: {}); {}; {}; {}; {}; {}; {}; {}; jitter ({:+.4}, {:+.4}) mv scale ({}, {}); render {}x{} -> {}x{}; sharpen {} {:.3}; dt {:.2} ms; pre-exposure {}; reset {}; near {} far {} fov {:.4}; m/vs {}; flags {:#x} | context flags {:#x}, max render {}x{}, max upscale {}x{}, api {}",
        d.command_list,
        ours.map_or("?", |o| if o { "yes" } else { "no" }),
        r("colour", &d.color),
        r("depth", &d.depth),
        r("motion", &d.motion_vectors),
        r("exposure", &d.exposure),
        r("reactive", &d.reactive),
        r("t&c", &d.transparency_and_composition),
        r("output", &d.output),
        d.jitter_offset.x,
        d.jitter_offset.y,
        d.motion_vector_scale.x,
        d.motion_vector_scale.y,
        d.render_size.width,
        d.render_size.height,
        d.upscale_size.width,
        d.upscale_size.height,
        d.enable_sharpening,
        d.sharpness,
        d.frame_time_delta,
        d.pre_exposure,
        d.reset,
        d.camera_near,
        d.camera_far,
        d.camera_fov_angle_vertical,
        d.view_space_to_meters_factor,
        d.flags,
        c.flags,
        c.max_render.width,
        c.max_render.height,
        c.max_upscale.width,
        c.max_upscale.height,
        api::version_name(c.version),
    )
}

// ------------------------------------------------------------------------------- the entry points

/// `ffxCreateContext(context, desc, memCb)`.
#[no_mangle]
pub unsafe extern "C" fn ffxCreateContext(out: *mut Raw, desc: *mut api::Header, _alloc: *const c_void) -> u32 {
    if out.is_null() || desc.is_null() {
        return refuse(api::ERROR_PARAMETER);
    }
    unsafe { *out = std::ptr::null_mut() };
    let (created, message) = match unsafe { read_create(desc) } {
        Ok(c) => c,
        Err((code, e)) => {
            eprintln!("ffx: {e}");
            return refuse(code);
        }
    };
    let Some(host) = current_host() else {
        once("no-host", || "ffxCreateContext before the device exists (no Host installed): refused".into());
        return refuse(api::ERROR_RUNTIME_ERROR);
    };
    let want = Wanted {
        max_render: Size::new(created.max_render.width, created.max_render.height),
        max_output: Size::new(created.max_upscale.width, created.max_upscale.height),
        hdr: created.flags & api::ENABLE_HDR != 0,
        depth_reversed: created.flags & api::ENABLE_DEPTH_INVERTED != 0,
    };
    let upscaler = match host.upscaler(&want) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("ffx: ffxCreateContext: no upscaler: {e}");
            return refuse(api::ERROR_RUNTIME_ERROR);
        }
    };
    eprintln!(
        "ffx: context: flags {:#x}, max render {}x{}, max upscale {}x{}, api {}, backend desc {:#x} -> {}",
        created.flags,
        created.max_render.width,
        created.max_render.height,
        created.max_upscale.width,
        created.max_upscale.height,
        api::version_name(created.version),
        created.backend,
        upscaler.name()
    );
    let c = Box::new(Context { magic: MAGIC, created, host, upscaler, message });
    unsafe { *out = Box::into_raw(c).cast() };
    CONTEXTS.fetch_add(1, Ordering::Relaxed);
    LIVE.fetch_add(1, Ordering::Relaxed);
    api::OK
}

/// `ffxDestroyContext(context, memCb)`. The title waits for the GPU before it destroys (the FFX
/// contract); the Upscaler releases its resources here.
#[no_mangle]
pub unsafe extern "C" fn ffxDestroyContext(c: *mut Raw, _alloc: *const c_void) -> u32 {
    let Some(ctx) = (unsafe { context(c) }) else { return refuse(api::ERROR_PARAMETER) };
    ctx.magic = 0;
    // SAFETY: we made it with Box::into_raw; the magic is cleared so a second destroy is refused
    drop(unsafe { Box::from_raw(ctx as *mut Context) });
    unsafe { *c = std::ptr::null_mut() };
    LIVE.fetch_sub(1, Ordering::Relaxed);
    api::OK
}

/// `ffxConfigure(context, desc)`: the debug callback and FSR's key/values are accepted; nothing of
/// ours takes them.
#[no_mangle]
pub unsafe extern "C" fn ffxConfigure(c: *mut Raw, desc: *const api::Header) -> u32 {
    if desc.is_null() {
        return refuse(api::ERROR_PARAMETER);
    }
    match unsafe { (*desc).kind } {
        api::CONFIGURE_GLOBALDEBUG => {
            if let Some(ctx) = unsafe { context(c) } {
                ctx.message = unsafe { (*(desc as *const api::ConfigureGlobalDebug)).message };
            }
            api::OK
        }
        api::CONFIGURE_UPSCALE_KEYVALUE => api::OK,
        k => {
            once("configure-unknown", || format!("ffxConfigure: descriptor type {k:#x} not known"));
            refuse(api::ERROR_UNKNOWN_DESCTYPE)
        }
    }
}

/// FSR's jitter phase count: `int(8 * (display / render)^2)` (`ffxFsr3GetJitterPhaseCount`).
pub fn phase_count(render_width: u32, display_width: u32) -> i32 {
    if render_width == 0 {
        return 0;
    }
    let r = display_width as f32 / render_width as f32;
    (8.0 * r * r) as i32
}

const PROVIDER_ID: u64 = u64::from_le_bytes(*b"rebuild\0");
const PROVIDER_NAME: &[u8] = b"rebuild (DLSS or FSR 3.1 behind FFX)\0";

/// `ffxQuery(context, desc)`.
#[no_mangle]
pub unsafe extern "C" fn ffxQuery(c: *mut Raw, desc: *mut api::Header) -> u32 {
    QUERIES.fetch_add(1, Ordering::Relaxed);
    if desc.is_null() {
        return refuse(api::ERROR_PARAMETER);
    }
    let kind = unsafe { (*desc).kind };
    unsafe {
        match kind {
            api::QUERY_UPSCALE_RATIO => {
                let q = &*(desc as *const api::QueryUpscaleRatio);
                let Some(m) = Mode::ALL.get(q.quality_mode as usize) else { return refuse(api::ERROR_PARAMETER) };
                if let Some(o) = q.out_ratio.as_mut() {
                    *o = m.ratio();
                }
                api::OK
            }
            api::QUERY_RENDER_RESOLUTION => {
                let q = &*(desc as *const api::QueryRenderResolution);
                let Some(&m) = Mode::ALL.get(q.quality_mode as usize) else { return refuse(api::ERROR_PARAMETER) };
                let out = Size::new(q.display_width, q.display_height);
                // the backend's own answer when there is a context (DLSS and FSR differ at quality)
                let s = context(c).and_then(|ctx| ctx.upscaler.render_size(out, m).ok()).unwrap_or_else(|| m.render_size(out));
                if let Some(w) = q.out_width.as_mut() {
                    *w = s.width;
                }
                if let Some(h) = q.out_height.as_mut() {
                    *h = s.height;
                }
                api::OK
            }
            api::QUERY_JITTER_PHASE_COUNT => {
                let q = &*(desc as *const api::QueryJitterPhaseCount);
                if let Some(o) = q.out_phase_count.as_mut() {
                    *o = phase_count(q.render_width, q.display_width);
                }
                api::OK
            }
            api::QUERY_JITTER_OFFSET => {
                let q = &*(desc as *const api::QueryJitterOffset);
                if q.phase_count <= 0 {
                    return refuse(api::ERROR_PARAMETER);
                }
                // FSR's: Halton(2,3) - 0.5 at (index mod phases) + 1, which is rebuild_upscale::jitter
                let j = rebuild_upscale::jitter(q.index.rem_euclid(q.phase_count) as u32, q.phase_count as u32);
                if let Some(x) = q.out_x.as_mut() {
                    *x = j[0];
                }
                if let Some(y) = q.out_y.as_mut() {
                    *y = j[1];
                }
                api::OK
            }
            api::QUERY_GPU_MEMORY => {
                let q = &*(desc as *const api::QueryGpuMemory);
                if let Some(u) = q.usage.as_mut() {
                    *u = api::MemoryUsage::default();
                }
                api::OK
            }
            api::QUERY_GPU_MEMORY_V2 => {
                let q = &*(desc as *const api::QueryGpuMemoryV2);
                if let Some(u) = q.usage.as_mut() {
                    *u = api::MemoryUsage::default();
                }
                api::OK
            }
            api::QUERY_RESOURCE_REQUIREMENTS => {
                let q = &mut *(desc as *mut api::QueryResourceRequirements);
                q.required = api::RESOURCE_COLOR | api::RESOURCE_DEPTH | api::RESOURCE_MV;
                let auto = context(c).is_some_and(|ctx| ctx.created.flags & api::ENABLE_AUTO_EXPOSURE != 0);
                if !auto {
                    q.optional = api::RESOURCE_EXPOSURE;
                }
                api::OK
            }
            api::QUERY_GET_VERSIONS => {
                let q = &*(desc as *const api::QueryVersions);
                if q.create_desc_type != api::CREATE_UPSCALE {
                    if let Some(n) = q.output_count.as_mut() {
                        *n = 0;
                    }
                    return api::OK;
                }
                let cap = q.output_count.as_ref().copied().unwrap_or(0);
                if let Some(n) = q.output_count.as_mut() {
                    *n = 1;
                }
                if cap >= 1 {
                    if let Some(id) = q.version_ids.as_mut() {
                        *id = PROVIDER_ID;
                    }
                    if let Some(name) = q.version_names.as_mut() {
                        *name = PROVIDER_NAME.as_ptr().cast();
                    }
                }
                api::OK
            }
            api::QUERY_GET_PROVIDER_VERSION => {
                let q = &mut *(desc as *mut api::QueryProviderVersion);
                if context(c).is_none() {
                    return refuse(api::ERROR_PARAMETER);
                }
                q.version_id = PROVIDER_ID;
                q.version_name = PROVIDER_NAME.as_ptr().cast();
                api::OK
            }
            k => {
                once("query-unknown", || format!("ffxQuery: descriptor type {k:#x} not known"));
                refuse(api::ERROR_UNKNOWN_DESCTYPE)
            }
        }
    }
}

/// `ffxDispatch(context, desc)`.
#[no_mangle]
pub unsafe extern "C" fn ffxDispatch(c: *mut Raw, desc: *const api::Header) -> u32 {
    if desc.is_null() {
        return refuse(api::ERROR_PARAMETER);
    }
    let Some(ctx) = (unsafe { context(c) }) else { return refuse(api::ERROR_PARAMETER) };
    match unsafe { (*desc).kind } {
        api::DISPATCH_UPSCALE => {}
        api::DISPATCH_REACTIVE_MASK => {
            once("reactive-mask", || "ffxDispatch: GENERATEREACTIVEMASK is not built (the title was not seen calling it)".into());
            return refuse(api::ERROR_UNKNOWN_DESCTYPE);
        }
        k => {
            once("dispatch-unknown", || format!("ffxDispatch: descriptor type {k:#x} not known"));
            return refuse(api::ERROR_UNKNOWN_DESCTYPE);
        }
    }
    DISPATCHES.fetch_add(1, Ordering::Relaxed);
    let d = unsafe { &*(desc as *const api::DispatchUpscale) };
    if !FIRST_DISPATCH_LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("ffx: {}", describe(d, &ctx.created, ctx.host.is_list(d.command_list)));
    }
    let t = match translate(d, &ctx.created) {
        Ok(t) => t,
        Err((code, e)) => {
            once("dispatch-refused", || e);
            return refuse(code);
        }
    };
    if t.dropped != 0 {
        once("dispatch-dropped", || format!("ffxDispatch: inputs not carried by the Upscaler interface yet: {:#x} (reactive 1, t&c 2, sharpening 4, auto exposure 8, display-res mv 0x10, jittered mv 0x20)", t.dropped));
    }
    let look = |r: &Res, what: &'static str| ctx.host.resource(r.resource).ok_or(what);
    let res = (|| -> Result<[ID3D12Resource; 5], &'static str> {
        let colour = look(&t.colour, "color")?;
        let exposure = match &t.exposure {
            Some(e) => look(e, "exposure")?,
            None => colour.clone(),
        };
        Ok([colour, look(&t.depth, "depth")?, look(&t.motion, "motionVectors")?, look(&t.output, "output")?, exposure])
    })();
    let res = match res {
        Ok(r) => r,
        Err(what) => {
            once("dispatch-resource", || format!("ffxDispatch: the {what} resource has no host resource"));
            return refuse(api::ERROR_PARAMETER);
        }
    };
    let host = ctx.host.clone();
    let upscaler = &mut ctx.upscaler;
    match host.record(t.list, &mut |list| record(list, upscaler, &t, &res)) {
        Ok(()) => {
            RECORDED.fetch_add(1, Ordering::Relaxed);
            api::OK
        }
        Err(e) => {
            once("dispatch-record", || format!("ffxDispatch: {e}"));
            refuse(api::ERROR_RUNTIME_ERROR)
        }
    }
}

/// The interface pointer as a borrowed resource (for hosts whose resources are host objects already).
pub fn borrowed_resource(p: Raw) -> Option<ID3D12Resource> {
    unsafe { ID3D12Resource::from_raw_borrowed(&p) }.cloned()
}
