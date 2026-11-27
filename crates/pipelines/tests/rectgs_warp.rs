//! Rectangle lists through the geometry stage on WARP: a rectangle-list pipeline (console topology type 19) made
//! by the pool's creator gets the generated geometry stage, and three vertices a rectangle draw the whole
//! rectangle, the fourth corner continuing every float output, integer outputs the first vertex's. Positions come
//! from the vertex number (no vertex buffer: nothing the CPU could expand), drawn direct and indexed. A pixel
//! shader that reads the primitive id gets the rectangle's number. WARP only, never the card; skipped (passing)
//! without WARP or `dxc.exe`.
#![cfg(windows)]

use std::mem::ManuallyDrop;
use std::path::PathBuf;

use rebuild_pipelines::d3d::{serialize, Gpu};
use rebuild_pipelines::layout::StageLayout;
use rebuild_pipelines::library::{self, Input, Library};
use rebuild_pipelines::pool::Creator;
use rebuild_pipelines::pso::{Programs, PsoCreator};
use rebuild_pipelines::rectgs::{RectGs, RECT_TOPOLOGY_TYPE};
use rebuild_pipelines::state::{Desc, GraphicsState, Key};
use rebuild_pipelines::{translate, Stage};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const SIZE: u32 = 64;

/// Two rectangles from the vertex number: A (vertices 0-2) top left, bottom left, bottom right (the corner
/// second); B (3-5) top left, bottom left, top right (the corner first). uv runs 0-1 across each; `id` is the
/// vertex number (an integer output: flat).
const VS: &str = r#"
static const float2 P[6] = { float2(-0.9, 0.4), float2(-0.9, -0.4), float2(-0.1, -0.4), float2(0.1, 0.4), float2(0.1, -0.4), float2(0.9, 0.4) };
static const float2 T[6] = { float2(0, 0), float2(0, 1), float2(1, 1), float2(0, 0), float2(0, 1), float2(1, 0) };
struct Out { float4 pos : SV_Position; float2 uv : TEXCOORD0; uint id : TEXCOORD1; };
Out main(uint v : SV_VertexID) { Out o; o.pos = float4(P[v], 0.5, 1); o.uv = T[v]; o.id = v; return o; }
"#;
const PS: &str = r#"
struct In { float4 pos : SV_Position; float2 uv : TEXCOORD0; uint id : TEXCOORD1; };
float4 main(In i) : SV_Target0 { return float4(i.uv, i.id / 8.0, 1); }
"#;
const PS_PRIM: &str = r#"
struct In { float4 pos : SV_Position; float2 uv : TEXCOORD0; uint id : TEXCOORD1; uint prim : SV_PrimitiveID; };
float4 main(In i) : SV_Target0 { return float4(i.prim * 0.5 + 0.25, i.uv.x, 0, 1); }
"#;

fn dxc_compile(dxc: &std::path::Path, dir: &std::path::Path, name: &str, profile: &str, src: &str) -> Vec<u8> {
    let (hl, out) = (dir.join(format!("{name}.hlsl")), dir.join(format!("{name}.dxil")));
    std::fs::write(&hl, src).unwrap();
    let r = std::process::Command::new(dxc).args(["-T", profile]).args(translate::DXC_ARGS).arg(&out).arg(&hl).output().unwrap();
    assert!(r.status.success(), "{name}: {}", String::from_utf8_lossy(&r.stderr));
    std::fs::read(&out).unwrap()
}

fn run(device: &ID3D12Device, queue: &ID3D12CommandQueue, list: &ID3D12GraphicsCommandList) {
    unsafe {
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        while fence.GetCompletedValue() < 1 {
            std::thread::yield_now();
        }
        assert_eq!(device.GetDeviceRemovedReason(), Ok(()), "device removed");
    }
}

fn resource(device: &ID3D12Device, heap: D3D12_HEAP_TYPE, desc: &D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES, clear: Option<&D3D12_CLEAR_VALUE>) -> ID3D12Resource {
    let props = D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() };
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, desc, state, clear.map(|c| c as *const _), &mut r) }.unwrap();
    r.unwrap()
}

fn transition(r: &ID3D12Resource, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: unsafe { std::mem::transmute_copy(r) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: from,
                StateAfter: to,
            }),
        },
        ..Default::default()
    }
}

