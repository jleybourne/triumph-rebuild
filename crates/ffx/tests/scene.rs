//! A synthetic FFX-API client, built the way the title's wrapper is (exe+0x28ef1d0 create,
//! +0x28ef560 jitter, +0x28ef700 dispatch; `docs/research/ffx.md`), drives our implementation end to end
//! on the card: `rebuild-upscale`'s synthetic scene goes in through `ffxDispatch`, and the upscale
//! quality checks of `upscaletest` must pass through this path (no NaN, no black frame, beats bilinear
//! on PSNR, steadier than bilinear on a static camera).
//!
//! GPU rules: runs only while `agents\MEASURING` is absent and with the `agents\CARD` lock
//! (`CardLock::take`); every fence wait is bounded at 5 s; no swap chain, no window.
//! Backends from the environment: FSR with `REBUILD_FIDELITYFX_DIR`, DLSS with
//! `REBUILD_STREAMLINE_DIR` + `REBUILD_DLSS_DLL`. Without either, or without the lock, it says so and
//! passes.

#![cfg(windows)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;

use rebuild_ffx::api::{self, Header};
use rebuild_ffx::backends::{prepare_dlss, Factory};
use rebuild_ffx::host::DirectHost;
use rebuild_upscale::d3d::{self, Gpu, Texture};
use rebuild_upscale::image::{bad_pixels, bilinear, mean_abs_diff, psnr};
use rebuild_upscale::scene::Scene;
use rebuild_upscale::Size;
use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

type Create = unsafe extern "C" fn(*mut *mut c_void, *mut Header, *const c_void) -> u32;
type Destroy = unsafe extern "C" fn(*mut *mut c_void, *const c_void) -> u32;
type Query = unsafe extern "C" fn(*mut *mut c_void, *mut Header) -> u32;
type Dispatch = unsafe extern "C" fn(*mut *mut c_void, *const Header) -> u32;

/// The four imports, bound the way the runtime binds the title's IAT.
struct Ffx {
    create: Create,
    destroy: Destroy,
    query: Query,
    dispatch: Dispatch,
}

fn bind() -> Ffx {
    let f = |n: &str| rebuild_ffx::export(n).unwrap_or_else(|| panic!("{n}")) as usize;
    unsafe {
        Ffx {
            create: std::mem::transmute::<usize, Create>(f("ffxCreateContext")),
            destroy: std::mem::transmute::<usize, Destroy>(f("ffxDestroyContext")),
            query: std::mem::transmute::<usize, Query>(f("ffxQuery")),
            dispatch: std::mem::transmute::<usize, Dispatch>(f("ffxDispatch")),
        }
    }
}

const OUTPUT: Size = Size::new(1920, 1080);
const RENDER: Size = Size::new(1280, 720);
const FRAMES: u32 = 32;
const MEASURED: u32 = 8;

/// Close, submit, and wait at most 5 s.
fn finish(g: &Gpu, fence: &ID3D12Fence, value: &mut u64) -> Result<(), String> {
    unsafe {
        g.list.Close().map_err(|e| format!("close: {e}"))?;
        g.queue.ExecuteCommandLists(&[Some(g.list.cast().unwrap())]);
        *value += 1;
        g.queue.Signal(fence, *value).map_err(|e| e.to_string())?;
        let event = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
        let r = fence.SetEventOnCompletion(*value, event).map_err(|e| e.to_string()).map(|_| WaitForSingleObject(event, 5000));
        let _ = CloseHandle(event);
        if r? != WAIT_OBJECT_0 {
            return Err("fence wait over 5 s".into());
        }
        g.device.GetDeviceRemovedReason().map_err(|e| format!("device removed: {e}"))
    }
}

fn resource(t: &Texture, state: u32) -> api::Resource {
    api::Resource {
        resource: t.resource.as_raw(),
        description: api::ResourceDesc { kind: 2, format: 0, width: t.size.width, height: t.size.height, depth: 1, mip_count: 1, flags: 0, usage: 0 },
        state,
    }
}

#[derive(Default, Debug)]
struct Result1 {
    psnr: f64,
    base_psnr: f64,
    stability: f64,
    base_stability: f64,
    nan: usize,
    black: usize,
}

