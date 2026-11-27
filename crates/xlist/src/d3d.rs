//! A real D3D12 sink, and everything a translated frame binds: root signature, the title's
//! pipelines (one PC pipeline per console handle, built at start), a shader-visible descriptor
//! heap the size of the console's, console buffer memory, barrier targets, an indirect signature.

#![cfg(windows)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::frame::Layout;
use crate::{translate, Cmd, Context, PipelineTable, Range, Sink, Stats};

const RT: u32 = 64;
const MEMORY: u64 = 64 << 20;

pub struct Setup {
    pub device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    fence_value: u64,
    signature: ID3D12RootSignature,
    pipelines: Vec<ID3D12PipelineState>,
    heap: ID3D12DescriptorHeap,
    rtv_heap: ID3D12DescriptorHeap,
    _rt: ID3D12Resource,
    memory: ID3D12Resource,
    memory_va: u64,
    targets: Vec<ID3D12Resource>,
    indirect: ID3D12CommandSignature,
    texture: ID3D12Resource,
    pub build_ms: f64,
}

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

fn blob(b: &ID3DBlob) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() }
}

fn compile(src: &[u8], target: PCSTR) -> Result<Vec<u8>, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    unsafe {
        D3DCompile(src.as_ptr().cast(), src.len(), PCSTR::null(), None, None, windows::core::s!("main"), target, 0, 0, &mut code, Some(&mut errors))
    }
    .map_err(|e| match &errors {
        Some(b) => format!("compile: {e}: {}", String::from_utf8_lossy(&blob(b))),
        None => format!("compile: {e}"),
    })?;
    Ok(blob(&code.ok_or("compile: no code")?))
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: u64, uav: bool, state: D3D12_RESOURCE_STATES) -> Result<ID3D12Resource, String> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r)
    }
    .map_err(err("buffer"))?;
    r.ok_or_else(|| "buffer: none".into())
}