/// Draws with `pso` (`draw` records the draws) into a cleared 64 x 64 RGBA8 target; its pixels.
fn render(gpu: &Gpu, root: &ID3D12RootSignature, pso: &ID3D12PipelineState, draw: impl FnOnce(&ID3D12Device, &ID3D12GraphicsCommandList) -> Vec<ID3D12Resource>) -> Vec<[u8; 4]> {
    let device = &gpu.device;
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
        let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, pso).unwrap();
        let tex = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(SIZE),
            Height: SIZE,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let clear = D3D12_CLEAR_VALUE { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Anonymous: D3D12_CLEAR_VALUE_0 { Color: [0.0; 4] } };
        let target = resource(device, D3D12_HEAP_TYPE_DEFAULT, &tex, D3D12_RESOURCE_STATE_RENDER_TARGET, Some(&clear));
        let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() }).unwrap();
        let rtv = heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&target, None, rtv);
        let pitch = SIZE * 4; // 256: the placement alignment already
        let buf = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: u64::from(pitch * SIZE),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let readback = resource(device, D3D12_HEAP_TYPE_READBACK, &buf, D3D12_RESOURCE_STATE_COPY_DEST, None);

        list.ClearRenderTargetView(rtv, &[0.0; 4], None);
        list.OMSetRenderTargets(1, Some(&rtv), false, None);
        list.SetGraphicsRootSignature(root);
        list.RSSetViewports(&[D3D12_VIEWPORT { Width: SIZE as f32, Height: SIZE as f32, MaxDepth: 1.0, ..Default::default() }]);
        list.RSSetScissorRects(&[windows::Win32::Foundation::RECT { left: 0, top: 0, right: SIZE as i32, bottom: SIZE as i32 }]);
        // the list's topology for a rectangle list: the triangle list the geometry stage completes
        list.IASetPrimitiveTopology(windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        let _keep = draw(device, &list);
        list.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&readback),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Width: SIZE, Height: SIZE, Depth: 1, RowPitch: pitch },
                },
            },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&target), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        run(device, &queue, &list);
        let mut p = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).unwrap();
        let px = std::slice::from_raw_parts(p as *const [u8; 4], (SIZE * SIZE) as usize).to_vec();
        readback.Unmap(0, None);
        px
    }
}

/// The pixel at (x, y) and where its centre is in clip space.
fn at(px: &[[u8; 4]], x: u32, y: u32) -> [u8; 4] {
    px[(y * SIZE + x) as usize]
}

fn near(got: u8, want: f32) -> bool {
    (f32::from(got) - want * 255.0).abs() <= 3.0
}

