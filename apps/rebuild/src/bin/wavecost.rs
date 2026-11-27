//! The cost of lane doubling: real wave-wide compute programs from the corpus, translated twice, timed on the card.
//!
//! REBUILD_SHADERS=D:\victory-data\shaders.rpf cargo run --release --bin wavecost [-- --programs 8] [--dry]
//!
//! ```text
//! doubled    one host wave of 32 per console wave of 64; host lane i carries console lanes i and i + 32
//!            (rebuild_sir::lift::Wave::Doubled, what the translator emits)
//! two-wave   two host waves of 32 per console wave, exchanging halves through groupshared memory with barriers
//!            (Wave::TwoWave: the previous layer's design, for this measurement only)
//! ```
//!
//! Programs: wave64 compute, wave-wide, 64-thread groups (one console wave per group, so the two-wave version's
//! barriers are uniform), no loops (none can run away on the card), and both versions compile. Every
//! `ResourceDescriptorHeap[...]` access is pointed at a valid resource of its type (a fixed slot per type), so
//! no access is undefined; user data is zero. Both versions read and write the same resources, so the difference
//! is the wave mapping.
//!
//! The card only while `agents\MEASURING` is absent and under the lanes' lock (`agents\CARD`); every fence wait
//! is bounded at 5 s. `--dry`: select, translate and compile only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rebuild_pipelines::d3d::{CardLock, Gpu};
use rebuild_pipelines::translate::{compile_file, dxc_path};
use rebuild_pipelines::Stage as HostStage;
use rebuild_shader::census;
use rebuild_shader::features::{self, Findings, WaveClass};
use rebuild_shader::gfx10;
use rebuild_shader::native::StageIo;
use rebuild_sir::lift::{self, Stage, Wave};

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// HLSL resource type → heap slot.
const SLOTS: [&str; 16] = [
    "Texture1D<float4>",
    "Texture1DArray<float4>",
    "Texture2D<float4>",
    "Texture2DArray<float4>",
    "Texture3D<float4>",
    "TextureCube<float4>",
    "RWTexture1D<float4>",
    "RWTexture1DArray<float4>",
    "RWTexture2D<float4>",
    "RWTexture2DArray<float4>",
    "RWTexture3D<float4>",
    "ByteAddressBuffer",
    "RWByteAddressBuffer",
    "Buffer<float4>",
    "RWBuffer<float4>",
    "ConstantBuffer<SirCb>",
];

/// Points every heap access at its type's slot; `None` if a type has no slot.
fn bind_slots(src: &str) -> Option<String> {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find(" = ResourceDescriptorHeap[") {
        let head = &rest[..at];
        // the declaration's type: the text after the last '{' or ';' or newline, minus the variable name
        let start = head.rfind(['{', ';', '\n']).map(|k| k + 1).unwrap_or(0);
        let decl = head[start..].trim();
        let ty = decl.rsplit_once(' ')?.0.trim();
        let slot = SLOTS.iter().position(|s| *s == ty)?;
        let close = rest[at..].find("];")? + at;
        out.push_str(&rest[..at]);
        out.push_str(&format!(" = ResourceDescriptorHeap[{slot}u];"));
        rest = &rest[close + 2..];
    }
    out.push_str(rest);
    // samplers: one sampler at slot 0 of the sampler heap
    let mut s = String::new();
    let mut rest = out.as_str();
    while let Some(at) = rest.find("SamplerDescriptorHeap[") {
        let close = rest[at..].find(']')? + at;
        s.push_str(&rest[..at]);
        s.push_str("SamplerDescriptorHeap[0u]");
        rest = &rest[close + 1..];
    }
    s.push_str(rest);
    Some(s)
}

struct Card {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    root: ID3D12RootSignature,
    heap: ID3D12DescriptorHeap,
    samplers: ID3D12DescriptorHeap,
    ud: ID3D12Resource,
    timer: ID3D12QueryHeap,
    stamps: ID3D12Resource,
    _keep: Vec<ID3D12Resource>,
}

fn resource(device: &ID3D12Device, desc: D3D12_RESOURCE_DESC, kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }
        .expect("resource");
    r.unwrap()
}

fn buffer_desc(bytes: u64, uav: bool) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
        ..Default::default()
    }
}