impl Setup {
    /// Build everything a frame of `layout` binds. One PC pipeline per console handle.
    pub fn new(layout: &Layout, descriptor_slots: u32) -> Result<(Setup, Context), String> {
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None::<&windows::core::IUnknown>, D3D_FEATURE_LEVEL_11_0, &mut device).map_err(err("device"))?;
            let device = device.ok_or("device: none")?;
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).map_err(err("queue"))?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err("fence"))?;

            // root signature: two SRV tables, a root CBV, four root constants
            let ranges = [
                D3D12_DESCRIPTOR_RANGE { RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV, NumDescriptors: 8, BaseShaderRegister: 0, RegisterSpace: 0, OffsetInDescriptorsFromTableStart: 0 },
                D3D12_DESCRIPTOR_RANGE { RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV, NumDescriptors: 8, BaseShaderRegister: 8, RegisterSpace: 0, OffsetInDescriptorsFromTableStart: 0 },
            ];
            let table = |r: &D3D12_DESCRIPTOR_RANGE| D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 { DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE { NumDescriptorRanges: 1, pDescriptorRanges: r } },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            };
            let params = [
                table(&ranges[0]),
                table(&ranges[1]),
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 1, RegisterSpace: 0, Num32BitValues: 4 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
            ];
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: 0,
                pStaticSamplers: std::ptr::null(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
            };
            let mut sb: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut sb, None).map_err(err("signature"))?;
            let signature: ID3D12RootSignature = device.CreateRootSignature(0, &blob(&sb.ok_or("signature: none")?)).map_err(err("signature"))?;

            let vs = compile(
                br#"cbuffer A : register(b0) { float4 a; }; cbuffer K : register(b1) { float4 k; };
                    float4 main(float3 p : POSITION) : SV_Position { return float4(p * 0.001 + a.xyz * 1e-9 + k.xyz * 1e-9, 1); }"#,
                windows::core::s!("vs_5_0"),
            )?;
            let ps = compile(br#"float4 main() : SV_Target { return float4(1, 0, 0, 1); }"#, windows::core::s!("ps_5_0"))?;

            // one PC pipeline per console handle, all built before the first frame
            let t = Instant::now();
            let element = D3D12_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("POSITION"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            };
            let mut pipelines = Vec::with_capacity(layout.pipeline_handles.len());
            for i in 0..layout.pipeline_handles.len() {
                let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&signature),
                    VS: D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr().cast(), BytecodeLength: vs.len() },
                    PS: D3D12_SHADER_BYTECODE { pShaderBytecode: ps.as_ptr().cast(), BytecodeLength: ps.len() },
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: [D3D12_CULL_MODE_NONE, D3D12_CULL_MODE_BACK, D3D12_CULL_MODE_FRONT][i % 3],
                        DepthBias: i as i32, // makes every pipeline a distinct object
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    InputLayout: D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: &element, NumElements: 1 },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    ..Default::default()
                };
                d.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
                d.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
                pipelines.push(device.CreateGraphicsPipelineState::<ID3D12PipelineState>(&d).map_err(err("pipeline"))?);
            }
            let build_ms = t.elapsed().as_secs_f64() * 1000.0;

            // the console's descriptor heap, as a shader-visible heap, filled with null views
            let slots = descriptor_slots + 16;
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: slots, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
                .map_err(err("heap"))?;
            let inc = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let mut null = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_FLOAT,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                ..Default::default()
            };
            null.Anonymous.Buffer = D3D12_BUFFER_SRV { FirstElement: 0, NumElements: 1, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_NONE };
            let cpu0 = heap.GetCPUDescriptorHandleForHeapStart();
            for i in 0..slots as usize {
                device.CreateShaderResourceView(None, Some(&null), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu0.ptr + i * inc });
            }

            // render target
            let rt_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: RT as u64,
                Height: RT,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let mut rt: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &rt_desc, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut rt)
                .map_err(err("render target"))?;
            let rt = rt.ok_or("render target: none")?;
            let rtv_heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() })
                .map_err(err("rtv heap"))?;
            device.CreateRenderTargetView(&rt, None, rtv_heap.GetCPUDescriptorHandleForHeapStart());

            // console buffer memory: constants, vertices, indices (zeros), indirect arguments
            let memory = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, MEMORY, false, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            memory.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p)).map_err(err("map"))?;
            std::ptr::write_bytes(p as *mut u8, 0, MEMORY as usize);
            let args = (p as *mut u8).add(40 << 20) as *mut u32;
            for i in 0..1024 {
                // D3D12_DRAW_INDEXED_ARGUMENTS: 3 indices, 1 instance, at a 32-byte stride
                let a = args.add(i * 8);
                a.write(3);
                a.add(1).write(1);
            }
            memory.Unmap(0, None);
            let memory_va = memory.GetGPUVirtualAddress();

            let targets = (0..layout.barrier_targets.len())
                .map(|_| buffer(&device, D3D12_HEAP_TYPE_DEFAULT, 64 << 10, true, D3D12_RESOURCE_STATE_UNORDERED_ACCESS))
                .collect::<Result<Vec<_>, _>>()?;

            let arg = D3D12_INDIRECT_ARGUMENT_DESC { Type: D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED, ..Default::default() };
            let mut indirect: Option<ID3D12CommandSignature> = None;
            device
                .CreateCommandSignature(&D3D12_COMMAND_SIGNATURE_DESC { ByteStride: 32, NumArgumentDescs: 1, pArgumentDescs: &arg, NodeMask: 0 }, None, &mut indirect)
                .map_err(err("command signature"))?;
            let indirect = indirect.ok_or("command signature: none")?;

            let tex_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 256,
                Height: 256,
                DepthOrArraySize: 1,
                MipLevels: 9,
                Format: DXGI_FORMAT_BC1_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            let mut texture: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &tex_desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture)
                .map_err(err("texture"))?;
            let texture = texture.ok_or("texture: none")?;

            let ctx = Context {
                pipelines: PipelineTable::new(&layout.pipeline_handles),
                descriptors: Range {
                    console: layout.descriptor_base,
                    bytes: descriptor_slots as u64 * crate::CONSOLE_DESCRIPTOR,
                    pc: heap.GetGPUDescriptorHandleForHeapStart().ptr,
                },
                descriptor_increment: inc as u64,
                memory: Range { console: layout.memory_base, bytes: MEMORY.min(layout.memory_bytes), pc: memory_va },
                barrier_targets: layout.barrier_targets.clone(),
            };

            Ok((
                Setup { device, queue, fence, fence_value: 0, signature, pipelines, heap, rtv_heap, _rt: rt, memory, memory_va, targets, indirect, texture, build_ms },
                ctx,
            ))
        }
    }
}

/// Writes translated calls into one D3D12 graphics command list.
pub struct D3dSink<'a> {
    pub list: &'a ID3D12GraphicsCommandList,
    pub setup: &'a Setup,
}

