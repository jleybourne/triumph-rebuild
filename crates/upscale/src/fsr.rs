//! FSR through AMD's FidelityFX SDK (v2.4.0, the FFX API). The declarations below are written from
//! its MIT headers (`ffx_api.h`, `ffx_api_types.h`, `ffx_api_loader.h`, `dx12/ffx_api_dx12.h`,
//! `upscalers/include/ffx_upscale.h`; MIT per the SDK's `docs/license.md`).
//!
//! Nothing AMD ships is linked or committed. At run time the backend loads, from the folder it is
//! given, `amd_fidelityfx_upscaler_dx12.dll` (the effect providers) and then
//! `amd_fidelityfx_loader_dx12.dll` (the FFX API entry points, which find the already-loaded upscaler
//! module). The provider is chosen explicitly: the newest **FSR 3.1** provider the DLL offers, which
//! runs on any vendor (the DLL's FSR 4 provider needs RDNA 4; on AMD the layer may later prefer it).

use std::ffi::{c_void, CStr};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::{Inputs, Mode, Signs, Size, Upscaler};

// ---- ffx_api.h ----

#[repr(C)]
struct Header {
    kind: u64,
    next: *mut Header,
}

impl Header {
    fn new(kind: u64) -> Header {
        Header { kind, next: std::ptr::null_mut() }
    }
}

const EFFECT_UPSCALE: u64 = 0x0001_0000;
const QUERY_GET_VERSIONS: u64 = 4;
const DESC_OVERRIDE_VERSION: u64 = 5;
const QUERY_GET_PROVIDER_VERSION: u64 = 6;
const CONFIGURE_GLOBALDEBUG: u64 = 7;
const CREATE_BACKEND_DX12: u64 = 0x02; // FFX_API_MAKE_BACKEND_SUB_ID(DX12 = 0, 0x02)
const CREATE_UPSCALE: u64 = EFFECT_UPSCALE;
const DISPATCH_UPSCALE: u64 = EFFECT_UPSCALE | 0x01;
const QUERY_RENDER_RESOLUTION: u64 = EFFECT_UPSCALE | 0x03;
const QUERY_JITTER_PHASES: u64 = EFFECT_UPSCALE | 0x04;
const QUERY_JITTER_OFFSET: u64 = EFFECT_UPSCALE | 0x05;

#[repr(C)]
struct QueryVersions {
    header: Header,
    create_desc_type: u64,
    device: *mut c_void,
    output_count: *mut u64,
    version_ids: *mut u64,
    version_names: *mut *const i8,
}

#[repr(C)]
struct OverrideVersion {
    header: Header,
    version_id: u64,
}

#[repr(C)]
struct ProviderVersion {
    header: Header,
    version_id: u64,
    version_name: *const i8,
}

#[repr(C)]
struct GlobalDebug {
    header: Header,
    effect_id: u64,
    message: Option<unsafe extern "C" fn(u32, *const u16)>,
    level: u32,
}

#[repr(C)]
struct BackendDx12 {
    header: Header,
    device: *mut c_void,
}

