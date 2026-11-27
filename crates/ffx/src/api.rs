//! The FFX API's descriptor types, as the title builds them. Written from AMD's MIT headers
//! (FidelityFX SDK v2.4.0: `ffx_api.h`, `ffx_api_types.h`, `upscalers/include/ffx_upscale.h`; MIT per the
//! SDK's `docs/license.md`) and checked against the title's own code (`docs/research/ffx.md`): every
//! layout below is the one its wrapper fills, offset for offset. x64 MSVC layouts; the asserts at the
//! bottom pin them.

use std::ffi::c_void;

/// `ffxApiHeader`: every descriptor starts with its type and a link to the next one in the chain.
#[repr(C)]
pub struct Header {
    pub kind: u64,
    pub next: *mut Header,
}

pub const EFFECT_MASK: u64 = 0x00ff_0000;
pub const BACKEND_MASK: u64 = 0xff00_0000;

// ---- general (effect 0) ----
pub const CONFIGURE_GLOBALDEBUG: u64 = 7;
pub const QUERY_GET_VERSIONS: u64 = 4;
pub const DESC_OVERRIDE_VERSION: u64 = 5;
pub const QUERY_GET_PROVIDER_VERSION: u64 = 6;
/// The backend description's sub id: `FFX_API_MAKE_BACKEND_SUB_ID(backend, 0x02)`. The title sends
/// `0x01000002` (its SDK's Xbox backend id), the PC SDK `0x00000002`; both carry the device at +0x10.
pub const BACKEND_DEVICE_SUB: u64 = 0x02;
/// `..._BACKEND_DX12_ALLOCATION_CALLBACKS` (sub 0x03): ignored, we allocate our own.
pub const BACKEND_ALLOCATION_SUB: u64 = 0x03;

// ---- upscale (effect 0x01) ----
pub const EFFECT_UPSCALE: u64 = 0x0001_0000;
pub const CREATE_UPSCALE: u64 = EFFECT_UPSCALE;
pub const DISPATCH_UPSCALE: u64 = EFFECT_UPSCALE | 0x01;
pub const QUERY_UPSCALE_RATIO: u64 = EFFECT_UPSCALE | 0x02;
pub const QUERY_RENDER_RESOLUTION: u64 = EFFECT_UPSCALE | 0x03;
pub const QUERY_JITTER_PHASE_COUNT: u64 = EFFECT_UPSCALE | 0x04;
pub const QUERY_JITTER_OFFSET: u64 = EFFECT_UPSCALE | 0x05;
pub const DISPATCH_REACTIVE_MASK: u64 = EFFECT_UPSCALE | 0x06;
pub const CONFIGURE_UPSCALE_KEYVALUE: u64 = EFFECT_UPSCALE | 0x07;
pub const QUERY_GPU_MEMORY: u64 = EFFECT_UPSCALE | 0x08;
pub const QUERY_GPU_MEMORY_V2: u64 = EFFECT_UPSCALE | 0x09;
pub const QUERY_RESOURCE_REQUIREMENTS: u64 = EFFECT_UPSCALE | 0x0a;
pub const CREATE_UPSCALE_VERSION: u64 = EFFECT_UPSCALE | 0x0b;

// ---- return codes ----
pub const OK: u32 = 0;
pub const ERROR: u32 = 1;
pub const ERROR_UNKNOWN_DESCTYPE: u32 = 2;
pub const ERROR_RUNTIME_ERROR: u32 = 3;
pub const NO_PROVIDER: u32 = 4;
pub const ERROR_MEMORY: u32 = 5;
pub const ERROR_PARAMETER: u32 = 6;

// ---- creation flags (FfxApiCreateContextUpscaleFlags) ----
pub const ENABLE_HDR: u32 = 1 << 0;
pub const ENABLE_DISPLAY_RES_MV: u32 = 1 << 1;
pub const ENABLE_MV_JITTER_CANCELLATION: u32 = 1 << 2;
pub const ENABLE_DEPTH_INVERTED: u32 = 1 << 3;
pub const ENABLE_DEPTH_INFINITE: u32 = 1 << 4;
pub const ENABLE_AUTO_EXPOSURE: u32 = 1 << 5;
pub const ENABLE_DYNAMIC_RESOLUTION: u32 = 1 << 6;

