//! The shader core on the card: random IR programs → HLSL → DXC → the GPU, every lane compared with
//! the 64-lane console interpreter (`sir::lanes::console`).
//!
//! cargo run --release --bin sirtest [-- --programs N]
//! DXC: the newest `dxc.exe` in the Windows SDK, or `REBUILD_DXC`.

use std::path::PathBuf;
use std::process::Command;

use rebuild_sir::classify::{classify, Class};
use rebuild_sir::gen::{self, Rng};
use rebuild_sir::hlsl::{self, Mode};
use rebuild_sir::lanes;

use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

fn dxc() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("REBUILD_DXC") {
        return Ok(p.into());
    }
    let root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&root)
        .map_err(|e| format!("Windows SDK: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.path().join("x64").join("dxc.exe")))
        .filter(|p| p.exists())
        .collect();
    versions.sort();
    versions.pop().ok_or_else(|| "no dxc.exe in the Windows SDK; set REBUILD_DXC".into())
}

fn compile(dxc: &PathBuf, src: &str, dir: &std::path::Path, n: usize) -> Result<Vec<u8>, String> {
    let hl = dir.join(format!("p{n}.hlsl"));
    let out = dir.join(format!("p{n}.dxil"));
    std::fs::write(&hl, src).map_err(|e| e.to_string())?;
    let r = Command::new(dxc)
        .args(["-T", "cs_6_6", "-E", "main", "-O3", "-enable-16bit-types", "-Fo"])
        .arg(&out)
        .arg(&hl)
        .output()
        .map_err(|e| format!("dxc: {e}"))?;
    if !r.status.success() {
        return Err(format!("dxc failed:\n{}\n{src}", String::from_utf8_lossy(&r.stderr)));
    }
    std::fs::read(&out).map_err(|e| e.to_string())
}

struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    signature: ID3D12RootSignature,
    input: ID3D12Resource,
    output: ID3D12Resource,
    back: ID3D12Resource,
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: u64, uav: bool, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
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
    unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }
        .expect("buffer");
    r.unwrap()
}

