//! Rectangle seams on WARP: every pixel of a rectangle is covered exactly once, whichever three corners the
//! title gives and in whatever order, both through the geometry stage ([`rebuild_pipelines::rectgs`]) and through
//! the list's CPU expansion (`rebuild_xlist::console::rect::expand`). Additive blending (ONE, ONE) of 1/255 a draw
//! counts the coverage per pixel: a pixel of the shared diagonal drawn twice reads 2, one drawn by neither 0.
//!
//! A whole-screen rectangle (the title's full-screen passes) leaves the geometry stage as one triangle, so it has
//! no diagonal: the console rasterises a rectangle as one primitive, and two triangles split every 2x2 pixel
//! quad on their shared diagonal (helper lanes beside real pixels). The pipeline statistics count the geometry
//! stage's primitives. (WARP itself clips a triangle that leaves the screen into a
//! fan, so on WARP even one triangle splits the quads of a diagonal; GPUs draw it whole inside their guard band.)
//! WARP only, never the card; skipped (passing) without WARP or `dxc.exe`.
#![cfg(windows)]

use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};

use rebuild_pipelines::d3d::{serialize, Gpu};
use rebuild_pipelines::layout::{RootDesc, StageLayout};
use rebuild_pipelines::pso::with_graphics_desc;
use rebuild_pipelines::rectgs::RectGs;
use rebuild_pipelines::state::{Blend, GraphicsState, InputElement};
use rebuild_pipelines::translate;
use rebuild_xlist::console::rect;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const W: u32 = 96;
const H: u32 = 54;

/// The title's back-buffer pass in short: a float3 position (here already in clip space) and a colour from the
/// vertex buffer, passed on with a uv; the pixel shader adds 1/255 a draw.
const VS: &str = r#"
struct In { float3 p : POSITION0; float4 c : COLOR0; };
struct Out { float4 pos : SV_Position; float4 c : COLOR0; float2 uv : TEXCOORD0; };
Out main(In i) { Out o; o.pos = float4(i.p.xy, 0.5, 1); o.c = i.c; o.uv = i.p.xy * 0.5 + 0.5; return o; }
"#;
const PS: &str = r#"
struct In { float4 pos : SV_Position; float4 c : COLOR0; float2 uv : TEXCOORD0; };
float4 main(In i) : SV_Target0 { return float4(1.0 / 255.0, 1.0 / 255.0, 1.0 / 255.0, 1.0 / 255.0); }
"#;
/// float3 position, unorm8x4 colour.
const STRIDE: usize = 16;
const LAYOUT: [(u32, u32); 2] = [(0, 6), (12, 28)];

fn dxc_compile(dxc: &Path, dir: &Path, name: &str, profile: &str, src: &str) -> Vec<u8> {
    let (hl, out) = (dir.join(format!("{name}.hlsl")), dir.join(format!("{name}.dxil")));
    std::fs::write(&hl, src).unwrap();
    let r = std::process::Command::new(dxc).args(["-T", profile]).args(translate::DXC_ARGS).arg(&out).arg(&hl).output().unwrap();
    assert!(r.status.success(), "{name}: {}", String::from_utf8_lossy(&r.stderr));
    std::fs::read(&out).unwrap()
}

fn buffer(device: &ID3D12Device, heap: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let props = D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() };
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }.unwrap();
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

/// A target, its readback and a list to draw with, reused for every case.
struct Bench {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    target: ID3D12Resource,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    _heap: ID3D12DescriptorHeap,
    readback: ID3D12Resource,
    vb: ID3D12Resource,
    stats: ID3D12QueryHeap,
    stats_readback: ID3D12Resource,
}

const PITCH: u32 = 512; // W * 4 rounded up to 256

