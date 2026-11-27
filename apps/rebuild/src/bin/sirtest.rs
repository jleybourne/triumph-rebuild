//! The shader core on the card: random IR programs → HLSL → DXC → the GPU, every lane of every
//! output and every word of memory compared with the 64-lane console interpreter (`sir::lanes`).
//!
//! Memory is Build 1's: two reserved 4 GiB windows with the test pages mapped as 64 KiB tiles, read
//! and written by the shaders through `ResourceDescriptorHeap[window]` at 32-bit offsets.
//!
//! cargo run --release --bin sirtest [-- --programs N]
//! DXC: the newest `dxc.exe` in the Windows SDK, or `REBUILD_DXC`.

use std::path::PathBuf;
use std::process::Command;

use rebuild_sir::classify::{classify, Class};
use rebuild_sir::gen::{self, Rng, MAPPED};
use rebuild_sir::hlsl::{self, Mode};
use rebuild_sir::lanes::{self, Memory};
use rebuild_sir::Op;
use rebuild_uma::tiles::Tiles;
use rebuild_uma::{Uma, PAGE, WINDOW};

use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const PAGE_BYTES: usize = 64 << 10;

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

fn map_bytes(r: &ID3D12Resource) -> *mut u8 {
    let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { r.Map(0, None, Some(&mut p)) }.expect("map");
    p as *mut u8
}

struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    signature: ID3D12RootSignature,
    heap: ID3D12DescriptorHeap,
    tiles: Tiles,
    input: ID3D12Resource,
    output: ID3D12Resource,
    back: ID3D12Resource,
    pages_up: ID3D12Resource,
    pages_back: ID3D12Resource,
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

            // root SRV (inputs), root UAV (outputs); the windows through the heap, directly indexed
            let view = |kind, reg| D3D12_ROOT_PARAMETER {
                ParameterType: kind,
                Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: reg, RegisterSpace: 0 } },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            };
            let params = [view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0), view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0)];
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: 2,
                pParameters: params.as_ptr(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
                ..Default::default()
            };
            let mut blob: Option<ID3DBlob> = None;
            let mut errors: Option<ID3DBlob> = None;
            if let Err(e) = D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, Some(&mut errors)) {
                let msg = errors.map(|b| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned());
                panic!("signature: {e} {msg:?}");
            }
            let b = blob.unwrap();
            let bytes = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
            let signature: ID3D12RootSignature = device.CreateRootSignature(0, bytes).expect("signature");

            // Build 1: two windows, the test pages mapped
            let mut tiles = Tiles::new(&device, &queue, 2).expect("tiles");
            let mut uma = Uma::new(2, 64 << 20);
            for &(w, page) in &MAPPED {
                let (_, mapped) = uma.place(w as u64 * WINDOW as u64 + page as u64 * PAGE as u64, PAGE).expect("place");
                tiles.map(&mapped).expect("map tiles");
            }
            // one raw UAV per window at heap slot = window index
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 2, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
                .unwrap();
            let inc = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            for (w, win) in tiles.windows.iter().enumerate() {
                let mut u = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: DXGI_FORMAT_R32_TYPELESS, ViewDimension: D3D12_UAV_DIMENSION_BUFFER, ..Default::default() };
                u.Anonymous.Buffer = D3D12_BUFFER_UAV { FirstElement: 0, NumElements: (WINDOW / 4) as u32, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_RAW };
                let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + w * inc };
                device.CreateUnorderedAccessView(win, None, Some(&u), at);
            }
            let input = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, 4096, false, D3D12_RESOURCE_STATE_GENERIC_READ);
            let output = buffer(&device, D3D12_HEAP_TYPE_DEFAULT, 4096, true, D3D12_RESOURCE_STATE_COMMON);
            let back = buffer(&device, D3D12_HEAP_TYPE_READBACK, 4096, false, D3D12_RESOURCE_STATE_COPY_DEST);
            let pages_up = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, (MAPPED.len() * PAGE_BYTES) as u64, false, D3D12_RESOURCE_STATE_GENERIC_READ);
            let pages_back = buffer(&device, D3D12_HEAP_TYPE_READBACK, (MAPPED.len() * PAGE_BYTES) as u64, false, D3D12_RESOURCE_STATE_COPY_DEST);
            let mut g = Gpu { device, queue, alloc, list, fence, value: 0, signature, heap, tiles, input, output, back, pages_up, pages_back };
            g.submit(|_| {});
            g
        }
    }

    fn submit(&mut self, f: impl FnOnce(&ID3D12GraphicsCommandList)) {
        unsafe {
            self.alloc.Reset().unwrap();
            self.list.Reset(&self.alloc, None).unwrap();
            f(&self.list);
            self.list.Close().unwrap();
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).unwrap();
            self.fence.SetEventOnCompletion(self.value, HANDLE::default()).unwrap();
        }
    }

    fn page_offset(w: u32, page: u32) -> (usize, u64) {
        (w as usize, page as u64 * PAGE as u64)
    }

    /// Run one program against `mem`: upload its pages, dispatch, read outputs and pages back.
    fn run(&mut self, dxil: &[u8], input: &[u8], outputs: usize, mem: &Memory) -> Result<(Vec<[u32; 64]>, Vec<u32>), String> {
        unsafe {
            let pso: ID3D12PipelineState = self
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&self.signature),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr().cast(), BytecodeLength: dxil.len() },
                    ..Default::default()
                })
                .map_err(|e| format!("pipeline: {e}"))?;
            std::ptr::copy_nonoverlapping(input.as_ptr(), map_bytes(&self.input), input.len());
            self.input.Unmap(0, None);
            // the pages' starting contents, as the interpreter has them
            let up = map_bytes(&self.pages_up) as *mut u32;
            for (k, &(w, page)) in MAPPED.iter().enumerate() {
                let base = (w as u64) << 32 | (page as u64) << 16;
                for i in 0..PAGE_BYTES / 4 {
                    up.add(k * PAGE_BYTES / 4 + i).write(mem.initial(base + i as u64 * 4));
                }
            }
            self.pages_up.Unmap(0, None);
            let (wins, up_res) = (self.tiles.windows.clone(), self.pages_up.clone());
            self.submit(|l| {
                for (k, &(w, page)) in MAPPED.iter().enumerate() {
                    let (wi, off) = Self::page_offset(w, page);
                    l.CopyBufferRegion(&wins[wi], off, &up_res, (k * PAGE_BYTES) as u64, PAGE_BYTES as u64);
                }
            });
            let (sig, heap, inp, out) = (self.signature.clone(), self.heap.clone(), self.input.clone(), self.output.clone());
            self.submit(|l| {
                l.SetPipelineState(&pso);
                l.SetDescriptorHeaps(&[Some(heap.clone())]);
                l.SetComputeRootSignature(&sig);
                l.SetComputeRootShaderResourceView(0, inp.GetGPUVirtualAddress());
                l.SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
                l.Dispatch(1, 1, 1);
            });
            let (back, pages_back) = (self.back.clone(), self.pages_back.clone());
            self.submit(|l| {
                l.CopyBufferRegion(&back, 0, &out, 0, (outputs * 256) as u64);
                for (k, &(w, page)) in MAPPED.iter().enumerate() {
                    let (wi, off) = Self::page_offset(w, page);
                    l.CopyBufferRegion(&pages_back, (k * PAGE_BYTES) as u64, &wins[wi], off, PAGE_BYTES as u64);
                }
            });
            self.device.GetDeviceRemovedReason().map_err(|e| format!("device removed: {e}"))?;
            let words = std::slice::from_raw_parts(map_bytes(&self.back) as *const u32, outputs * 64);
            let outs = (0..outputs).map(|k| words[k * 64..k * 64 + 64].try_into().unwrap()).collect();
            self.back.Unmap(0, None);
            let pages = std::slice::from_raw_parts(map_bytes(&self.pages_back) as *const u32, MAPPED.len() * PAGE_BYTES / 4).to_vec();
            self.pages_back.Unmap(0, None);
            Ok((outs, pages))
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
    println!("memory    2 tiled windows (Build 1), pages {MAPPED:?} mapped, page 5 never");

    let mut rng = Rng(0xC0FF_EE11_2233_4455);
    let (mut agnostic, mut doubled, mut wrong) = (0usize, 0usize, 0usize);
    let (mut host_branches, mut predicated) = (0usize, 0usize);
    let (mut loads, mut stores, mut halves) = (0usize, 0usize, 0usize);
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
        for o in &p.insts {
            match o {
                Op::Load(..) => loads += 1,
                Op::Store(..) => stores += 1,
                Op::PkAddF16(..) | Op::PkMulF16(..) => halves += 1,
                _ => {}
            }
        }
        let exec = gen::exec(&mut rng);
        let (sin, vin) = gen::inputs(&mut rng);
        let mem0 = gen::memory(&mut rng);
        let mut mem = mem0.clone();
        let want = lanes::console_mem(&p, exec, &sin, &vin, &mut mem);
        let want_pages: Vec<u32> = MAPPED
            .iter()
            .flat_map(|&(w, page)| {
                let base = (w as u64) << 32 | (page as u64) << 16;
                let m = &mem;
                (0..PAGE_BYTES / 4).map(move |i| m.load((base + i as u64 * 4) as u32, w))
            })
            .collect();
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
            let (got, pages) = match gpu.run(&dxil, &input, p.outputs.len(), &mem0) {
                Ok(g) => g,
                Err(e) => {
                    println!("FAILED    program {n} ({mode:?}): {e}\n{src}");
                    std::process::exit(1);
                }
            };
            let out_ok = got == want;
            let mem_ok = pages == want_pages;
            if !out_ok || !mem_ok {
                wrong += 1;
                if wrong <= 3 {
                    if !out_ok {
                        let (k, l) = (0..want.len()).flat_map(|k| (0..64).map(move |l| (k, l))).find(|&(k, l)| got[k][l] != want[k][l]).unwrap();
                        println!("WRONG     program {n} ({mode:?}) exec {exec:#018x}: output {k} lane {l}: got {:#x}, want {:#x}", got[k][l], want[k][l]);
                    } else {
                        let i = pages.iter().zip(&want_pages).position(|(a, b)| a != b).unwrap();
                        println!("WRONG     program {n} ({mode:?}): memory word {i} (page {:?}): got {:#x}, want {:#x}", MAPPED[i / (PAGE_BYTES / 4)], pages[i], want_pages[i]);
                    }
                    println!("{src}");
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
    println!("ops       {loads} loads, {stores} stores through the windows (mapped and unmapped pages, uniform and varying windows) · {halves} packed half-precision ops");
    if wrong == 0 {
        println!("PROVEN    every lane of every output and every word of the 4 mapped pages match the 64-lane console interpreter, on the card");
        println!("PROVEN    console addresses are window offsets: no page directory; unmapped tiles read 0 and drop writes");
        println!("PROVEN    native half precision (float16_t) matches round-to-nearest-even bit for bit");
        println!("PROVEN    live masks: host branches and predicated regions both exact, stores masked in both");
    } else {
        println!("FAILED    {wrong} runs differ");
        std::process::exit(1);
    }
}
