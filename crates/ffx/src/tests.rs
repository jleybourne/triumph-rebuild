//! No GPU here: the descriptors as the title builds them, the conversions and the queries.
//! The end-to-end run on the card is `tests/scene.rs`.

use super::*;

fn header(kind: u64) -> api::Header {
    api::Header { kind, next: std::ptr::null_mut() }
}

fn res(p: usize, state: u32, w: u32, h: u32) -> api::Resource {
    api::Resource { resource: p as Raw, description: api::ResourceDesc { kind: 2, format: 4, width: w, height: h, depth: 1, mip_count: 1, flags: 0, usage: 0 }, state }
}

/// The dispatch the title's wrapper builds (exe+0x28ef700): inputs PIXEL_COMPUTE_READ, output UAV,
/// jitter negated, motion-vector scale (1, 1), pre-exposure 1, view-space-to-meters 0.
fn title_dispatch() -> api::DispatchUpscale {
    api::DispatchUpscale {
        header: header(api::DISPATCH_UPSCALE),
        command_list: 0x1000 as Raw,
        color: res(0x10, api::STATE_PIXEL_COMPUTE_READ, 1280, 720),
        depth: res(0x20, api::STATE_PIXEL_COMPUTE_READ, 1280, 720),
        motion_vectors: res(0x30, api::STATE_PIXEL_COMPUTE_READ, 1280, 720),
        exposure: api::Resource::NONE,
        reactive: api::Resource::NONE,
        transparency_and_composition: api::Resource::NONE,
        output: res(0x40, api::STATE_UNORDERED_ACCESS, 1920, 1080),
        jitter_offset: api::Coords { x: -0.25, y: 0.125 },
        motion_vector_scale: api::Coords { x: 1.0, y: 1.0 },
        render_size: api::Dim { width: 1280, height: 720 },
        upscale_size: api::Dim { width: 1920, height: 1080 },
        enable_sharpening: false,
        sharpness: 0.0,
        frame_time_delta: 33.3,
        pre_exposure: 1.0,
        reset: true,
        camera_near: 1000.0,
        camera_far: 0.1,
        camera_fov_angle_vertical: 0.9,
        view_space_to_meters_factor: 0.0,
        flags: 0,
    }
}

fn created(flags: u32) -> Created {
    Created { flags, max_render: api::Dim { width: 1920, height: 1080 }, max_upscale: api::Dim { width: 1920, height: 1080 }, version: 0x100_1001, backend: 0x100_0002 }
}

#[test]
fn the_titles_creation_chain_reads_as_upscale_backend_version() {
    // exe+0x28ef1d0: upscale -> backend (0x01000002, the device) -> version 4.1.1 (0x1001001)
    let mut version = api::CreateUpscaleVersion { header: header(api::CREATE_UPSCALE_VERSION), version: 0x100_1001 };
    let mut backend = api::CreateBackend { header: header(0x100_0002), device: 0x1234 as Raw };
    backend.header.next = &mut version.header;
    let mut up = api::CreateUpscale {
        header: header(api::CREATE_UPSCALE),
        flags: api::ENABLE_HDR | api::ENABLE_DEPTH_INVERTED | api::ENABLE_AUTO_EXPOSURE,
        max_render: api::Dim { width: 1280, height: 720 },
        max_upscale: api::Dim { width: 1920, height: 1080 },
        message: None,
    };
    up.header.next = &mut backend.header;
    let (c, _) = unsafe { read_create(&up.header) }.expect("reads");
    assert_eq!(c.flags, 0x29);
    assert_eq!((c.max_render, c.max_upscale), (api::Dim { width: 1280, height: 720 }, api::Dim { width: 1920, height: 1080 }));
    assert_eq!(c.backend, 0x100_0002);
    assert_eq!(api::version_name(c.version), "4.1.1");

    // a chain without the upscale description, and zero sizes, are refused
    let b = api::CreateBackend { header: header(2), device: std::ptr::null_mut() };
    assert_eq!(unsafe { read_create(&b.header) }.unwrap_err().0, api::ERROR_UNKNOWN_DESCTYPE);
    up.max_upscale = api::Dim::default();
    assert_eq!(unsafe { read_create(&up.header) }.unwrap_err().0, api::ERROR_PARAMETER);
}

#[test]
fn a_title_dispatch_translates_to_the_layers_convention() {
    let d = title_dispatch();
    let t = translate(&d, &created(api::ENABLE_DEPTH_INVERTED | api::ENABLE_HDR)).expect("translates");
    assert_eq!(t.jitter, [0.25, -0.125], "FFX's jitterOffset is the negated sample offset (measured, Signs::FIDELITYFX)");
    assert_eq!(t.motion_scale, [1.0, 1.0]);
    assert_eq!((t.render, t.output_size), (Size::new(1280, 720), Size::new(1920, 1080)));
    assert_eq!(t.mode, Mode::Quality);
    assert!(t.depth_reversed && t.hdr && t.reset);
    assert_eq!(t.exposure, None);
    assert_eq!(t.dropped, 0);
    assert_eq!(t.colour, Res { resource: 0x10 as Raw, state: api::STATE_PIXEL_COMPUTE_READ, format: 4 });
    assert_eq!(t.list, 0x1000 as Raw);
}