impl Bench {
    fn new(device: &ID3D12Device) -> Bench {
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
            list.Close().unwrap();
            let tex = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(W),
                Height: H,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let props = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
            let mut target: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &tex, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut target).unwrap();
            let target = target.unwrap();
            let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() }).unwrap();
            let rtv = heap.GetCPUDescriptorHandleForHeapStart();
            device.CreateRenderTargetView(&target, None, rtv);
            Bench {
                device: device.clone(),
                fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                value: 0,
                readback: buffer(device, D3D12_HEAP_TYPE_READBACK, u64::from(PITCH * H), D3D12_RESOURCE_STATE_COPY_DEST),
                vb: buffer(device, D3D12_HEAP_TYPE_UPLOAD, 4096, D3D12_RESOURCE_STATE_GENERIC_READ),
                queue,
                alloc,
                list,
                target,
                rtv,
                _heap: heap,
                stats: {
                    let mut q: Option<ID3D12QueryHeap> = None;
                    device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS, Count: 1, NodeMask: 0 }, &mut q).unwrap();
                    q.unwrap()
                },
                stats_readback: buffer(device, D3D12_HEAP_TYPE_READBACK, 256, D3D12_RESOURCE_STATE_COPY_DEST),
            }
        }
    }

    /// Draws `vertices` (STRIDE bytes each) with `pso` as a triangle list: the coverage count of every pixel, and
    /// the primitives the geometry stage emitted.
    fn coverage(&mut self, root: &ID3D12RootSignature, pso: &ID3D12PipelineState, vertices: &[u8]) -> (Vec<u8>, u64) {
        unsafe {
            let mut p = std::ptr::null_mut();
            self.vb.Map(0, None, Some(&mut p)).unwrap();
            std::ptr::copy_nonoverlapping(vertices.as_ptr(), p as *mut u8, vertices.len());
            self.vb.Unmap(0, None);
            self.alloc.Reset().unwrap();
            let l = &self.list;
            l.Reset(&self.alloc, pso).unwrap();
            l.ClearRenderTargetView(self.rtv, &[0.0; 4], None);
            l.OMSetRenderTargets(1, Some(&self.rtv), false, None);
            l.SetGraphicsRootSignature(root);
            l.RSSetViewports(&[D3D12_VIEWPORT { Width: W as f32, Height: H as f32, MaxDepth: 1.0, ..Default::default() }]);
            l.RSSetScissorRects(&[windows::Win32::Foundation::RECT { left: 0, top: 0, right: W as i32, bottom: H as i32 }]);
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            l.IASetVertexBuffers(0, Some(&[D3D12_VERTEX_BUFFER_VIEW { BufferLocation: self.vb.GetGPUVirtualAddress(), SizeInBytes: vertices.len() as u32, StrideInBytes: STRIDE as u32 }]));
            l.BeginQuery(&self.stats, D3D12_QUERY_TYPE_PIPELINE_STATISTICS, 0);
            l.DrawInstanced((vertices.len() / STRIDE) as u32, 1, 0, 0);
            l.EndQuery(&self.stats, D3D12_QUERY_TYPE_PIPELINE_STATISTICS, 0);
            l.ResolveQueryData(&self.stats, D3D12_QUERY_TYPE_PIPELINE_STATISTICS, 0, 1, &self.stats_readback, 0);
            l.ResourceBarrier(&[transition(&self.target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Width: W, Height: H, Depth: 1, RowPitch: PITCH },
                    },
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&self.target), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
            l.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            l.ResourceBarrier(&[transition(&self.target, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
            l.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(l.cast::<ID3D12CommandList>().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).unwrap();
            while self.fence.GetCompletedValue() < self.value {
                std::thread::yield_now();
            }
            assert_eq!(self.device.GetDeviceRemovedReason(), Ok(()), "device removed");
            let mut p = std::ptr::null_mut();
            self.readback.Map(0, None, Some(&mut p)).unwrap();
            let bytes = std::slice::from_raw_parts(p as *const u8, (PITCH * H) as usize);
            let out = (0..H).flat_map(|y| (0..W).map(move |x| (y, x))).map(|(y, x)| bytes[(y * PITCH + x * 4) as usize]).collect();
            self.readback.Unmap(0, None);
            let mut p = std::ptr::null_mut();
            self.stats_readback.Map(0, None, Some(&mut p)).unwrap();
            let st = *(p as *const D3D12_QUERY_DATA_PIPELINE_STATISTICS);
            self.stats_readback.Unmap(0, None);
            (out, st.GSPrimitives)
        }
    }
}

fn vertex(x: f32, y: f32) -> Vec<u8> {
    let mut b = Vec::with_capacity(STRIDE);
    for f in [x, y, 0.0] {
        b.extend_from_slice(&f.to_le_bytes());
    }
    b.extend_from_slice(&[255, 255, 255, 255]);
    b
}

/// Every way a title can give a rectangle: three of its four corners, in any order.
fn orders(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<[[f32; 2]; 3]> {
    let c = [[x0, y1], [x0, y0], [x1, y0], [x1, y1]]; // TL BL BR TR (y up)
    let mut v = Vec::new();
    for skip in 0..4 {
        let three: Vec<[f32; 2]> = (0..4).filter(|&i| i != skip).map(|i| c[i]).collect();
        for p in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            v.push([three[p[0]], three[p[1]], three[p[2]]]);
        }
    }
    v
}

/// Each pixel exactly once inside, never outside (no pixel centre lies on an edge of the rectangles used).
fn assert_once(count: &[u8], x0: f32, y0: f32, x1: f32, y1: f32, what: &str) {
    let mut wrong = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let (cx, cy) = ((x as f32 + 0.5) / W as f32 * 2.0 - 1.0, 1.0 - (y as f32 + 0.5) / H as f32 * 2.0);
            let want = u8::from(cx > x0 && cx < x1 && cy > y0 && cy < y1);
            let got = count[(y * W + x) as usize];
            if got != want {
                wrong.push((x, y, got));
            }
        }
    }
    assert!(wrong.is_empty(), "{what}: {} pixels covered other than once (x, y, count): {:?}", wrong.len(), &wrong[..wrong.len().min(12)]);
}