fn texture_desc(dim: D3D12_RESOURCE_DIMENSION, w: u64, h: u32, d: u16) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: dim,
        Width: w,
        Height: h,
        DepthOrArraySize: d,
        MipLevels: 1,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    }
}

impl Card {
    fn new(device: ID3D12Device) -> Result<Card, String> {
        unsafe {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).map_err(|e| e.to_string())?;
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|e| e.to_string())?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(|e| e.to_string())?;
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| e.to_string())?;
            // root: user data (b0), the descriptor shadow (t0, space101), 16 static samplers (space100); heaps indexed directly
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: lift::SHADOW_SPACE } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
            ];
            let statics: Vec<D3D12_STATIC_SAMPLER_DESC> = (0..16)
                .map(|k| D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    MaxLOD: 1000.0,
                    ShaderRegister: k,
                    RegisterSpace: lift::IMMEDIATE_SAMPLER_SPACE,
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                    ..Default::default()
                })
                .collect();
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: 2,
                pParameters: params.as_ptr(),
                NumStaticSamplers: statics.len() as u32,
                pStaticSamplers: statics.as_ptr(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            };
            let mut blob: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None).map_err(|e| e.to_string())?;
            let b = blob.unwrap();
            let root: ID3D12RootSignature = device.CreateRootSignature(0, std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).map_err(|e| e.to_string())?;

            // resources: a 1D array, a 2D array of 6 (also the cube), a 3D texture, a 1 MiB buffer
            let t1 = resource(&device, texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE1D, 256, 1, 6), D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
            let t2 = resource(&device, texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 128, 128, 6), D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
            let t3 = resource(&device, texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE3D, 32, 32, 32), D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
            let buf = resource(&device, buffer_desc(1 << 20, true), D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
            // read-only copies for the SRVs (a resource is not read and written as both in one dispatch)
            let rs = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
            let s1 = resource(&device, texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE1D, 256, 1, 6), D3D12_HEAP_TYPE_DEFAULT, rs);
            let s2 = resource(&device, texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 128, 128, 6), D3D12_HEAP_TYPE_DEFAULT, rs);
            let s3 = resource(&device, texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE3D, 32, 32, 32), D3D12_HEAP_TYPE_DEFAULT, rs);
            let sbuf = resource(&device, buffer_desc(1 << 20, true), D3D12_HEAP_TYPE_DEFAULT, rs);
            let ud = resource(&device, buffer_desc(64 << 10, false), D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            ud.Map(0, None, Some(&mut p)).unwrap();
            std::ptr::write_bytes(p as *mut u8, 0, 64 << 10);
            ud.Unmap(0, None);

            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 16, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
                .map_err(|e| e.to_string())?;
            let inc = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let at = |k: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + k * inc };
            let fmt = DXGI_FORMAT_R16G16B16A16_FLOAT;
            let srv = |dim, f: &dyn Fn(&mut D3D12_SHADER_RESOURCE_VIEW_DESC)| {
                let mut d = D3D12_SHADER_RESOURCE_VIEW_DESC { Format: fmt, ViewDimension: dim, Shader4ComponentMapping: 0x1688, ..Default::default() };
                f(&mut d);
                d
            };
            let views: [(Option<&ID3D12Resource>, D3D12_SHADER_RESOURCE_VIEW_DESC); 6] = [
                (Some(&s1), srv(D3D12_SRV_DIMENSION_TEXTURE1D, &|d| d.Anonymous.Texture1D = D3D12_TEX1D_SRV { MipLevels: 1, ..Default::default() })),
                (Some(&s1), srv(D3D12_SRV_DIMENSION_TEXTURE1DARRAY, &|d| d.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_SRV { MipLevels: 1, ArraySize: 6, ..Default::default() })),
                (Some(&s2), srv(D3D12_SRV_DIMENSION_TEXTURE2D, &|d| d.Anonymous.Texture2D = D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() })),
                (Some(&s2), srv(D3D12_SRV_DIMENSION_TEXTURE2DARRAY, &|d| d.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_SRV { MipLevels: 1, ArraySize: 6, ..Default::default() })),
                (Some(&s3), srv(D3D12_SRV_DIMENSION_TEXTURE3D, &|d| d.Anonymous.Texture3D = D3D12_TEX3D_SRV { MipLevels: 1, ..Default::default() })),
                (Some(&s2), srv(D3D12_SRV_DIMENSION_TEXTURECUBE, &|d| d.Anonymous.TextureCube = D3D12_TEXCUBE_SRV { MipLevels: 1, ..Default::default() })),
            ];
            for (k, (r, d)) in views.iter().enumerate() {
                device.CreateShaderResourceView(*r, Some(d), at(k));
            }
            let uav = |dim, f: &dyn Fn(&mut D3D12_UNORDERED_ACCESS_VIEW_DESC)| {
                let mut d = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: fmt, ViewDimension: dim, ..Default::default() };
                f(&mut d);
                d
            };
            let uavs: [(&ID3D12Resource, D3D12_UNORDERED_ACCESS_VIEW_DESC); 5] = [
                (&t1, uav(D3D12_UAV_DIMENSION_TEXTURE1D, &|_| {})),
                (&t1, uav(D3D12_UAV_DIMENSION_TEXTURE1DARRAY, &|d| d.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV { ArraySize: 6, ..Default::default() })),
                (&t2, uav(D3D12_UAV_DIMENSION_TEXTURE2D, &|_| {})),
                (&t2, uav(D3D12_UAV_DIMENSION_TEXTURE2DARRAY, &|d| d.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV { ArraySize: 6, ..Default::default() })),
                (&t3, uav(D3D12_UAV_DIMENSION_TEXTURE3D, &|d| d.Anonymous.Texture3D = D3D12_TEX3D_UAV { WSize: 32, ..Default::default() })),
            ];
            for (k, (r, d)) in uavs.iter().enumerate() {
                device.CreateUnorderedAccessView(*r, None, Some(d), at(6 + k));
            }
            let mut raw = D3D12_SHADER_RESOURCE_VIEW_DESC { Format: DXGI_FORMAT_R32_TYPELESS, ViewDimension: D3D12_SRV_DIMENSION_BUFFER, Shader4ComponentMapping: 0x1688, ..Default::default() };
            raw.Anonymous.Buffer = D3D12_BUFFER_SRV { NumElements: (1 << 18) as u32, Flags: D3D12_BUFFER_SRV_FLAG_RAW, ..Default::default() };
            device.CreateShaderResourceView(&sbuf, Some(&raw), at(11));
            let mut rawu = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: DXGI_FORMAT_R32_TYPELESS, ViewDimension: D3D12_UAV_DIMENSION_BUFFER, ..Default::default() };
            rawu.Anonymous.Buffer = D3D12_BUFFER_UAV { NumElements: (1 << 18) as u32, Flags: D3D12_BUFFER_UAV_FLAG_RAW, ..Default::default() };
            device.CreateUnorderedAccessView(&buf, None, Some(&rawu), at(12));
            let mut typed = D3D12_SHADER_RESOURCE_VIEW_DESC { Format: DXGI_FORMAT_R32G32B32A32_FLOAT, ViewDimension: D3D12_SRV_DIMENSION_BUFFER, Shader4ComponentMapping: 0x1688, ..Default::default() };
            typed.Anonymous.Buffer = D3D12_BUFFER_SRV { NumElements: (1 << 16) as u32, ..Default::default() };
            device.CreateShaderResourceView(&sbuf, Some(&typed), at(13));
            let mut typedu = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: DXGI_FORMAT_R32G32B32A32_FLOAT, ViewDimension: D3D12_UAV_DIMENSION_BUFFER, ..Default::default() };
            typedu.Anonymous.Buffer = D3D12_BUFFER_UAV { NumElements: (1 << 16) as u32, ..Default::default() };
            device.CreateUnorderedAccessView(&buf, None, Some(&typedu), at(14));
            device.CreateConstantBufferView(Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: ud.GetGPUVirtualAddress(), SizeInBytes: 64 << 10 }), at(15));

            let samplers: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, NumDescriptors: 1, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
                .map_err(|e| e.to_string())?;
            device.CreateSampler(
                &D3D12_SAMPLER_DESC {
                    Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    MaxLOD: 1000.0,
                    ..Default::default()
                },
                samplers.GetCPUDescriptorHandleForHeapStart(),
            );
            let mut timer: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2, NodeMask: 0 }, &mut timer).map_err(|e| e.to_string())?;
            let stamps = resource(&device, buffer_desc(16, false), D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
            Ok(Card { device, queue, alloc, list, fence, value: 0, root, heap, samplers, ud, timer: timer.unwrap(), stamps, _keep: vec![t1, t2, t3, buf, s1, s2, s3, sbuf] })
        }
    }

    /// Dispatches `groups` groups `reps` times in one list; GPU microseconds. Every wait is bounded at 5 s.
    fn time(&mut self, pso: &ID3D12PipelineState, groups: u32, reps: u32) -> Result<f64, String> {
        unsafe {
            self.alloc.Reset().unwrap();
            self.list.Reset(&self.alloc, None).unwrap();
            let l = &self.list;
            l.SetDescriptorHeaps(&[Some(self.heap.clone()), Some(self.samplers.clone())]);
            l.SetPipelineState(pso);
            l.SetComputeRootSignature(&self.root);
            l.SetComputeRootConstantBufferView(0, self.ud.GetGPUVirtualAddress());
            l.SetComputeRootShaderResourceView(1, self.ud.GetGPUVirtualAddress());
            l.EndQuery(&self.timer, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            for _ in 0..reps {
                l.Dispatch(groups, 1, 1);
            }
            l.EndQuery(&self.timer, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            l.ResolveQueryData(&self.timer, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.stamps, 0);
            l.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).unwrap();
            let ev = CreateEventW(None, false, false, None).unwrap();
            self.fence.SetEventOnCompletion(self.value, ev).unwrap();
            let r = WaitForSingleObject(ev, 5000);
            let _ = CloseHandle(ev);
            if r != WAIT_OBJECT_0 {
                return Err("a fence wait passed 5 s".into());
            }
            self.device.GetDeviceRemovedReason().map_err(|e| format!("device removed: {e}"))?;
            let freq = self.queue.GetTimestampFrequency().map_err(|e| e.to_string())? as f64;
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            self.stamps.Map(0, None, Some(&mut p)).unwrap();
            let t = std::slice::from_raw_parts(p as *const u64, 2);
            let us = t[1].saturating_sub(t[0]) as f64 / freq * 1e6;
            self.stamps.Unmap(0, None);
            Ok(us)
        }
    }

    fn pipeline(&self, dxil: &[u8]) -> Result<ID3D12PipelineState, String> {
        unsafe {
            self.device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&self.root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr().cast(), BytecodeLength: dxil.len() },
                    ..Default::default()
                })
                .map_err(|e| format!("pipeline: {e}"))
        }
    }
}