impl Sink for D3dSink<'_> {
    fn begin(&mut self) {
        unsafe {
            let s = self.setup;
            self.list.SetDescriptorHeaps(&[Some(s.heap.clone())]);
            self.list.SetGraphicsRootSignature(&s.signature);
            let rtv = s.rtv_heap.GetCPUDescriptorHandleForHeapStart();
            self.list.OMSetRenderTargets(1, Some(&rtv), false, None);
            self.list.RSSetViewports(&[D3D12_VIEWPORT { Width: RT as f32, Height: RT as f32, MaxDepth: 1.0, ..Default::default() }]);
            self.list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: RT as i32, bottom: RT as i32 }]);
            self.list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        }
    }
    #[inline]
    fn pipeline(&mut self, i: u32) {
        unsafe { self.list.SetPipelineState(&self.setup.pipelines[i as usize]) }
    }
    #[inline]
    fn table(&mut self, slot: u32, h: u64) {
        unsafe { self.list.SetGraphicsRootDescriptorTable(slot, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: h }) }
    }
    #[inline]
    fn cbv(&mut self, _slot: u32, va: u64) {
        unsafe { self.list.SetGraphicsRootConstantBufferView(2, va) }
    }
    #[inline]
    fn constants(&mut self, v: &[u32; 4]) {
        unsafe { self.list.SetGraphicsRoot32BitConstants(3, 4, v.as_ptr().cast(), 0) }
    }
    #[inline]
    fn vertex_buffer(&mut self, va: u64, bytes: u32, stride: u32) {
        unsafe { self.list.IASetVertexBuffers(0, Some(&[D3D12_VERTEX_BUFFER_VIEW { BufferLocation: va, SizeInBytes: bytes, StrideInBytes: stride }])) }
    }
    #[inline]
    fn index_buffer(&mut self, va: u64, bytes: u32) {
        unsafe { self.list.IASetIndexBuffer(Some(&D3D12_INDEX_BUFFER_VIEW { BufferLocation: va, SizeInBytes: bytes, Format: DXGI_FORMAT_R16_UINT })) }
    }
    #[inline]
    fn barrier(&mut self, r: u32) {
        unsafe {
            let b = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        // borrowed, not AddRef'd: the barrier never drops it
                        pResource: std::mem::transmute_copy(&self.setup.targets[r as usize]),
                    }),
                },
            };
            self.list.ResourceBarrier(&[b]);
        }
    }
    #[inline]
    fn draw(&mut self, n: u32, inst: u32, first: u32) {
        unsafe { self.list.DrawIndexedInstanced(n, inst, first, 0, 0) }
    }
    #[inline]
    fn draw_indirect(&mut self, va: u64, count: u32) {
        unsafe {
            self.list.ExecuteIndirect(&self.setup.indirect, count, &self.setup.memory, va - self.setup.memory_va, None, 0);
        }
    }
}

/// One command list per recorded list, reused frame after frame.
pub struct Lists {
    slots: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)>,
}

unsafe impl Sync for Lists {}
unsafe impl Sync for Setup {}

impl Lists {
    pub fn new(setup: &Setup, n: usize) -> Result<Lists, String> {
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            unsafe {
                let a: ID3D12CommandAllocator = setup.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(err("allocator"))?;
                let l: ID3D12GraphicsCommandList = setup.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).map_err(err("list"))?;
                l.Close().map_err(err("close"))?;
                slots.push((a, l));
            }
        }
        Ok(Lists { slots })
    }
}

pub struct FrameTimes {
    /// first translation start -> last list closed
    pub wall_ms: f64,
    /// sum over lists of (reset + translate + record + close), across all threads
    pub thread_ms: f64,
    pub stats: Stats,
}