#[test]
fn every_pixel_of_a_rectangle_is_covered_exactly_once() {
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
    let dir: PathBuf = std::env::temp_dir().join(format!("rebuild-rectseam-warp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let vs = dxc_compile(&dxc, &dir, "vs", "vs_6_6", VS);
    let ps = dxc_compile(&dxc, &dir, "ps", "ps_6_6", PS);
    let gs = RectGs::new(None).with_dxc(Some(dxc)).get(&vs, Some(&ps)).unwrap();
    let none = StageLayout::default();
    let root = gpu.root_signature(&serialize(&RootDesc::graphics(&none, &none, 16)).unwrap()).unwrap();
    let add = Blend { enable: true, src: 2, dst: 2, op: 1, src_alpha: 2, dst_alpha: 2, op_alpha: 1, ..Default::default() };
    let input = |name: &str, format: u32, offset: u32| InputElement { name: name.into(), index: 0, format, slot: 0, offset, per_instance: false, step_rate: 0 };
    let state = GraphicsState {
        blend: [add; 8],
        input: vec![input("POSITION", 6, 0), input("COLOR", 28, 12)],
        targets: vec![DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32],
        ..Default::default()
    };
    let make = |gs: Option<&[u8]>| with_graphics_desc(&root, &vs, Some(&ps), gs, &state, |d| unsafe { gpu.device.CreateGraphicsPipelineState::<ID3D12PipelineState>(d) }).unwrap();
    let (with_gs, triangles) = (make(Some(&gs)), make(None));
    let mut bench = Bench::new(&gpu.device);

    // the control: the three vertices alone, as the triangle they make, leave half the rectangle out
    let three: Vec<u8> = [[-1.0f32, 1.0], [-1.0, -1.0], [1.0, -1.0]].iter().flat_map(|p| vertex(p[0], p[1])).collect();
    let (c, _) = bench.coverage(&root, &triangles, &three);
    let covered = c.iter().filter(|&&n| n == 1).count();
    assert!(covered > 0 && covered < (W * H) as usize * 2 / 3, "a lone triangle covers about half: {covered}");

    // full screen (the title's back-buffer pass), off pixel centres, and two pixels high
    for (x0, y0, x1, y1) in [(-1.0f32, -1.0f32, 1.0f32, 1.0f32), (-0.73, -0.55, 0.61, 0.87), (-0.9, 0.01, 0.9, 0.07), (-1.0, -1.0, 1.0, 0.45)] {
        for corners in orders(x0, y0, x1, y1) {
            let three: Vec<u8> = corners.iter().flat_map(|p| vertex(p[0], p[1])).collect();
            let (c, primitives) = bench.coverage(&root, &with_gs, &three);
            assert_once(&c, x0, y0, x1, y1, &format!("geometry stage, corners {corners:?}"));
            // the whole screen as one triangle (no diagonal), any other rectangle as two
            let whole = (x0, y0, x1, y1) == (-1.0, -1.0, 1.0, 1.0);
            assert_eq!(primitives, if whole { 1 } else { 2 }, "geometry stage primitives, corners {corners:?}");
            // the list's CPU expansion of the same three vertices, drawn as triangles
            let mut six = vec![0u8; 6 * STRIDE];
            assert_eq!(rect::expand(&three, &mut six, STRIDE, &LAYOUT), 1);
            let (c, _) = bench.coverage(&root, &triangles, &six);
            assert_once(&c, x0, y0, x1, y1, &format!("CPU expansion, corners {corners:?}"));
        }
    }

    // whole-screen rectangles larger than the screen: one triangle, every pixel once
    for corners in [[[-1.5f32, 1.25], [-1.5, -3.0], [2.0, -3.0]], [[2.0, 2.0], [-2.0, 2.0], [-2.0, -2.0]]] {
        let three: Vec<u8> = corners.iter().flat_map(|p| vertex(p[0], p[1])).collect();
        let (c, primitives) = bench.coverage(&root, &with_gs, &three);
        assert!(c.iter().all(|&n| n == 1), "{corners:?}: every pixel once");
        assert_eq!(primitives, 1, "{corners:?}: one triangle");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