#[test]
fn dispatch_extras_are_reported_and_holes_refused() {
    let mut d = title_dispatch();
    d.reactive = res(0x50, api::STATE_UNORDERED_ACCESS, 1280, 720);
    d.enable_sharpening = true;
    d.sharpness = 0.5;
    d.upscale_size = api::Dim::default(); // optional: the context's maximum
    d.pre_exposure = 0.0; // invalid per FFX: taken as 1
    let t = translate(&d, &created(api::ENABLE_AUTO_EXPOSURE)).unwrap();
    assert_eq!(t.dropped, DROPPED_REACTIVE | DROPPED_SHARPENING | DROPPED_AUTO_EXPOSURE);
    assert_eq!(t.output_size, Size::new(1920, 1080));
    assert_eq!(t.pre_exposure, 1.0);
    d.depth = api::Resource::NONE;
    assert_eq!(translate(&d, &created(0)).unwrap_err().0, api::ERROR_PARAMETER);
    let mut d = title_dispatch();
    d.command_list = std::ptr::null_mut();
    assert_eq!(translate(&d, &created(0)).unwrap_err().0, api::ERROR_PARAMETER);
}

#[test]
fn modes_by_ratio() {
    let out = Size::new(2560, 1440);
    assert_eq!(mode_for(Size::new(2560, 1440), out), Mode::NativeAa);
    assert_eq!(mode_for(Size::new(1706, 960), out), Mode::Quality);
    assert_eq!(mode_for(Size::new(1505, 847), out), Mode::Balanced);
    assert_eq!(mode_for(Size::new(1280, 720), out), Mode::Performance);
    assert_eq!(mode_for(Size::new(853, 480), out), Mode::UltraPerformance);
}

#[test]
fn ffx_states_map_to_d3d12() {
    assert_eq!(d3d_state(api::STATE_PIXEL_COMPUTE_READ), SRV);
    assert_eq!(d3d_state(api::STATE_UNORDERED_ACCESS), D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    assert_eq!(d3d_state(api::STATE_COMMON), D3D12_RESOURCE_STATE_COMMON);
    assert_eq!(d3d_state(api::STATE_GENERIC_READ), D3D12_RESOURCE_STATE_COPY_SOURCE | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    assert_eq!(d3d_state(api::STATE_RENDER_TARGET), D3D12_RESOURCE_STATE_RENDER_TARGET);
}

#[test]
fn queries_answer_as_fsr_does() {
    // the title asks for the phase count at creation and one offset per frame (exe+0x28ef2d1, +0x28ef560)
    assert_eq!(phase_count(1706, 2560), 18, "FSR truncates; the measured SDK answer for 1706 -> 2560");
    assert_eq!(phase_count(1280, 1920), 18);
    assert_eq!(phase_count(1920, 1920), 8);
    let mut n = 0i32;
    let mut q = api::QueryJitterPhaseCount { header: header(api::QUERY_JITTER_PHASE_COUNT), render_width: 1280, display_width: 2560, out_phase_count: &mut n };
    let mut ctx: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { ffxQuery(&mut ctx, &mut q.header) }, api::OK);
    assert_eq!(n, 32);
    for index in [0, 1, 17, 18, 40] {
        let (mut x, mut y) = (9.0f32, 9.0f32);
        let mut q = api::QueryJitterOffset { header: header(api::QUERY_JITTER_OFFSET), index, phase_count: 18, out_x: &mut x, out_y: &mut y };
        assert_eq!(unsafe { ffxQuery(&mut ctx, &mut q.header) }, api::OK);
        assert_eq!([x, y], rebuild_upscale::jitter(index as u32 % 18, 18));
    }
    let (mut w, mut h) = (0u32, 0u32);
    let mut q = api::QueryRenderResolution { header: header(api::QUERY_RENDER_RESOLUTION), display_width: 2560, display_height: 1440, quality_mode: 3, out_width: &mut w, out_height: &mut h };
    assert_eq!(unsafe { ffxQuery(&mut ctx, &mut q.header) }, api::OK);
    assert_eq!((w, h), (1280, 720));
    let mut u = api::MemoryUsage { total: 7, aliasable: 7 };
    let mut q = api::QueryGpuMemory { header: header(api::QUERY_GPU_MEMORY), usage: &mut u };
    assert_eq!(unsafe { ffxQuery(&mut ctx, &mut q.header) }, api::OK);
    assert_eq!(u.total, 0);
    let mut q = header(0x10_00ff);
    assert_eq!(unsafe { ffxQuery(&mut ctx, &mut q) }, api::ERROR_UNKNOWN_DESCTYPE);
}

#[test]
fn bad_contexts_are_refused_not_dereferenced() {
    let d = title_dispatch();
    let mut junk = [1u64; 4];
    let mut ctx: Raw = junk.as_mut_ptr().cast();
    assert_eq!(unsafe { ffxDispatch(&mut ctx, &d.header) }, api::ERROR_PARAMETER);
    assert_eq!(unsafe { ffxDestroyContext(&mut ctx, std::ptr::null()) }, api::ERROR_PARAMETER);
    let mut null: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { ffxDispatch(&mut null, &d.header) }, api::ERROR_PARAMETER);
    assert_eq!(unsafe { ffxCreateContext(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) }, api::ERROR_PARAMETER);
}

#[test]
fn every_imported_name_is_exported() {
    for n in ["ffxCreateContext", "ffxDestroyContext", "ffxDispatch", "ffxQuery", "ffxConfigure"] {
        assert!(export(n).is_some(), "{n}");
    }
    assert!(export("ffxNothing").is_none());
}