/// One sequence from a reset through the FFX entry points; quality on the last MEASURED frames.
fn sequence(ffx: &Ffx, g: &mut Gpu, fence: &ID3D12Fence, value: &mut u64, moving: bool) -> Result<Result1, String> {
    let srv = D3D12_RESOURCE_STATES(D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0);
    let mut colour = g.texture(RENDER, DXGI_FORMAT_R16G16B16A16_FLOAT, false, srv);
    let mut depth = g.texture(RENDER, DXGI_FORMAT_R32_FLOAT, false, srv);
    let mut motion = g.texture(RENDER, DXGI_FORMAT_R16G16_FLOAT, false, srv);
    // the output rests in COPY_SOURCE: the dispatch must move it to UAV and back
    let mut output = g.texture(OUTPUT, DXGI_FORMAT_R16G16B16A16_FLOAT, true, D3D12_RESOURCE_STATE_COPY_SOURCE);

    // create, as exe+0x28ef1d0: upscale -> backend (the title's 0x01000002) -> version 4.1.1
    let mut version = api::CreateUpscaleVersion { header: Header { kind: api::CREATE_UPSCALE_VERSION, next: std::ptr::null_mut() }, version: 0x100_1001 };
    let mut backend = api::CreateBackend { header: Header { kind: 0x100_0002, next: &mut version.header }, device: g.device.as_raw() };
    let mut desc = api::CreateUpscale {
        header: Header { kind: api::CREATE_UPSCALE, next: &mut backend.header },
        flags: 0,
        max_render: api::Dim { width: RENDER.width, height: RENDER.height },
        max_upscale: api::Dim { width: OUTPUT.width, height: OUTPUT.height },
        message: None,
    };
    let mut ctx: *mut c_void = std::ptr::null_mut();
    let r = unsafe { (ffx.create)(&mut ctx, &mut desc.header, std::ptr::null()) };
    if r != api::OK {
        return Err(format!("ffxCreateContext: {r}"));
    }
    let mut phases = 0i32;
    let mut q = api::QueryJitterPhaseCount { header: Header { kind: api::QUERY_JITTER_PHASE_COUNT, next: std::ptr::null_mut() }, render_width: RENDER.width, display_width: OUTPUT.width, out_phase_count: &mut phases };
    assert_eq!(unsafe { (ffx.query)(&mut ctx, &mut q.header) }, api::OK);

    let scene = Scene { moving, depth_reversed: false };
    let mut out = Result1::default();
    let mut prev: Option<(Vec<[f32; 4]>, Vec<[f32; 4]>)> = None;
    let run = (|| -> Result<(), String> {
        for frame in 0..FRAMES {
            // jitter, as exe+0x28ef560: FFX's offset for this index is where the sample goes
            let (mut x, mut y) = (0.0f32, 0.0f32);
            let mut q = api::QueryJitterOffset { header: Header { kind: api::QUERY_JITTER_OFFSET, next: std::ptr::null_mut() }, index: frame as i32, phase_count: phases, out_x: &mut x, out_y: &mut y };
            assert_eq!(unsafe { (ffx.query)(&mut ctx, &mut q.header) }, api::OK);
            let f = scene.render(frame, RENDER, [x, y]);
            g.begin();
            let _s = [g.upload(&mut colour, &d3d::rgba16f(&f.colour)), g.upload(&mut depth, &d3d::r32f(&f.depth)), g.upload(&mut motion, &d3d::rg16f(&f.motion))];
            // dispatch, as exe+0x28ef700: jitter negated, motion-vector scale (1, 1), pre-exposure 1
            let d = api::DispatchUpscale {
                header: Header { kind: api::DISPATCH_UPSCALE, next: std::ptr::null_mut() },
                command_list: g.list.as_raw(),
                color: resource(&colour, api::STATE_PIXEL_COMPUTE_READ),
                depth: resource(&depth, api::STATE_PIXEL_COMPUTE_READ),
                motion_vectors: resource(&motion, api::STATE_PIXEL_COMPUTE_READ),
                exposure: api::Resource::NONE,
                reactive: api::Resource::NONE,
                transparency_and_composition: api::Resource::NONE,
                output: resource(&output, api::STATE_COPY_SRC),
                jitter_offset: api::Coords { x: -x, y: -y },
                motion_vector_scale: api::Coords { x: 1.0, y: 1.0 },
                render_size: api::Dim { width: RENDER.width, height: RENDER.height },
                upscale_size: api::Dim { width: OUTPUT.width, height: OUTPUT.height },
                enable_sharpening: false,
                sharpness: 0.0,
                frame_time_delta: 16.7,
                pre_exposure: 1.0,
                reset: frame == 0,
                camera_near: 0.1,
                camera_far: 1000.0,
                camera_fov_angle_vertical: 1.0,
                view_space_to_meters_factor: 0.0,
                flags: 0,
            };
            let r = unsafe { (ffx.dispatch)(&mut ctx, &d.header) };
            if r != api::OK {
                return Err(format!("ffxDispatch: {r}"));
            }
            let back = g.copy_out(&mut output);
            finish(g, fence, value)?;
            let px = d3d::from_rgba16f(&Gpu::read(&output, &back));
            let (nan, black) = bad_pixels(&px);
            out.nan += nan;
            if black == px.len() {
                out.black += 1;
            }
            if frame + MEASURED < FRAMES {
                continue;
            }
            let truth = scene.truth(frame, OUTPUT, 4);
            let base = bilinear(&f.colour, RENDER, OUTPUT);
            out.psnr += psnr(&px, &truth) / MEASURED as f64;
            out.base_psnr += psnr(&base, &truth) / MEASURED as f64;
            if let Some((p, b)) = &prev {
                out.stability += mean_abs_diff(&px, p) / (MEASURED - 1) as f64;
                out.base_stability += mean_abs_diff(&base, b) / (MEASURED - 1) as f64;
            }
            prev = Some((px, base));
        }
        Ok(())
    })();
    // the GPU is idle here (every frame waited): destroy as the title does
    assert_eq!(unsafe { (ffx.destroy)(&mut ctx, std::ptr::null()) }, api::OK);
    assert!(ctx.is_null());
    run.map(|_| out)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[test]
fn the_synthetic_scene_through_ffx_dispatch_beats_bilinear_on_every_backend() {
    let fsr = env_path("REBUILD_FIDELITYFX_DIR");
    let dlss = env_path("REBUILD_STREAMLINE_DIR").is_some() && env_path("REBUILD_DLSS_DLL").is_some();
    if fsr.is_none() && !dlss {
        eprintln!("skipped: no backend configured (REBUILD_FIDELITYFX_DIR, or REBUILD_STREAMLINE_DIR + REBUILD_DLSS_DLL)");
        return;
    }
    let _lock = match rebuild_pipelines::d3d::CardLock::take() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    if dlss {
        prepare_dlss(); // Streamline before the device
    }
    let mut g = Gpu::new().expect("device");
    let fence: ID3D12Fence = unsafe { g.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.expect("fence");
    let mut value = 0u64;
    let ffx = bind();
    let mut runs: Vec<(&str, Factory)> = Vec::new();
    if dlss {
        runs.push(("dlss", Factory::new(&g.device, None, false)));
    }
    if let Some(dir) = fsr {
        runs.push(("fsr", Factory::new(&g.device, Some(dir), true)));
    }
    let mut failures = Vec::new();
    for (name, factory) in runs {
        rebuild_ffx::install(Arc::new(DirectHost { factory }));
        for moving in [true, false] {
            let tag = if moving { "moving" } else { "static" };
            match sequence(&ffx, &mut g, &fence, &mut value, moving) {
                Ok(r) => {
                    eprintln!(
                        "{name} {tag}: PSNR {:.2} dB (bilinear {:.2}), frame-to-frame {:.3} (bilinear {:.3}), NaN {}, black frames {}",
                        r.psnr, r.base_psnr, r.stability, r.base_stability, r.nan, r.black
                    );
                    let ok = r.nan == 0 && r.black == 0 && r.psnr > r.base_psnr && (moving || r.stability < r.base_stability);
                    if !ok {
                        failures.push(format!("{name} {tag}: {r:?}"));
                    }
                }
                Err(e) => failures.push(format!("{name} {tag}: {e}")),
            }
        }
    }
    let s = rebuild_ffx::stats();
    eprintln!("ffx stats: {s:?}");
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(s.dispatches, s.recorded, "every dispatch recorded");
    assert_eq!(s.live, 0);
}