// ---- resource states (FfxApiResourceState) ----
pub const STATE_COMMON: u32 = 1 << 0;
pub const STATE_UNORDERED_ACCESS: u32 = 1 << 1;
pub const STATE_COMPUTE_READ: u32 = 1 << 2;
pub const STATE_PIXEL_READ: u32 = 1 << 3;
pub const STATE_PIXEL_COMPUTE_READ: u32 = STATE_PIXEL_READ | STATE_COMPUTE_READ;
pub const STATE_COPY_SRC: u32 = 1 << 4;
pub const STATE_COPY_DEST: u32 = 1 << 5;
pub const STATE_GENERIC_READ: u32 = STATE_COPY_SRC | STATE_COMPUTE_READ;
pub const STATE_INDIRECT_ARGUMENT: u32 = 1 << 6;
pub const STATE_PRESENT: u32 = 1 << 7;
pub const STATE_RENDER_TARGET: u32 = 1 << 8;
pub const STATE_DEPTH_ATTACHMENT: u32 = 1 << 9;

/// `ffxCreateContextDescUpscaleVersion::version` layout: `(major << 22) | (minor << 12) | patch`.
pub fn version_name(v: u32) -> String {
    format!("{}.{}.{}", v >> 22, (v >> 12) & 0x3ff, v & 0xfff)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dim {
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Coords {
    pub x: f32,
    pub y: f32,
}

/// `FfxApiResourceDescription`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceDesc {
    pub kind: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_count: u32,
    pub flags: u32,
    pub usage: u32,
}

/// `FfxApiResource`: the resource pointer is the title's (a console `ID3D12Resource`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Resource {
    pub resource: *mut c_void,
    pub description: ResourceDesc,
    pub state: u32,
}

impl Resource {
    pub const NONE: Resource = Resource {
        resource: std::ptr::null_mut(),
        description: ResourceDesc { kind: 0, format: 0, width: 0, height: 0, depth: 0, mip_count: 0, flags: 0, usage: 0 },
        state: 0,
    };
    pub fn is_null(&self) -> bool {
        self.resource.is_null()
    }
}

pub type Message = Option<unsafe extern "C" fn(u32, *const u16)>;

/// `ffxCreateContextDescUpscale` (0x10000).
#[repr(C)]
pub struct CreateUpscale {
    pub header: Header,
    pub flags: u32,
    pub max_render: Dim,
    pub max_upscale: Dim,
    pub message: Message,
}

/// `ffxCreateContextDescUpscaleVersion` (0x1000b).
#[repr(C)]
pub struct CreateUpscaleVersion {
    pub header: Header,
    pub version: u32,
}

/// The backend description (sub 0x02): `ffxCreateBackendDX12Desc` on the PC, its Xbox twin in the title.
#[repr(C)]
pub struct CreateBackend {
    pub header: Header,
    pub device: *mut c_void,
}

/// `ffxDispatchDescUpscale` (0x10001).
#[repr(C)]
pub struct DispatchUpscale {
    pub header: Header,
    pub command_list: *mut c_void,
    pub color: Resource,
    pub depth: Resource,
    pub motion_vectors: Resource,
    pub exposure: Resource,
    pub reactive: Resource,
    pub transparency_and_composition: Resource,
    pub output: Resource,
    pub jitter_offset: Coords,
    pub motion_vector_scale: Coords,
    pub render_size: Dim,
    pub upscale_size: Dim,
    pub enable_sharpening: bool,
    pub sharpness: f32,
    pub frame_time_delta: f32,
    pub pre_exposure: f32,
    pub reset: bool,
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov_angle_vertical: f32,
    pub view_space_to_meters_factor: f32,
    pub flags: u32,
}

/// 0x10002.
#[repr(C)]
pub struct QueryUpscaleRatio {
    pub header: Header,
    pub quality_mode: u32,
    pub out_ratio: *mut f32,
}

/// 0x10003.
#[repr(C)]
pub struct QueryRenderResolution {
    pub header: Header,
    pub display_width: u32,
    pub display_height: u32,
    pub quality_mode: u32,
    pub out_width: *mut u32,
    pub out_height: *mut u32,
}

/// 0x10004.
#[repr(C)]
pub struct QueryJitterPhaseCount {
    pub header: Header,
    pub render_width: u32,
    pub display_width: u32,
    pub out_phase_count: *mut i32,
}

/// 0x10005.
#[repr(C)]
pub struct QueryJitterOffset {
    pub header: Header,
    pub index: i32,
    pub phase_count: i32,
    pub out_x: *mut f32,
    pub out_y: *mut f32,
}

/// `FfxApiEffectMemoryUsage`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryUsage {
    pub total: u64,
    pub aliasable: u64,
}

/// 0x10008.
#[repr(C)]
pub struct QueryGpuMemory {
    pub header: Header,
    pub usage: *mut MemoryUsage,
}

/// 0x10009.
#[repr(C)]
pub struct QueryGpuMemoryV2 {
    pub header: Header,
    pub device: *mut c_void,
    pub max_render: Dim,
    pub max_upscale: Dim,
    pub flags: u32,
    pub usage: *mut MemoryUsage,
}