struct Pick {
    hash: String,
    /// cross-lane operations in the doubled source (ballots and lane reads)
    cross: usize,
    dxil: [Vec<u8>; 2],
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want: usize = args.iter().position(|a| a == "--programs").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(8);
    let dry = args.iter().any(|a| a == "--dry");
    let path = std::env::var("REBUILD_SHADERS").expect("REBUILD_SHADERS: a copy of shaders.rpf (never the game folder)");
    let data = std::fs::read(&path).expect("read shaders.rpf");
    let walk = census::walk(&data, 4);
    let dxc = dxc_path().expect("dxc.exe");
    let dir = std::env::temp_dir().join("rebuild-wavecost");
    std::fs::create_dir_all(&dir).unwrap();
    let next = AtomicUsize::new(0);
    let picks: Mutex<Vec<Pick>> = Mutex::new(Vec::new());
    let tried = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(p) = walk.programs.get(i) else { break };
                let StageIo::Cs(io) = &p.native.io else { continue };
                if !p.native.wave64 || io.threads.iter().map(|t| (*t).max(1)).product::<u32>() != 64 {
                    continue;
                }
                let row = features::analyse(&p.native, &mut Findings::default());
                if row.decode_error.is_some() || row.class() == WaveClass::Agnostic || row.loops > 0 {
                    continue;
                }
                let Ok(insts) = gfx10::decode(&p.native.patched_code()) else { continue };
                if Wave::choose(row.class(), true, true, &insts) != Wave::Doubled {
                    continue;
                }
                let bind = lift::Bindings { hw: p.native.stage, layout: &p.native.layout, used: &p.native.used };
                let mut srcs = Vec::new();
                for w in [Wave::Doubled, Wave::TwoWave] {
                    let l = lift::lift_program_wave(&insts, Stage::Cs(*io), Some(bind), true, w);
                    let Ok(src) = lift::cs_program(&l, io, Some(&bind)) else { break };
                    if src.contains("for (;;)") || src.contains("SamplerComparisonState") {
                        break;
                    }
                    let Some(src) = bind_slots(&src) else { break };
                    srcs.push(src);
                }
                if srcs.len() != 2 {
                    continue;
                }
                tried.fetch_add(1, Ordering::Relaxed);
                let cross = srcs[0].matches("WaveActiveBallot").count() + srcs[0].matches("WaveReadLane").count() + srcs[0].matches("sir_r").count();
                let mut dxil = Vec::new();
                for (k, src) in srcs.iter().enumerate() {
                    let hl = dir.join(format!("{}.{k}.hlsl", p.hash));
                    let out = dir.join(format!("{}.{k}.dxil", p.hash));
                    std::fs::write(&hl, src).unwrap();
                    if compile_file(&dxc, HostStage::Cs, &hl, &out).is_err() {
                        break;
                    }
                    dxil.push(std::fs::read(&out).unwrap());
                }
                if let Ok(d) = <[Vec<u8>; 2]>::try_from(dxil) {
                    picks.lock().unwrap().push(Pick { hash: p.hash.clone(), cross, dxil: d });
                }
            });
        }
    });
    let mut picks = picks.into_inner().unwrap();
    // the most cross-lane work first
    picks.sort_by(|a, b| b.cross.cmp(&a.cross).then(a.hash.cmp(&b.hash)));
    picks.truncate(want);
    println!(
        "wavecost — {} candidates (wave64 compute, wave-wide, 64-thread groups, no loops, both versions translated); {} compiled, timing {}",
        tried.load(Ordering::Relaxed),
        picks.len(),
        picks.len().min(want)
    );
    if dry {
        for p in &picks {
            println!("  {}  cross-lane ops {}", p.hash, p.cross);
        }
        return;
    }
    let _lock = match CardLock::take() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("wavecost: not on the card: {e}");
            std::process::exit(3);
        }
    };
    let gpu = Gpu::new(false).expect("device");
    let mut card = Card::new(gpu.device.clone()).expect("card setup");
    let (groups, reps) = (2048u32, 8u32);
    println!("  {groups} groups of 64 console threads × {reps} dispatches; median of 5; GPU µs");
    println!("  program            cross   doubled   two-wave   two-wave / doubled");
    let mut ratios = Vec::new();
    for p in &picks {
        let psos: Vec<_> = match p.dxil.iter().map(|d| card.pipeline(d)).collect::<Result<Vec<_>, _>>() {
            Ok(v) => v,
            Err(e) => {
                println!("  {}  {e}", p.hash);
                continue;
            }
        };
        // one small dispatch of each first: it must finish well inside the bound
        let mut ok = true;
        for pso in &psos {
            if let Err(e) = card.time(pso, 1, 1) {
                println!("  {}  stopped: {e}", p.hash);
                ok = false;
                break;
            }
        }
        if !ok {
            break;
        }
        let mut t = [Vec::new(), Vec::new()];
        for _ in 0..5 {
            for (k, pso) in psos.iter().enumerate() {
                match card.time(pso, groups, reps) {
                    Ok(us) => t[k].push(us),
                    Err(e) => {
                        println!("  {}  stopped: {e}", p.hash);
                        // the card lock goes first: process::exit skips destructors
                        drop(_lock);
                        std::process::exit(1);
                    }
                }
            }
        }
        let med = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let (d, w) = (med(&mut t[0]), med(&mut t[1]));
        ratios.push(w / d);
        println!("  {}  {:>5}   {:>7.1}   {:>8.1}   {:>6.2}×", p.hash, p.cross, d, w, w / d);
    }
    if !ratios.is_empty() {
        let geo = (ratios.iter().map(|r| r.ln()).sum::<f64>() / ratios.len() as f64).exp();
        println!("  geometric mean: two-wave takes {geo:.2}× the doubled time ({:+.1} %)", (geo - 1.0) * 100.0);
    }
}
