//! DLSS through NVIDIA Streamline (MIT; the declarations below are written from its public headers,
//! SDK 2.14.16: `sl_struct.h`, `sl_core_types.h`, `sl_core_api.h`, `sl_consts.h`, `sl_dlss.h`).
//!
//! Nothing NVIDIA ships is linked or committed. At run time the backend loads `sl.interposer.dll` from
//! the Streamline folder it is given, and Streamline loads `sl.common.dll`, `sl.dlss.dll` and, through
//! NGX, `nvngx_dlss.dll` from the plugin paths: the Streamline folder and the folder of the DLSS DLL
//! (the registry entry `upscaler.dlss_dll` proposed in `agents/upscale.md`).
//!
//! Manual hooking: the layer creates its D3D12 device natively and hands it over with
//! `slSetD3DDevice`; no proxies, no swap-chain hooks, no over-the-air downloads.

use std::ffi::{c_char, c_void, CStr};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::{Inputs, Mode, Size, Upscaler};

// ---- sl_struct.h ----

#[repr(C)]
#[derive(Clone, Copy)]
struct StructType {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
struct Base {
    next: *mut Base,
    struct_type: StructType,
    struct_version: usize,
}

impl Base {
    const fn new(t: StructType, version: usize) -> Base {
        Base { next: std::ptr::null_mut(), struct_type: t, struct_version: version }
    }
}

const fn st(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> StructType {
    StructType { data1, data2, data3, data4 }
}

// ---- sl_core_types.h / sl_consts.h / sl_dlss.h ----

const PREFERENCES: StructType = st(0x1ca10965, 0xbf8e, 0x432b, [0x8d, 0xa1, 0x67, 0x16, 0xd8, 0x79, 0xfb, 0x14]);
const VIEWPORT: StructType = st(0x171b6435, 0x9b3c, 0x4fc8, [0x99, 0x94, 0xfb, 0xe5, 0x25, 0x69, 0xaa, 0xa4]);
const RESOURCE: StructType = st(0x3a9d70cf, 0x2418, 0x4b72, [0x83, 0x91, 0x13, 0xf8, 0x72, 0x1c, 0x72, 0x61]);
const RESOURCE_TAG: StructType = st(0x4c6a5aad, 0xb445, 0x496c, [0x87, 0xff, 0x1a, 0xf3, 0x84, 0x5b, 0xe6, 0x53]);
const ADAPTER_INFO: StructType = st(0x0677315f, 0xa746, 0x4492, [0x9f, 0x42, 0xcb, 0x61, 0x42, 0xc9, 0xc3, 0xd4]);
const FEATURE_VERSION: StructType = st(0x6d5b51f0, 0x076b, 0x486d, [0x99, 0x95, 0x5a, 0x56, 0x10, 0x43, 0xf5, 0xc1]);
const CONSTANTS: StructType = st(0xdcd35ad7, 0x4e4a, 0x4bad, [0xa9, 0x0c, 0xe0, 0xc4, 0x9e, 0xb2, 0x3a, 0xfe]);
const DLSS_OPTIONS: StructType = st(0x6ac826e4, 0x4c61, 0x4101, [0xa9, 0x2d, 0x63, 0x8d, 0x42, 0x10, 0x57, 0xb8]);
const DLSS_OPTIMAL: StructType = st(0xef1d0957, 0xfd58, 0x4df7, [0xb5, 0x04, 0x8b, 0x69, 0xd8, 0xaa, 0x6b, 0x76]);

const SDK_VERSION: u64 = (2u64 << 48) | (14u64 << 32) | (16u64 << 16) | 0xfedc;
const FEATURE_DLSS: u32 = 0;
const BUFFER_DEPTH: u32 = 0;
const BUFFER_MOTION: u32 = 1;
const BUFFER_SCALING_IN: u32 = 3;
const BUFFER_SCALING_OUT: u32 = 4;
const BUFFER_EXPOSURE: u32 = 13;

const FLAG_DISABLE_CL_STATE_TRACKING: u64 = 1 << 0;
const FLAG_USE_MANUAL_HOOKING: u64 = 1 << 2;

const LIFECYCLE_ONLY_VALID_NOW: u32 = 0;
const B_FALSE: u8 = 0;
const B_TRUE: u8 = 1;
const INVALID_FLOAT: f32 = 3.402_823_466e38;

#[repr(C)]
struct Preferences {
    base: Base,
    show_console: bool,
    log_level: u32,
    paths_to_plugins: *const *const u16,
    num_paths_to_plugins: u32,
    path_to_logs_and_data: *const u16,
    allocate_callback: *const c_void,
    release_callback: *const c_void,
    log_message_callback: Option<unsafe extern "C" fn(u32, *const c_char)>,
    flags: u64,
    features_to_load: *const u32,
    num_features_to_load: u32,
    application_id: u32,
    engine: u32,
    engine_version: *const c_char,
    project_id: *const c_char,
    render_api: u32,
}

#[repr(C)]
struct ViewportHandle {
    base: Base,
    value: u32,
}

#[repr(C)]
struct Resource {
    base: Base,
    kind: u8,
    native: *mut c_void,
    memory: *mut c_void,
    view: *mut c_void,
    state: u32,
    width: u32,
    height: u32,
    native_format: u32,
    mip_levels: u32,
    array_layers: u32,
    gpu_virtual_address: u64,
    flags: u32,
    usage: u32,
    internal_flags: u16,
    reserved: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Extent {
    top: u32,
    left: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct ResourceTag {
    base: Base,
    resource: *mut Resource,
    kind: u32,
    lifecycle: u32,
    extent: Extent,
}

#[repr(C)]
struct AdapterInfo {
    base: Base,
    device_luid: *const u8,
    device_luid_size: u32,
    vk_physical_device: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct Version {
    major: u32,
    minor: u32,
    build: u32,
}

#[repr(C)]
struct FeatureVersion {
    base: Base,
    sl: Version,
    ngx: Version,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Float4x4([f32; 16]);

const IDENTITY: Float4x4 = Float4x4([1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);

#[repr(C)]
struct Constants {
    base: Base,
    camera_view_to_clip: Float4x4,
    clip_to_camera_view: Float4x4,
    clip_to_lens_clip: Float4x4,
    clip_to_prev_clip: Float4x4,
    prev_clip_to_clip: Float4x4,
    jitter_offset: [f32; 2],
    mvec_scale: [f32; 2],
    camera_pinhole_offset: [f32; 2],
    camera_pos: [f32; 3],
    camera_up: [f32; 3],
    camera_right: [f32; 3],
    camera_fwd: [f32; 3],
    camera_near: f32,
    camera_far: f32,
    camera_fov: f32,
    camera_aspect_ratio: f32,
    motion_vectors_invalid_value: f32,
    depth_inverted: u8,
    camera_motion_included: u8,
    motion_vectors_3d: u8,
    reset: u8,
    orthographic_projection: u8,
    motion_vectors_dilated: u8,
    motion_vectors_jittered: u8,
    min_relative_linear_depth_object_separation: f32,
}

#[repr(C)]
struct DlssOptions {
    base: Base,
    mode: u32,
    output_width: u32,
    output_height: u32,
    sharpness: f32,
    pre_exposure: f32,
    exposure_scale: f32,
    color_buffers_hdr: u8,
    indicator_invert_x: u8,
    indicator_invert_y: u8,
    dlaa_preset: u32,
    quality_preset: u32,
    balanced_preset: u32,
    performance_preset: u32,
    ultra_performance_preset: u32,
    ultra_quality_preset: u32,
    use_auto_exposure: u8,
    alpha_upscaling_enabled: u8,
}

#[repr(C)]
#[derive(Default)]
struct DlssOptimalSettingsBody {
    optimal_render_width: u32,
    optimal_render_height: u32,
    optimal_sharpness: f32,
    render_width_min: u32,
    render_height_min: u32,
    render_width_max: u32,
    render_height_max: u32,
}

#[repr(C)]
struct DlssOptimalSettings {
    base: Base,
    body: DlssOptimalSettingsBody,
}

/// The opaque frame token (a C++ class with a vtable; only ever passed back by pointer).
#[repr(C)]
struct FrameToken {
    _opaque: [u8; 0],
}

// Layout checks against the C++ headers (x64 MSVC).
const _: () = {
    assert!(std::mem::size_of::<Base>() == 32);
    assert!(std::mem::size_of::<Preferences>() == 144);
    assert!(std::mem::offset_of!(Preferences, render_api) == 136);
    assert!(std::mem::offset_of!(Resource, native) == 40);
    assert!(std::mem::size_of::<Resource>() == 112);
    assert!(std::mem::offset_of!(ResourceTag, extent) == 48);
    assert!(std::mem::offset_of!(Constants, jitter_offset) == 32 + 5 * 64);
    assert!(std::mem::offset_of!(Constants, depth_inverted) == 32 + 5 * 64 + 6 * 4 + 12 * 4 + 5 * 4);
    assert!(std::mem::offset_of!(Constants, min_relative_linear_depth_object_separation) == 452);
    assert!(std::mem::offset_of!(DlssOptions, dlaa_preset) == 32 + 6 * 4 + 4);
    assert!(std::mem::size_of::<DlssOptions>() == 88);
};

type Res = i32;
type FnInit = unsafe extern "C" fn(*const Preferences, u64) -> Res;
type FnShutdown = unsafe extern "C" fn() -> Res;
type FnIsFeatureSupported = unsafe extern "C" fn(u32, *const AdapterInfo) -> Res;
type FnSetFeatureLoaded = unsafe extern "C" fn(u32, bool) -> Res;
type FnEvaluateFeature = unsafe extern "C" fn(u32, *const FrameToken, *const *const Base, u32, *mut c_void) -> Res;
type FnSetConstants = unsafe extern "C" fn(*const Constants, *const FrameToken, *const ViewportHandle) -> Res;
type FnGetFeatureVersion = unsafe extern "C" fn(u32, *mut FeatureVersion) -> Res;
type FnGetFeatureFunction = unsafe extern "C" fn(u32, *const c_char, *mut *mut c_void) -> Res;
type FnGetNewFrameToken = unsafe extern "C" fn(*mut *mut FrameToken, *const u32) -> Res;
type FnSetD3DDevice = unsafe extern "C" fn(*mut c_void) -> Res;
type FnFreeResources = unsafe extern "C" fn(u32, *const ViewportHandle) -> Res;
type FnDlssGetOptimalSettings = unsafe extern "C" fn(*const DlssOptions, *mut DlssOptimalSettings) -> Res;
type FnDlssSetOptions = unsafe extern "C" fn(*const ViewportHandle, *const DlssOptions) -> Res;

const RESULTS: [&str; 40] = [
    "eOk", "eErrorIO", "eErrorDriverOutOfDate", "eErrorOSOutOfDate", "eErrorOSDisabledHWS", "eErrorDeviceNotCreated",
    "eErrorNoSupportedAdapterFound", "eErrorAdapterNotSupported", "eErrorNoPlugins", "eErrorVulkanAPI", "eErrorDXGIAPI",
    "eErrorD3DAPI", "eErrorNRDAPI", "eErrorNVAPI", "eErrorReflexAPI", "eErrorNGXFailed", "eErrorJSONParsing",
    "eErrorMissingProxy", "eErrorMissingResourceState", "eErrorInvalidIntegration", "eErrorMissingInputParameter",
    "eErrorNotInitialized", "eErrorComputeFailed", "eErrorInitNotCalled", "eErrorExceptionHandler",
    "eErrorInvalidParameter", "eErrorMissingConstants", "eErrorDuplicatedConstants", "eErrorMissingOrInvalidAPI",
    "eErrorCommonConstantsMissing", "eErrorUnsupportedInterface", "eErrorFeatureMissing", "eErrorFeatureNotSupported",
    "eErrorFeatureMissingHooks", "eErrorFeatureFailedToLoad", "eErrorFeatureWrongPriority",
    "eErrorFeatureMissingDependency", "eErrorFeatureManagerInvalidState", "eErrorInvalidState", "eWarnOutOfVRAM",
];

fn check(what: &str, r: Res) -> Result<(), String> {
    if r == 0 {
        Ok(())
    } else {
        Err(format!("{what}: {}", RESULTS.get(r as usize).copied().unwrap_or("unknown result")))
    }
}

unsafe extern "C" fn log(kind: u32, msg: *const c_char) {
    // 0 info, 1 warn, 2 error; info is noisy, keep warnings and errors
    if kind >= 1 && !msg.is_null() {
        let s = CStr::from_ptr(msg).to_string_lossy();
        eprint!("streamline {}: {}", if kind == 1 { "warn" } else { "error" }, s);
        if !s.ends_with('\n') {
            eprintln!();
        }
    }
}

/// How this backend maps the layer's convention onto Streamline's (measured: `upscaletest --sweep`).
#[derive(Clone, Copy, Debug)]
pub struct Signs {
    /// Multiplies the layer's jitter (+y down) before it goes to `Constants::jitterOffset`.
    pub jitter: [f32; 2],
    /// Multiplies the layer's motion scale before it goes to `Constants::mvecScale` (Ã· render size).
    pub motion: [f32; 2],
}

impl Signs {
    /// The layer's convention passed through unchanged (what the sweep calls (+,+,+,+)).
    pub const LAYER: Signs = Signs { jitter: [1.0, 1.0], motion: [1.0, 1.0] };
    /// Measured on the RTX 4070 Ti (`upscaletest --sweep`, 2560x1440 quality, 16 sign combinations):
    /// Streamline's `jitterOffset` is the NEGATED sample offset on both axes (31.58 dB against 27.16 dB
    /// passed through), and its motion vectors are previous − current, +y down, like the layer's
    /// (every flipped motion sign falls to 18-20 dB).
    pub const STREAMLINE: Signs = Signs { jitter: [-1.0, -1.0], motion: [1.0, 1.0] };
}

pub struct Dlss {
    _lib: HMODULE,
    shutdown: FnShutdown,
    evaluate: FnEvaluateFeature,
    set_constants: FnSetConstants,
    new_frame: FnGetNewFrameToken,
    free: FnFreeResources,
    optimal: Option<FnDlssGetOptimalSettings>,
    set_options: Option<FnDlssSetOptions>,
    version: String,
    frame: u32,
    viewport: u32,
    pub signs: Signs,
    /// The DLSS preset for every mode (0 = the DLL's default).
    pub preset: u32,
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

unsafe fn proc<T>(lib: HMODULE, name: &str) -> Result<T, String> {
    let c = std::ffi::CString::new(name).unwrap();
    let f = GetProcAddress(lib, PCSTR(c.as_ptr() as *const u8)).ok_or_else(|| format!("sl.interposer.dll has no {name}"))?;
    Ok(std::mem::transmute_copy(&f))
}

fn sl_mode(m: Mode) -> u32 {
    match m {
        Mode::NativeAa => 6,         // eDLAA
        Mode::Quality => 3,          // eMaxQuality
        Mode::Balanced => 2,         // eBalanced
        Mode::Performance => 1,      // eMaxPerformance
        Mode::UltraPerformance => 4, // eUltraPerformance
    }
}

impl Dlss {
    /// Step 1, before the D3D12 device exists: load Streamline from `streamline_dir` (holding
    /// `sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll`) and `slInit` with the DLSS DLL's folder
    /// as a second plugin path.
    pub fn init(streamline_dir: &Path, dlss_dll: &Path, logs: Option<&Path>) -> Result<Dlss, String> {
        if !dlss_dll.is_file() {
            return Err(format!("DLSS DLL not found: {}", dlss_dll.display()));
        }
        let interposer = streamline_dir.join("sl.interposer.dll");
        if !interposer.is_file() {
            return Err(format!("sl.interposer.dll not found in {}", streamline_dir.display()));
        }
        let dlss_dir: PathBuf = dlss_dll.parent().map(Path::to_path_buf).unwrap_or_default();
        unsafe {
            let w = wide(&interposer);
            let lib = LoadLibraryW(PCWSTR(w.as_ptr())).map_err(|e| format!("load {}: {e}", interposer.display()))?;
            let init: FnInit = proc(lib, "slInit")?;
            let p0 = wide(streamline_dir);
            let p1 = wide(&dlss_dir);
            let paths = [p0.as_ptr(), p1.as_ptr()];
            let log_dir = logs.map(wide);
            let features = [FEATURE_DLSS];
            let project = c"1f6e1b7a-5e3c-4f0b-9c84-2b9e5d0c7a31";
            let engine_version = c"0.1";
            let prefs = Preferences {
                base: Base::new(PREFERENCES, 1),
                show_console: false,
                log_level: if logs.is_some() { 2 } else { 1 },
                paths_to_plugins: paths.as_ptr(),
                num_paths_to_plugins: 2,
                path_to_logs_and_data: log_dir.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                allocate_callback: std::ptr::null(),
                release_callback: std::ptr::null(),
                log_message_callback: Some(log),
                // no over-the-air downloads, no downloaded plugins: only what the owner put on disk
                flags: FLAG_DISABLE_CL_STATE_TRACKING | FLAG_USE_MANUAL_HOOKING,
                features_to_load: features.as_ptr(),
                num_features_to_load: 1,
                application_id: 0,
                engine: 0,
                engine_version: engine_version.as_ptr(),
                project_id: project.as_ptr(),
                render_api: 1, // eD3D12
            };
            check("slInit", init(&prefs, SDK_VERSION))?;
            Ok(Dlss {
                _lib: lib,
                shutdown: proc(lib, "slShutdown")?,
                evaluate: proc(lib, "slEvaluateFeature")?,
                set_constants: proc(lib, "slSetConstants")?,
                new_frame: proc(lib, "slGetNewFrameToken")?,
                free: proc(lib, "slFreeResources")?,
                optimal: None,
                set_options: None,
                version: String::new(),
                frame: 0,
                viewport: 0,
                signs: Signs::STREAMLINE,
                preset: 0,
            })
        }
    }

    /// Step 2, once the native device exists: hand it to Streamline, check DLSS on this adapter, load
    /// it and fetch the DLSS entry points.
    pub fn attach(&mut self, device: &ID3D12Device, luid: [u8; 8]) -> Result<(), String> {
        unsafe {
            let lib = self._lib;
            let set_device: FnSetD3DDevice = proc(lib, "slSetD3DDevice")?;
            check("slSetD3DDevice", set_device(device.as_raw()))?;
            let supported: FnIsFeatureSupported = proc(lib, "slIsFeatureSupported")?;
            let info = AdapterInfo { base: Base::new(ADAPTER_INFO, 1), device_luid: luid.as_ptr(), device_luid_size: 8, vk_physical_device: std::ptr::null_mut() };
            check("slIsFeatureSupported(DLSS)", supported(FEATURE_DLSS, &info))?;
            let loaded: FnSetFeatureLoaded = proc(lib, "slSetFeatureLoaded")?;
            check("slSetFeatureLoaded(DLSS)", loaded(FEATURE_DLSS, true))?;
            let get: FnGetFeatureFunction = proc(lib, "slGetFeatureFunction")?;
            let mut f: *mut c_void = std::ptr::null_mut();
            check("slGetFeatureFunction(slDLSSGetOptimalSettings)", get(FEATURE_DLSS, c"slDLSSGetOptimalSettings".as_ptr(), &mut f))?;
            self.optimal = Some(std::mem::transmute::<*mut c_void, FnDlssGetOptimalSettings>(f));
            check("slGetFeatureFunction(slDLSSSetOptions)", get(FEATURE_DLSS, c"slDLSSSetOptions".as_ptr(), &mut f))?;
            self.set_options = Some(std::mem::transmute::<*mut c_void, FnDlssSetOptions>(f));
        }
        self.refresh_version()
    }

    /// Re-read the Streamline and DLSS versions (NGX reports its version only once DLSS has run).
    pub fn refresh_version(&mut self) -> Result<(), String> {
        unsafe {
            let ver: FnGetFeatureVersion = proc(self._lib, "slGetFeatureVersion")?;
            let mut v = FeatureVersion { base: Base::new(FEATURE_VERSION, 1), sl: Version::default(), ngx: Version::default() };
            check("slGetFeatureVersion", ver(FEATURE_DLSS, &mut v))?;
            self.version = format!("Streamline {}.{}.{}, DLSS {}.{}.{}", v.sl.major, v.sl.minor, v.sl.build, v.ngx.major, v.ngx.minor, v.ngx.build);
        }
        Ok(())
    }

    fn options(&self, mode: Mode, output: Size, hdr: bool, pre_exposure: f32) -> DlssOptions {
        DlssOptions {
            base: Base::new(DLSS_OPTIONS, 3),
            mode: sl_mode(mode),
            output_width: output.width,
            output_height: output.height,
            sharpness: 0.0,
            pre_exposure,
            exposure_scale: 1.0,
            color_buffers_hdr: if hdr { B_TRUE } else { B_FALSE },
            indicator_invert_x: B_FALSE,
            indicator_invert_y: B_FALSE,
            dlaa_preset: self.preset,
            quality_preset: self.preset,
            balanced_preset: self.preset,
            performance_preset: self.preset,
            ultra_performance_preset: self.preset,
            ultra_quality_preset: self.preset,
            use_auto_exposure: B_FALSE,
            alpha_upscaling_enabled: B_FALSE,
        }
    }

    /// The SDK's render-size range for a mode: (optimal, min, max).
    pub fn sizes(&self, output: Size, mode: Mode) -> Result<(Size, Size, Size), String> {
        let o = self.options(mode, output, false, 1.0);
        let mut s = DlssOptimalSettings { base: Base::new(DLSS_OPTIMAL, 1), body: Default::default() };
        let f = self.optimal.ok_or("DLSS not attached")?;
        unsafe { check("slDLSSGetOptimalSettings", f(&o, &mut s))? };
        let b = &s.body;
        Ok((
            Size::new(b.optimal_render_width, b.optimal_render_height),
            Size::new(b.render_width_min, b.render_height_min),
            Size::new(b.render_width_max, b.render_height_max),
        ))
    }
}

fn resource(r: &ID3D12Resource, state: D3D12_RESOURCE_STATES) -> Resource {
    let d = unsafe { r.GetDesc() };
    Resource {
        base: Base::new(RESOURCE, 1),
        kind: 0, // eTex2d
        native: r.as_raw(),
        memory: std::ptr::null_mut(),
        view: std::ptr::null_mut(),
        state: state.0 as u32,
        width: d.Width as u32,
        height: d.Height,
        native_format: d.Format.0 as u32,
        mip_levels: d.MipLevels as u32,
        array_layers: d.DepthOrArraySize as u32,
        gpu_virtual_address: 0,
        flags: 0,
        usage: 0,
        internal_flags: 0,
        reserved: 0,
    }
}

impl Upscaler for Dlss {
    type Resource = ID3D12Resource;
    type CommandList = ID3D12GraphicsCommandList;

    fn name(&self) -> String {
        if self.version.is_empty() {
            "DLSS (Streamline)".into()
        } else {
            self.version.clone()
        }
    }

    fn render_size(&self, output: Size, mode: Mode) -> Result<Size, String> {
        self.sizes(output, mode).map(|s| s.0)
    }

    fn evaluate(&mut self, list: &ID3D12GraphicsCommandList, mode: Mode, i: &Inputs<ID3D12Resource>) -> Result<(), String> {
        i.check().map_err(|e| format!("inputs refused: {e:?}"))?;
        let viewport = ViewportHandle { base: Base::new(VIEWPORT, 1), value: self.viewport };
        let srv = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        unsafe {
            let mut token: *mut FrameToken = std::ptr::null_mut();
            let index = self.frame;
            check("slGetNewFrameToken", (self.new_frame)(&mut token, &index))?;
            self.frame = self.frame.wrapping_add(1);

            let (w, h) = (i.render.width as f32, i.render.height as f32);
            let c = Constants {
                base: Base::new(CONSTANTS, 2),
                camera_view_to_clip: IDENTITY,
                clip_to_camera_view: IDENTITY,
                clip_to_lens_clip: IDENTITY,
                clip_to_prev_clip: IDENTITY,
                prev_clip_to_clip: IDENTITY,
                jitter_offset: [i.jitter[0] * self.signs.jitter[0], i.jitter[1] * self.signs.jitter[1]],
                // the layer's scale gives render pixels; Streamline wants values in [-1, 1] (Ã· render size)
                mvec_scale: [i.motion_scale[0] * self.signs.motion[0] / w, i.motion_scale[1] * self.signs.motion[1] / h],
                camera_pinhole_offset: [0.0, 0.0],
                camera_pos: [0.0, 0.0, 0.0],
                camera_up: [0.0, 1.0, 0.0],
                camera_right: [1.0, 0.0, 0.0],
                camera_fwd: [0.0, 0.0, 1.0],
                camera_near: i.near,
                camera_far: i.far,
                camera_fov: i.fov_y,
                camera_aspect_ratio: i.output_size.width as f32 / i.output_size.height as f32,
                motion_vectors_invalid_value: INVALID_FLOAT,
                depth_inverted: if i.depth_reversed { B_TRUE } else { B_FALSE },
                camera_motion_included: B_TRUE,
                motion_vectors_3d: B_FALSE,
                reset: if i.reset { B_TRUE } else { B_FALSE },
                orthographic_projection: B_FALSE,
                motion_vectors_dilated: B_FALSE,
                motion_vectors_jittered: B_FALSE,
                min_relative_linear_depth_object_separation: 40.0,
            };
            check("slSetConstants", (self.set_constants)(&c, token, &viewport))?;
            let o = self.options(mode, i.output_size, i.hdr, i.pre_exposure);
            check("slDLSSSetOptions", self.set_options.ok_or("DLSS not attached")?(&viewport, &o))?;

            let mut colour = resource(i.colour, srv);
            let mut depth = resource(i.depth, srv);
            let mut motion = resource(i.motion, srv);
            let mut output = resource(i.output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
            let mut exposure = i.exposure.map(|e| resource(e, srv));
            let rx = Extent { top: 0, left: 0, width: i.render.width, height: i.render.height };
            let ox = Extent { top: 0, left: 0, width: i.output_size.width, height: i.output_size.height };
            let tag = |r: &mut Resource, kind: u32, e: Extent| ResourceTag {
                base: Base::new(RESOURCE_TAG, 1),
                resource: r,
                kind,
                lifecycle: LIFECYCLE_ONLY_VALID_NOW,
                extent: e,
            };
            let mut tags = vec![
                tag(&mut colour, BUFFER_SCALING_IN, rx),
                tag(&mut depth, BUFFER_DEPTH, rx),
                tag(&mut motion, BUFFER_MOTION, rx),
                tag(&mut output, BUFFER_SCALING_OUT, ox),
            ];
            if let Some(e) = exposure.as_mut() {
                tags.push(tag(e, BUFFER_EXPOSURE, Extent { top: 0, left: 0, width: 1, height: 1 }));
            }
            let mut inputs: Vec<*const Base> = vec![&viewport.base];
            inputs.extend(tags.iter().map(|t| &t.base as *const Base));
            check("slEvaluateFeature(DLSS)", (self.evaluate)(FEATURE_DLSS, token, inputs.as_ptr(), inputs.len() as u32, list.as_raw()))?;
        }
        Ok(())
    }
}

impl Drop for Dlss {
    fn drop(&mut self) {
        unsafe {
            let viewport = ViewportHandle { base: Base::new(VIEWPORT, 1), value: self.viewport };
            if !self.version.is_empty() {
                let _ = (self.free)(FEATURE_DLSS, &viewport);
            }
            let _ = (self.shutdown)();
        }
    }
}