/// 0x1000a.
#[repr(C)]
pub struct QueryResourceRequirements {
    pub header: Header,
    pub required: u64,
    pub optional: u64,
}

pub const RESOURCE_COLOR: u64 = 1 << 0;
pub const RESOURCE_DEPTH: u64 = 1 << 1;
pub const RESOURCE_MV: u64 = 1 << 2;
pub const RESOURCE_EXPOSURE: u64 = 1 << 3;
pub const RESOURCE_REACTIVE: u64 = 1 << 4;
pub const RESOURCE_TRANSPARENCY: u64 = 1 << 5;

/// 4: `ffxQueryDescGetVersions`.
#[repr(C)]
pub struct QueryVersions {
    pub header: Header,
    pub create_desc_type: u64,
    pub device: *mut c_void,
    pub output_count: *mut u64,
    pub version_ids: *mut u64,
    pub version_names: *mut *const i8,
}

/// 6: `ffxQueryGetProviderVersion`.
#[repr(C)]
pub struct QueryProviderVersion {
    pub header: Header,
    pub version_id: u64,
    pub version_name: *const i8,
}

/// 7: `ffxConfigureDescGlobalDebug`.
#[repr(C)]
pub struct ConfigureGlobalDebug {
    pub header: Header,
    pub effect_id: u64,
    pub message: Message,
    pub level: u32,
}

// Layouts, offset for offset with the title's wrapper (docs/research/ffx.md §3).
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Header>() == 16);
    assert!(size_of::<Resource>() == 48);
    assert!(offset_of!(Resource, state) == 40);
    assert!(offset_of!(CreateUpscale, flags) == 0x10);
    assert!(offset_of!(CreateUpscale, max_render) == 0x14);
    assert!(offset_of!(CreateUpscale, max_upscale) == 0x1c);
    assert!(offset_of!(CreateUpscale, message) == 0x28);
    assert!(size_of::<CreateUpscale>() == 0x30);
    assert!(offset_of!(CreateUpscaleVersion, version) == 0x10);
    assert!(offset_of!(CreateBackend, device) == 0x10);
    assert!(offset_of!(DispatchUpscale, command_list) == 0x10);
    assert!(offset_of!(DispatchUpscale, color) == 0x18);
    assert!(offset_of!(DispatchUpscale, depth) == 0x48);
    assert!(offset_of!(DispatchUpscale, motion_vectors) == 0x78);
    assert!(offset_of!(DispatchUpscale, exposure) == 0xa8);
    assert!(offset_of!(DispatchUpscale, reactive) == 0xd8);
    assert!(offset_of!(DispatchUpscale, transparency_and_composition) == 0x108);
    assert!(offset_of!(DispatchUpscale, output) == 0x138);
    assert!(offset_of!(DispatchUpscale, jitter_offset) == 0x168);
    assert!(offset_of!(DispatchUpscale, motion_vector_scale) == 0x170);
    assert!(offset_of!(DispatchUpscale, render_size) == 0x178);
    assert!(offset_of!(DispatchUpscale, upscale_size) == 0x180);
    assert!(offset_of!(DispatchUpscale, enable_sharpening) == 0x188);
    assert!(offset_of!(DispatchUpscale, sharpness) == 0x18c);
    assert!(offset_of!(DispatchUpscale, frame_time_delta) == 0x190);
    assert!(offset_of!(DispatchUpscale, pre_exposure) == 0x194);
    assert!(offset_of!(DispatchUpscale, reset) == 0x198);
    assert!(offset_of!(DispatchUpscale, camera_near) == 0x19c);
    assert!(offset_of!(DispatchUpscale, camera_far) == 0x1a0);
    assert!(offset_of!(DispatchUpscale, camera_fov_angle_vertical) == 0x1a4);
    assert!(offset_of!(DispatchUpscale, view_space_to_meters_factor) == 0x1a8);
    assert!(offset_of!(DispatchUpscale, flags) == 0x1ac);
    assert!(size_of::<DispatchUpscale>() == 0x1b0);
    assert!(offset_of!(QueryJitterPhaseCount, render_width) == 0x10);
    assert!(offset_of!(QueryJitterPhaseCount, display_width) == 0x14);
    assert!(offset_of!(QueryJitterPhaseCount, out_phase_count) == 0x18);
    assert!(offset_of!(QueryJitterOffset, index) == 0x10);
    assert!(offset_of!(QueryJitterOffset, phase_count) == 0x14);
    assert!(offset_of!(QueryJitterOffset, out_x) == 0x18);
    assert!(offset_of!(QueryJitterOffset, out_y) == 0x20);
    assert!(offset_of!(QueryGpuMemory, usage) == 0x10);
    assert!(offset_of!(QueryRenderResolution, out_width) == 0x20);
};