/// Translate and record `count` frames on a persistent pool of `threads` workers (started once,
/// released per frame by a barrier: no thread is created inside the timed region). Each frame is
/// submitted and waited for before the next.
pub fn run_frames(
    setup: &mut Setup,
    lists: &Lists,
    ctx: &Context,
    frames: &[Vec<Vec<Cmd>>],
    count: usize,
    threads: usize,
    record: bool,
) -> Result<Vec<FrameTimes>, String> {
    use std::sync::{Barrier, Mutex};
    let start = Barrier::new(threads + 1);
    let end = Barrier::new(threads + 1);
    let next = AtomicUsize::new(0);
    let current = AtomicUsize::new(0);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let results: Mutex<Vec<(f64, Stats)>> = Mutex::new(Vec::new());
    let mut out = Vec::with_capacity(count);
    let setup_ptr = setup as *mut Setup as usize;
    std::thread::scope(|scope| -> Result<(), String> {
        for _ in 0..threads {
            scope.spawn(|| {
                // SAFETY: workers only read the setup while the main thread waits at `end`
                let setup_ref: &Setup = unsafe { &*(setup_ptr as *const Setup) };
                loop {
                    start.wait();
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let frame = &frames[current.load(Ordering::Acquire)];
                    let (mut ms, mut st) = (0.0, Stats::default());
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= frame.len() {
                            break;
                        }
                        let t = Instant::now();
                        let s = if record {
                            let (a, l) = &lists.slots[i];
                            unsafe {
                                a.Reset().expect("allocator reset");
                                l.Reset(a, None).expect("list reset");
                            }
                            let s = translate(ctx, &frame[i], &mut D3dSink { list: l, setup: setup_ref });
                            unsafe { l.Close().expect("close") };
                            s
                        } else {
                            let mut c = crate::Count::default();
                            let s = translate(ctx, &frame[i], &mut c);
                            std::hint::black_box(c.checksum);
                            s
                        };
                        ms += t.elapsed().as_secs_f64() * 1000.0;
                        st.calls += s.calls;
                        st.draws += s.draws;
                        st.elided += s.elided;
                        st.refused += s.refused;
                    }
                    results.lock().unwrap().push((ms, st));
                    end.wait();
                }
            });
        }
        let mut failure = None;
        for f in 0..count {
            current.store(f % frames.len(), Ordering::Release);
            next.store(0, Ordering::Release);
            results.lock().unwrap().clear();
            let t = Instant::now();
            start.wait();
            end.wait();
            let wall_ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut stats = Stats::default();
            let mut thread_ms = 0.0;
            for (ms, s) in results.lock().unwrap().iter() {
                thread_ms += ms;
                stats.calls += s.calls;
                stats.draws += s.draws;
                stats.elided += s.elided;
                stats.refused += s.refused;
            }
            if record {
                // SAFETY: every worker is parked at `start`
                let setup = unsafe { &mut *(setup_ptr as *mut Setup) };
                let n = frames[f % frames.len()].len();
                let r = unsafe {
                    let cl: Vec<Option<ID3D12CommandList>> = lists.slots[..n].iter().map(|(_, l)| Some(l.cast().unwrap())).collect();
                    setup.queue.ExecuteCommandLists(&cl);
                    setup.fence_value += 1;
                    setup
                        .queue
                        .Signal(&setup.fence, setup.fence_value)
                        .and_then(|_| setup.fence.SetEventOnCompletion(setup.fence_value, HANDLE::default()))
                        .and_then(|_| setup.device.GetDeviceRemovedReason())
                };
                if let Err(e) = r {
                    failure = Some(format!("frame {f}: {e}"));
                    break;
                }
            }
            out.push(FrameTimes { wall_ms, thread_ms, stats });
        }
        stop.store(true, Ordering::Release);
        start.wait();
        failure.map_or(Ok(()), Err)
    })?;
    Ok(out)
}

impl Setup {
    /// Decision 2's per-frame cost: `n` console descriptors the title wrote this frame, each turned
    /// into a PC view at its slot (half buffer views, half texture views), from a dirty list — no
    /// scan. Returns milliseconds.
    pub fn write_descriptors(&self, slots: &[u32]) -> f64 {
        unsafe {
            let inc = self.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let base = self.heap.GetCPUDescriptorHandleForHeapStart().ptr;
            let t = Instant::now();
            for (k, &slot) in slots.iter().enumerate() {
                let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: base + slot as usize * inc };
                if k & 1 == 0 {
                    let mut d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_FLOAT,
                        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        ..Default::default()
                    };
                    d.Anonymous.Buffer = D3D12_BUFFER_SRV { FirstElement: (k as u64 * 64) % (1 << 20), NumElements: 64, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_NONE };
                    self.device.CreateShaderResourceView(&self.memory, Some(&d), at);
                } else {
                    let mut d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_BC1_UNORM,
                        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        ..Default::default()
                    };
                    d.Anonymous.Texture2D = D3D12_TEX2D_SRV { MostDetailedMip: (k % 4) as u32, MipLevels: 1, PlaneSlice: 0, ResourceMinLODClamp: 0.0 };
                    self.device.CreateShaderResourceView(&self.texture, Some(&d), at);
                }
            }
            t.elapsed().as_secs_f64() * 1000.0
        }
    }
}