impl Gpu {
    fn new() -> Gpu {
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None::<&windows::core::IUnknown>, D3D_FEATURE_LEVEL_12_0, &mut device).expect("device");
            let device = device.unwrap();
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COMPUTE, ..Default::default() }).unwrap();
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE).unwrap();
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COMPUTE, &alloc, None).unwrap();
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let view = |kind, reg| D3D12_ROOT_PARAMETER {
                ParameterType: kind,
                Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: reg, RegisterSpace: 0 } },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            };
            let params = [view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0), view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0)];
            let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: 2, pParameters: params.as_ptr(), ..Default::default() };
            let mut blob: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None).expect("signature");
            let b = blob.unwrap();
            let bytes = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
            let signature: ID3D12RootSignature = device.CreateRootSignature(0, bytes).expect("signature");
            let input = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, 4096, false, D3D12_RESOURCE_STATE_GENERIC_READ);
            let output = buffer(&device, D3D12_HEAP_TYPE_DEFAULT, 4096, true, D3D12_RESOURCE_STATE_COMMON);
            let back = buffer(&device, D3D12_HEAP_TYPE_READBACK, 4096, false, D3D12_RESOURCE_STATE_COPY_DEST);
            Gpu { device, queue, alloc, list, fence, value: 0, signature, input, output, back }
        }
    }

    /// Run one program: returns `outputs` × 64 lanes.
    fn run(&mut self, dxil: &[u8], mode: Mode, input: &[u8], outputs: usize) -> Result<Vec<[u32; 64]>, String> {
        unsafe {
            let pso: ID3D12PipelineState = self
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&self.signature),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr().cast(), BytecodeLength: dxil.len() },
                    ..Default::default()
                })
                .map_err(|e| format!("pipeline: {e}"))?;
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            self.input.Map(0, None, Some(&mut p)).unwrap();
            std::ptr::copy_nonoverlapping(input.as_ptr(), p as *mut u8, input.len());
            self.input.Unmap(0, None);
            self.alloc.Reset().unwrap();
            self.list.Reset(&self.alloc, &pso).unwrap();
            self.list.SetComputeRootSignature(&self.signature);
            self.list.SetComputeRootShaderResourceView(0, self.input.GetGPUVirtualAddress());
            self.list.SetComputeRootUnorderedAccessView(1, self.output.GetGPUVirtualAddress());
            self.list.Dispatch(1, 1, 1);
            let _ = mode;
            self.list.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).unwrap();
            self.fence.SetEventOnCompletion(self.value, HANDLE::default()).unwrap();
            // copy out on the same queue
            self.alloc.Reset().unwrap();
            self.list.Reset(&self.alloc, None).unwrap();
            self.list.CopyBufferRegion(&self.back, 0, &self.output, 0, (outputs * 256) as u64);
            self.list.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).unwrap();
            self.fence.SetEventOnCompletion(self.value, HANDLE::default()).unwrap();
            self.device.GetDeviceRemovedReason().map_err(|e| format!("device removed: {e}"))?;
            self.back.Map(0, None, Some(&mut p)).unwrap();
            let words = std::slice::from_raw_parts(p as *const u32, outputs * 64);
            let out = (0..outputs).map(|k| words[k * 64..k * 64 + 64].try_into().unwrap()).collect();
            self.back.Unmap(0, None);
            Ok(out)
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let programs: usize = args.iter().position(|a| a == "--programs").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(200);
    let dxc = match dxc() {
        Ok(d) => d,
        Err(e) => {
            println!("FAILED    {e}");
            std::process::exit(1);
        }
    };
    let dir = std::env::temp_dir().join("rebuild-sirtest");
    std::fs::create_dir_all(&dir).unwrap();
    let mut gpu = Gpu::new();
    println!("sirtest — the shader core on the card: IR → HLSL → DXC → GPU, against the 64-lane interpreter");
    println!("dxc       {}", dxc.display());

    let mut rng = Rng(0xC0FF_EE11_2233_4455);
    let (mut agnostic, mut doubled, mut wrong) = (0usize, 0usize, 0usize);
    let (mut host_branches, mut predicated) = (0usize, 0usize);
    for n in 0..programs {
        // four kinds in turn: wave-wide or agnostic, straight-line or with nested branches
        let wave = n % 2 == 0;
        let branchy = n % 4 >= 2;
        let p = gen::program_with(&mut rng, 16, wave, branchy);
        for (_, low) in hlsl::plan(&p) {
            match low {
                hlsl::Lowering::HostBranch => host_branches += 1,
                hlsl::Lowering::Predicated => predicated += 1,
            }
        }
        let exec = gen::exec(&mut rng);
        let (sin, vin) = gen::inputs(&mut rng);
        let want = lanes::console(&p, exec, &sin, &vin);
        let mut input = Vec::new();
        for w in &vin {
            for v in w {
                input.extend_from_slice(&v.to_le_bytes());
            }
        }
        input.extend_from_slice(&sin[0].to_le_bytes());
        input.extend_from_slice(&[0u8; 12]);
        input.extend_from_slice(&(exec as u32).to_le_bytes());
        input.extend_from_slice(&((exec >> 32) as u32).to_le_bytes());
        let modes: &[Mode] = if classify(&p).0 == Class::Agnostic { &[Mode::Agnostic, Mode::Doubled] } else { &[Mode::Doubled] };
        for &mode in modes {
            let src = hlsl::emit(&p, mode);
            let dxil = match compile(&dxc, &src, &dir, n) {
                Ok(d) => d,
                Err(e) => {
                    println!("FAILED    program {n}: {e}");
                    std::process::exit(1);
                }
            };
            let got = match gpu.run(&dxil, mode, &input, p.outputs.len()) {
                Ok(g) => g,
                Err(e) => {
                    println!("FAILED    program {n} ({mode:?}): {e}\n{src}");
                    std::process::exit(1);
                }
            };
            // only active lanes' outputs carry meaning in the agnostic mode's wave ops… agnostic
            // programs have none, so every lane must match in both modes
            if got != want {
                wrong += 1;
                if wrong <= 3 {
                    let (k, l) = (0..want.len()).flat_map(|k| (0..64).map(move |l| (k, l))).find(|&(k, l)| got[k][l] != want[k][l]).unwrap();
                    println!("WRONG     program {n} ({mode:?}) exec {exec:#018x}: output {k} lane {l}: got {:#x}, want {:#x}\n{src}", got[k][l], want[k][l]);
                }
            }
            match mode {
                Mode::Agnostic => agnostic += 1,
                Mode::Doubled => doubled += 1,
            }
        }
    }
    println!("ran       {agnostic} agnostic (wave32, one console lane per thread) + {doubled} doubled (two console lanes per thread, EXEC as data)");
    println!("branches  {host_branches} lowered to host branches (uniform condition, or no cross-lane op inside) · {predicated} predicated (EXEC as data)");
    if wrong == 0 {
        println!("PROVEN    every lane of every output matches the 64-lane console interpreter, on the card");
        println!("PROVEN    a per-lane WaveReadLaneAt index (the doubled swizzle) is exact on this GPU");
        println!("PROVEN    live masks: host branches and predicated regions both exact, nested up to depth 2");
    } else {
        println!("FAILED    {wrong} runs differ");
        std::process::exit(1);
    }
}