// ---- ffx_api_types.h ----

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Dim {
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Coords {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceDesc {
    kind: u32,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    mip_count: u32,
    flags: u32,
    usage: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Resource {
    resource: *mut c_void,
    description: ResourceDesc,
    state: u32,
}

const NO_RESOURCE: Resource = Resource {
    resource: std::ptr::null_mut(),
    description: ResourceDesc { kind: 0, format: 0, width: 0, height: 0, depth: 0, mip_count: 0, flags: 0, usage: 0 },
    state: 0,
};

const TYPE_TEXTURE2D: u32 = 2;
const USAGE_READ_ONLY: u32 = 0;
const USAGE_UAV: u32 = 1 << 1;
const STATE_UNORDERED_ACCESS: u32 = 1 << 1;
const STATE_PIXEL_COMPUTE_READ: u32 = (1 << 3) | (1 << 2);

fn surface_format(f: DXGI_FORMAT) -> Result<u32, String> {
    Ok(match f {
        DXGI_FORMAT_R16G16B16A16_FLOAT => 4,
        DXGI_FORMAT_R16G16_FLOAT => 18,
        DXGI_FORMAT_R32_FLOAT => 28,
        _ => return Err(format!("FSR backend: format {f:?} not mapped")),
    })
}

// ---- ffx_upscale.h ----

const FLAG_HDR: u32 = 1 << 0;
const FLAG_DEPTH_INVERTED: u32 = 1 << 3;

#[repr(C)]
struct CreateUpscale {
    header: Header,
    flags: u32,
    max_render: Dim,
    max_upscale: Dim,
    message: Option<unsafe extern "C" fn(u32, *const u16)>,
}

#[repr(C)]
struct DispatchUpscale {
    header: Header,
    command_list: *mut c_void,
    color: Resource,
    depth: Resource,
    motion_vectors: Resource,
    exposure: Resource,
    reactive: Resource,
    transparency_and_composition: Resource,
    output: Resource,
    jitter_offset: Coords,
    motion_vector_scale: Coords,
    render_size: Dim,
    upscale_size: Dim,
    enable_sharpening: bool,
    sharpness: f32,
    frame_time_delta: f32,
    pre_exposure: f32,
    reset: bool,
    camera_near: f32,
    camera_far: f32,
    camera_fov_angle_vertical: f32,
    view_space_to_meters_factor: f32,
    flags: u32,
}

#[repr(C)]
struct QueryRenderResolution {
    header: Header,
    display_width: u32,
    display_height: u32,
    quality_mode: u32,
    out_width: *mut u32,
    out_height: *mut u32,
}

#[repr(C)]
struct QueryJitterPhases {
    header: Header,
    render_width: u32,
    display_width: u32,
    out_phases: *mut i32,
}

#[repr(C)]
struct QueryJitterOffset {
    header: Header,
    index: i32,
    phase_count: i32,
    out_x: *mut f32,
    out_y: *mut f32,
}

// Layout checks against the C headers (x64 MSVC).
const _: () = {
    assert!(std::mem::size_of::<Resource>() == 48);
    assert!(std::mem::offset_of!(DispatchUpscale, color) == 24);
    assert!(std::mem::offset_of!(DispatchUpscale, jitter_offset) == 24 + 7 * 48);
    assert!(std::mem::offset_of!(DispatchUpscale, enable_sharpening) == 392);
    assert!(std::mem::offset_of!(DispatchUpscale, reset) == 408);
    assert!(std::mem::offset_of!(DispatchUpscale, flags) == 428);
    assert!(std::mem::size_of::<DispatchUpscale>() == 432);
    assert!(std::mem::size_of::<CreateUpscale>() == 48);
};

type Ret = u32;
type Context = *mut c_void;
type FnCreate = unsafe extern "C" fn(*mut Context, *mut Header, *const c_void) -> Ret;
type FnDestroy = unsafe extern "C" fn(*mut Context, *const c_void) -> Ret;
type FnConfigure = unsafe extern "C" fn(*mut Context, *const Header) -> Ret;
type FnQuery = unsafe extern "C" fn(*mut Context, *mut Header) -> Ret;
type FnDispatch = unsafe extern "C" fn(*mut Context, *const Header) -> Ret;

const RETURNS: [&str; 8] = [
    "OK",
    "ERROR",
    "ERROR_UNKNOWN_DESCTYPE",
    "ERROR_RUNTIME_ERROR",
    "NO_PROVIDER",
    "ERROR_MEMORY",
    "ERROR_PARAMETER",
    "PROVIDER_NO_SUPPORT_NEW_DESCTYPE",
];

fn check(what: &str, r: Ret) -> Result<(), String> {
    if r == 0 {
        Ok(())
    } else {
        Err(format!("{what}: FFX_API_RETURN_{}", RETURNS.get(r as usize).copied().unwrap_or("?")))
    }
}

unsafe extern "C" fn message(kind: u32, msg: *const u16) {
    if msg.is_null() {
        return;
    }
    let mut n = 0;
    while *msg.add(n) != 0 {
        n += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(msg, n));
    eprintln!("fidelityfx {}: {}", if kind == 0 { "error" } else { "warn" }, s.trim_end());
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

unsafe fn proc<T>(lib: HMODULE, name: &str) -> Result<T, String> {
    let c = std::ffi::CString::new(name).unwrap();
    let f = GetProcAddress(lib, PCSTR(c.as_ptr() as *const u8)).ok_or_else(|| format!("amd_fidelityfx_loader_dx12.dll has no {name}"))?;
    Ok(std::mem::transmute_copy(&f))
}

fn quality(m: Mode) -> u32 {
    match m {
        Mode::NativeAa => 0,
        Mode::Quality => 1,
        Mode::Balanced => 2,
        Mode::Performance => 3,
        Mode::UltraPerformance => 4,
    }
}

pub struct Fsr {
    _upscaler: HMODULE,
    _loader: HMODULE,
    create: FnCreate,
    destroy: FnDestroy,
    configure: FnConfigure,
    query: FnQuery,
    dispatch: FnDispatch,
    context: Context,
    /// (depth reversed, HDR) the context was created for; a change recreates it.
    created_for: Option<(bool, bool)>,
    device: Option<ID3D12Device>,
    max: (Size, Size),
    /// Provider id to create with (the newest FSR 3.1), and every provider the DLL offers.
    provider: Option<(u64, String)>,
    pub providers: Vec<String>,
    dll_version: String,
    pub signs: Signs,
}

impl Fsr {
    /// Load the FFX API from `dir` (holding `amd_fidelityfx_loader_dx12.dll` and
    /// `amd_fidelityfx_upscaler_dx12.dll`).
    pub fn load(dir: &Path) -> Result<Fsr, String> {
        let up = dir.join("amd_fidelityfx_upscaler_dx12.dll");
        let lo = dir.join("amd_fidelityfx_loader_dx12.dll");
        for p in [&up, &lo] {
            if !p.is_file() {
                return Err(format!("not found: {}", p.display()));
            }
        }
        unsafe {
            // the upscaler first: the loader then finds it already in the process
            let upscaler = LoadLibraryW(PCWSTR(wide(&up).as_ptr())).map_err(|e| format!("load {}: {e}", up.display()))?;
            let loader = LoadLibraryW(PCWSTR(wide(&lo).as_ptr())).map_err(|e| format!("load {}: {e}", lo.display()))?;
            let f = Fsr {
                _upscaler: upscaler,
                _loader: loader,
                create: proc(loader, "ffxCreateContext")?,
                destroy: proc(loader, "ffxDestroyContext")?,
                configure: proc(loader, "ffxConfigure")?,
                query: proc(loader, "ffxQuery")?,
                dispatch: proc(loader, "ffxDispatch")?,
                context: std::ptr::null_mut(),
                created_for: None,
                device: None,
                max: (Size::new(0, 0), Size::new(0, 0)),
                provider: None,
                providers: Vec::new(),
                dll_version: crate::file_version(&up).unwrap_or_else(|| "?".into()),
                signs: Signs::FIDELITYFX,
            };
            // warnings and errors from every effect
            let dbg = GlobalDebug { header: Header::new(CONFIGURE_GLOBALDEBUG), effect_id: 0, message: Some(message), level: 2 };
            let _ = (f.configure)(std::ptr::null_mut(), &dbg.header);
            Ok(f)
        }
    }

    /// Pick the provider on this device and create the context for outputs up to `max_output`.
    pub fn attach(&mut self, device: &ID3D12Device, max_output: Size) -> Result<(), String> {
        self.device = Some(device.clone());
        self.max = (max_output, max_output);
        unsafe {
            let mut count = 0u64;
            let mut q = QueryVersions {
                header: Header::new(QUERY_GET_VERSIONS),
                create_desc_type: CREATE_UPSCALE,
                device: device.as_raw(),
                output_count: &mut count,
                version_ids: std::ptr::null_mut(),
                version_names: std::ptr::null_mut(),
            };
            check("ffxQuery(GET_VERSIONS count)", (self.query)(std::ptr::null_mut(), &mut q.header))?;
            let mut ids = vec![0u64; count as usize];
            let mut names = vec![std::ptr::null::<i8>(); count as usize];
            q.version_ids = ids.as_mut_ptr();
            q.version_names = names.as_mut_ptr();
            check("ffxQuery(GET_VERSIONS)", (self.query)(std::ptr::null_mut(), &mut q.header))?;
            let names: Vec<String> = names.iter().map(|n| if n.is_null() { "?".into() } else { CStr::from_ptr(*n).to_string_lossy().into_owned() }).collect();
            self.providers = names.clone();
            // the newest 3.1.x provider; fall back to the default (first) one
            self.provider = ids.iter().zip(&names).filter(|(_, n)| n.starts_with("3.1")).max_by_key(|(_, n)| version_key(n)).map(|(i, n)| (*i, n.clone()));
        }
        self.recreate(false, false)
    }

    fn recreate(&mut self, depth_reversed: bool, hdr: bool) -> Result<(), String> {
        let device = self.device.clone().ok_or("FSR not attached")?;
        unsafe {
            if !self.context.is_null() {
                (self.destroy)(&mut self.context, std::ptr::null());
                self.context = std::ptr::null_mut();
            }
            let mut backend = BackendDx12 { header: Header::new(CREATE_BACKEND_DX12), device: device.as_raw() };
            let mut over = OverrideVersion { header: Header::new(DESC_OVERRIDE_VERSION), version_id: self.provider.as_ref().map_or(0, |p| p.0) };
            if self.provider.is_some() {
                over.header.next = &mut backend.header;
            }
            let mut c = CreateUpscale {
                header: Header::new(CREATE_UPSCALE),
                flags: if depth_reversed { FLAG_DEPTH_INVERTED } else { 0 } | if hdr { FLAG_HDR } else { 0 },
                max_render: Dim { width: self.max.0.width, height: self.max.0.height },
                max_upscale: Dim { width: self.max.1.width, height: self.max.1.height },
                message: Some(message),
            };
            c.header.next = if self.provider.is_some() { &mut over.header } else { &mut backend.header };
            check("ffxCreateContext(upscale)", (self.create)(&mut self.context, &mut c.header, std::ptr::null()))?;
        }
        self.created_for = Some((depth_reversed, hdr));
        Ok(())
    }

    /// The provider actually running in the context ("3.1.x" etc.).
    pub fn provider_name(&self) -> String {
        unsafe {
            let mut q = ProviderVersion { header: Header::new(QUERY_GET_PROVIDER_VERSION), version_id: 0, version_name: std::ptr::null() };
            let mut ctx = self.context;
            if self.context.is_null() || (self.query)(&mut ctx, &mut q.header) != 0 || q.version_name.is_null() {
                return "?".into();
            }
            CStr::from_ptr(q.version_name).to_string_lossy().into_owned()
        }
    }

    /// FSR's own jitter for frame `index` of `phases` (pixels, its sign convention).
    pub fn sdk_jitter(&self, index: i32, phases: i32) -> Result<[f32; 2], String> {
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut q = QueryJitterOffset { header: Header::new(QUERY_JITTER_OFFSET), index, phase_count: phases, out_x: &mut x, out_y: &mut y };
        let mut ctx = self.context;
        unsafe { check("ffxQuery(GETJITTEROFFSET)", (self.query)(&mut ctx, &mut q.header))? };
        Ok([x, y])
    }

    /// FSR's jitter phase count for a render/output width pair.
    pub fn sdk_phases(&self, render_width: u32, display_width: u32) -> Result<i32, String> {
        let mut n = 0i32;
        let mut q = QueryJitterPhases { header: Header::new(QUERY_JITTER_PHASES), render_width, display_width, out_phases: &mut n };
        let mut ctx = self.context;
        unsafe { check("ffxQuery(GETJITTERPHASECOUNT)", (self.query)(&mut ctx, &mut q.header))? };
        Ok(n)
    }
}

fn version_key(name: &str) -> Vec<u32> {
    name.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).collect()
}

fn resource(r: &ID3D12Resource, state: u32, uav: bool) -> Result<Resource, String> {
    let d = unsafe { r.GetDesc() };
    Ok(Resource {
        resource: r.as_raw(),
        description: ResourceDesc {
            kind: TYPE_TEXTURE2D,
            format: surface_format(d.Format)?,
            width: d.Width as u32,
            height: d.Height,
            depth: 1,
            mip_count: d.MipLevels as u32,
            flags: 0,
            usage: if uav { USAGE_UAV } else { USAGE_READ_ONLY },
        },
        state,
    })
}

impl Upscaler for Fsr {
    type Resource = ID3D12Resource;
    type CommandList = ID3D12GraphicsCommandList;

    fn name(&self) -> String {
        format!("FSR {} (FidelityFX SDK v2.4.0, upscaler DLL {})", self.provider_name(), self.dll_version)
    }

    fn render_size(&self, output: Size, mode: Mode) -> Result<Size, String> {
        let (mut w, mut h) = (0u32, 0u32);
        let mut q = QueryRenderResolution {
            header: Header::new(QUERY_RENDER_RESOLUTION),
            display_width: output.width,
            display_height: output.height,
            quality_mode: quality(mode),
            out_width: &mut w,
            out_height: &mut h,
        };
        let mut ctx = self.context;
        unsafe { check("ffxQuery(GETRENDERRESOLUTIONFROMQUALITYMODE)", (self.query)(&mut ctx, &mut q.header))? };
        Ok(Size::new(w, h))
    }

    fn set_signs(&mut self, signs: Signs) {
        self.signs = signs;
    }

    fn evaluate(&mut self, list: &ID3D12GraphicsCommandList, _mode: Mode, i: &Inputs<ID3D12Resource>) -> Result<(), String> {
        i.check().map_err(|e| format!("inputs refused: {e:?}"))?;
        if i.output_size.width > self.max.1.width || i.output_size.height > self.max.1.height {
            return Err("FSR: output larger than the context was created for".into());
        }
        if self.created_for != Some((i.depth_reversed, i.hdr)) {
            self.recreate(i.depth_reversed, i.hdr)?;
        }
        let d = DispatchUpscale {
            header: Header::new(DISPATCH_UPSCALE),
            command_list: list.as_raw(),
            color: resource(i.colour, STATE_PIXEL_COMPUTE_READ, false)?,
            depth: resource(i.depth, STATE_PIXEL_COMPUTE_READ, false)?,
            motion_vectors: resource(i.motion, STATE_PIXEL_COMPUTE_READ, false)?,
            exposure: match i.exposure {
                Some(e) => resource(e, STATE_PIXEL_COMPUTE_READ, false)?,
                None => NO_RESOURCE,
            },
            reactive: NO_RESOURCE,
            transparency_and_composition: NO_RESOURCE,
            output: resource(i.output, STATE_UNORDERED_ACCESS, true)?,
            jitter_offset: Coords { x: i.jitter[0] * self.signs.jitter[0], y: i.jitter[1] * self.signs.jitter[1] },
            // FSR: pixelSpaceMV = stored × motionVectorScale, which is exactly the layer's motion_scale
            motion_vector_scale: Coords { x: i.motion_scale[0] * self.signs.motion[0], y: i.motion_scale[1] * self.signs.motion[1] },
            render_size: Dim { width: i.render.width, height: i.render.height },
            upscale_size: Dim { width: i.output_size.width, height: i.output_size.height },
            enable_sharpening: false,
            sharpness: 0.0,
            frame_time_delta: i.frame_ms,
            pre_exposure: i.pre_exposure,
            reset: i.reset,
            camera_near: i.near,
            camera_far: i.far,
            camera_fov_angle_vertical: i.fov_y,
            view_space_to_meters_factor: 1.0,
            flags: 0,
        };
        unsafe { check("ffxDispatch(upscale)", (self.dispatch)(&mut self.context, &d.header)) }
    }
}

impl Drop for Fsr {
    fn drop(&mut self) {
        if !self.context.is_null() {
            unsafe { (self.destroy)(&mut self.context, std::ptr::null()) };
        }
    }
}