#[test]
fn rectangles_draw_whole_through_the_geometry_stage() {
    let gpu = match Gpu::warp() {
        Ok(g) => g,
        Err(e) => {
            println!("skipped: {e}");
            return;
        }
    };
    let Some(dxc) = translate::dxc_path() else {
        println!("skipped: no dxc.exe (Windows SDK, or REBUILD_DXC)");
        return;
    };
    let dir: PathBuf = std::env::temp_dir().join(format!("rebuild-rectgs-warp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let vs = dxc_compile(&dxc, &dir, "vs", "vs_6_6", VS);
    let ps = dxc_compile(&dxc, &dir, "ps", "ps_6_6", PS);
    let ps_prim = dxc_compile(&dxc, &dir, "psprim", "ps_6_6", PS_PRIM);

    // a program library of the three, and the creator the pool runs
    let lib = dir.join("programs.lib");
    let none = StageLayout::default();
    let input = vec![
        Input { hash: 1, stage: Stage::Vs, dxil: vs, layout: none.clone() },
        Input { hash: 2, stage: Stage::Ps, dxil: ps, layout: none.clone() },
        Input { hash: 3, stage: Stage::Ps, dxil: ps_prim, layout: none.clone() },
    ];
    library::build(&lib, 16, input, 1, serialize).unwrap();
    let programs = Programs::new(&gpu.device, Library::load(&lib, 1, true).unwrap().0, None).with_rect_gs(RectGs::new(Some(dir.join("gs"))).with_dxc(Some(dxc)));
    let creator = PsoCreator::new(&gpu.device, programs, None);
    let state = GraphicsState { topology: RECT_TOPOLOGY_TYPE, targets: vec![DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32], ..Default::default() };
    let make = |ps: u64| {
        let desc = Desc::Graphics { vs: 1, ps: Some(ps), state: Box::new(state.clone()) };
        let key = Key::of(&desc);
        let pso = creator.create(&key, &desc).unwrap_or_else(|e| panic!("{e}"));
        assert!(creator.rect_gs(&key), "built with the geometry stage");
        pso
    };
    let pso = make(2);
    let pso_prim = make(3);
    let s = creator.programs().rect_gs().stats();
    assert_eq!((s.compiled, s.refused), (2, 0), "one stage per signature: the primitive id makes a second");
    // a triangle pipeline gets none
    let tri = Desc::Graphics { vs: 1, ps: Some(2), state: Box::new(GraphicsState { topology: 3, ..state.clone() }) };
    creator.create(&Key::of(&tri), &tri).unwrap();
    assert!(!creator.rect_gs(&Key::of(&tri)));
    let root = creator.programs().graphics(1, Some(2)).unwrap().2;

    // A direct, B indexed (indices 3, 4, 5): the title's counts, three vertices a rectangle
    let px = render(&gpu, &root, &pso, |device, list| unsafe {
        list.DrawInstanced(3, 1, 0, 0);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 256,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let ib = resource(device, D3D12_HEAP_TYPE_UPLOAD, &desc, D3D12_RESOURCE_STATE_GENERIC_READ, None);
        let mut p = std::ptr::null_mut();
        ib.Map(0, None, Some(&mut p)).unwrap();
        std::ptr::copy_nonoverlapping([3u16, 4, 5].as_ptr(), p as *mut u16, 3);
        ib.Unmap(0, None);
        list.IASetIndexBuffer(Some(&D3D12_INDEX_BUFFER_VIEW { BufferLocation: ib.GetGPUVirtualAddress(), SizeInBytes: 6, Format: DXGI_FORMAT_R16_UINT }));
        list.DrawIndexedInstanced(3, 1, 0, 0, 0);
        vec![ib]
    });
    // clip space of a pixel centre, and the uv of a point in a rectangle spanning x0..x0+0.8, y -0.4..0.4
    let clip = |x: u32, y: u32| ((x as f32 + 0.5) / 32.0 - 1.0, 1.0 - (y as f32 + 0.5) / 32.0);
    let uv = |x: u32, y: u32, x0: f32| {
        let (cx, cy) = clip(x, y);
        ((cx - x0) / 0.8, (0.4 - cy) / 0.8)
    };
    // A's half the title's three vertices do not cover (above the diagonal top left - bottom right): the fourth
    // corner's, top right, uv (1, 0)
    let (x, y) = (25, 22);
    let (u, v) = uv(x, y, -0.9);
    assert!(u > v, "a pixel of the missing half");
    let p = at(&px, x, y);
    assert!(near(p[0], u) && near(p[1], v) && p[3] == 255, "A's fourth-corner half: {p:?}, want uv ({u}, {v})");
    assert_eq!(p[2], 0, "the flat id is the first vertex's (0), not the corner's (1)");
    // and its own half
    let p = at(&px, 18, 44);
    let (u, v) = uv(18, 44, -0.9);
    assert!(near(p[0], u) && near(p[1], v) && p[2] == 0, "A's own half: {p:?}");
    // B (indexed): its missing half, bottom right, uv (1, 1)
    let (x, y) = (57, 41);
    let (u, v) = uv(x, y, 0.1);
    assert!(u + v > 1.0);
    let p = at(&px, x, y);
    assert!(near(p[0], u) && near(p[1], v), "B's fourth-corner half (indexed): {p:?}, want uv ({u}, {v})");
    assert!(near(p[2], 3.0 / 8.0), "B's flat id is its first vertex's (3): {p:?}");
    // outside both, and between them: the clear colour
    for (x, y) in [(32, 32), (2, 2), (60, 60), (20, 4)] {
        assert_eq!(at(&px, x, y), [0; 4], "({x}, {y}) is outside the rectangles");
    }
    // every pixel inside A is covered: the whole rectangle
    let mut inside = 0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (cx, cy) = clip(x, y);
            if (-0.9..-0.1).contains(&cx) && (-0.4..0.4).contains(&cy) {
                inside += 1;
                assert_eq!(at(&px, x, y)[3], 255, "({x}, {y}) inside A is drawn");
            }
        }
    }
    assert!(inside > 300);

    // one draw of both rectangles, the pixel shader reading the primitive id: the rectangle's number
    let px = render(&gpu, &root, &pso_prim, |_, list| unsafe {
        list.DrawInstanced(6, 1, 0, 0);
        Vec::new()
    });
    let (a, b) = (at(&px, 25, 22), at(&px, 57, 41));
    assert!(near(a[0], 0.25) && near(b[0], 0.75), "primitive ids 0 and 1, both halves: {a:?} {b:?}");
    assert!(near(a[1], uv(25, 22, -0.9).0) && near(b[1], uv(57, 41, 0.1).0));
    let _ = std::fs::remove_dir_all(&dir);
}
