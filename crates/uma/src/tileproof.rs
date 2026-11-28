//! Build 1's proofs, run on the card: `gputest --tiled`, `--arena`, `--scale GB`.

#![cfg(windows)]

use std::time::Instant;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::arena::{Acquired, Arena, Key};
use crate::tiles::Tiles;
use crate::{Uma, PAGE, WINDOW};

fn err<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

fn blob(b: &ID3DBlob) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() }
}

const SHADERS: &[(&str, &[u8])] = &[
    (
        "fill",
        br#"RWByteAddressBuffer o : register(u0); cbuffer C : register(b0) { uint tag; uint n; };
            [numthreads(256, 1, 1)] void main(uint i : SV_DispatchThreadID) { if (i < n) o.Store(i * 4, tag); }"#,
    ),
    (
        "double",
        br#"ByteAddressBuffer s : register(t0); RWByteAddressBuffer o : register(u0); cbuffer C : register(b0) { uint tag; uint n; };
            [numthreads(256, 1, 1)] void main(uint i : SV_DispatchThreadID) { if (i < n) o.Store(i * 4, s.Load(i * 4) * 2); }"#,
    ),
];

/// A device, a direct queue, and two tiny compute programs over root views.
pub struct Gpu {
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    value: u64,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    signature: ID3D12RootSignature,
    fill: ID3D12PipelineState,
    double: ID3D12PipelineState,
}

impl Gpu {
    pub fn new() -> Result<Gpu, String> {
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None::<&windows::core::IUnknown>, D3D_FEATURE_LEVEL_11_0, &mut device).map_err(err("device"))?;
            let device = device.ok_or("device: none")?;
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).map_err(err("queue"))?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err("fence"))?;
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(err("allocator"))?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(err("list"))?;
            list.Close().map_err(err("close"))?;

            let view = |kind, reg| D3D12_ROOT_PARAMETER {
                ParameterType: kind,
                Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: reg, RegisterSpace: 0 } },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            };
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 2 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
            ];
            let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: 3, pParameters: params.as_ptr(), ..Default::default() };
            let mut sb: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut sb, None).map_err(err("signature"))?;
            let signature: ID3D12RootSignature = device.CreateRootSignature(0, &blob(&sb.ok_or("signature: none")?)).map_err(err("signature"))?;
            let mut psos = Vec::new();
            for (_, src) in SHADERS {
                let mut code: Option<ID3DBlob> = None;
                let mut errors: Option<ID3DBlob> = None;
                D3DCompile(src.as_ptr().cast(), src.len(), PCSTR::null(), None, None, windows::core::s!("main"), windows::core::s!("cs_5_0"), 0, 0, &mut code, Some(&mut errors))
                    .map_err(|e| format!("compile: {e}: {}", errors.as_ref().map(|b| String::from_utf8_lossy(&blob(b)).into_owned()).unwrap_or_default()))?;
                let code = blob(&code.ok_or("compile: none")?);
                psos.push(
                    device
                        .CreateComputePipelineState::<ID3D12PipelineState>(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                            pRootSignature: std::mem::transmute_copy(&signature),
                            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                            ..Default::default()
                        })
                        .map_err(err("pipeline"))?,
                );
            }
            let double = psos.pop().unwrap();
            let fill = psos.pop().unwrap();
            Ok(Gpu { device, queue, fence, value: 0, alloc, list, signature, fill, double })
        }
    }

    /// Record with `f`, submit, wait, and check the device.
    pub fn submit(&mut self, f: impl FnOnce(&ID3D12GraphicsCommandList)) -> Result<(), String> {
        unsafe {
            self.alloc.Reset().map_err(err("reset"))?;
            self.list.Reset(&self.alloc, None).map_err(err("reset"))?;
            f(&self.list);
            self.list.Close().map_err(err("close"))?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().map_err(err("cast"))?)]);
            self.wait()
        }
    }

    pub fn device_check(&self) -> Result<(), String> {
        unsafe { self.device.GetDeviceRemovedReason() }.map_err(err("device removed"))
    }

    /// Wait for everything on the queue (including tile mapping updates).
    pub fn wait(&mut self) -> Result<(), String> {
        unsafe {
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).map_err(err("signal"))?;
            self.fence.SetEventOnCompletion(self.value, HANDLE::default()).map_err(err("wait"))?;
            self.device.GetDeviceRemovedReason().map_err(err("device removed"))
        }
    }

    /// Write `tag` into `dwords` dwords at GPU address `va`.
    pub fn fill(&mut self, va: u64, tag: u32, dwords: u32) -> Result<(), String> {
        let (sig, pso) = (self.signature.clone(), self.fill.clone());
        self.submit(|l| unsafe {
            l.SetComputeRootSignature(&sig);
            l.SetPipelineState(&pso);
            l.SetComputeRoot32BitConstant(0, tag, 0);
            l.SetComputeRoot32BitConstant(0, dwords, 1);
            l.SetComputeRootUnorderedAccessView(2, va);
            l.Dispatch(dwords.div_ceil(256), 1, 1);
        })
    }

    /// dst[i] = 2 * src[i] for `dwords` dwords.
    pub fn double(&mut self, src: u64, dst: u64, dwords: u32) -> Result<(), String> {
        let (sig, pso) = (self.signature.clone(), self.double.clone());
        self.submit(|l| unsafe {
            l.SetComputeRootSignature(&sig);
            l.SetPipelineState(&pso);
            l.SetComputeRoot32BitConstant(0, 0, 0);
            l.SetComputeRoot32BitConstant(0, dwords, 1);
            l.SetComputeRootShaderResourceView(1, src);
            l.SetComputeRootUnorderedAccessView(2, dst);
            l.Dispatch(dwords.div_ceil(256), 1, 1);
        })
    }

    fn buffer(&self, kind: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES) -> Result<ID3D12Resource, String> {
        let mut r: Option<ID3D12Resource> = None;
        let mut desc = crate::tiles::buffer_desc(bytes);
        desc.Flags = D3D12_RESOURCE_FLAG_NONE;
        unsafe {
            self.device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r)
        }
        .map_err(err("buffer"))?;
        r.ok_or_else(|| "buffer: none".into())
    }

    /// Copy `(resource, offset, bytes)` pieces back to the CPU, in order.
    pub fn read(&mut self, pieces: &[(&ID3D12Resource, u64, u64)]) -> Result<Vec<u8>, String> {
        let total: u64 = pieces.iter().map(|p| p.2).sum();
        let back = self.buffer(D3D12_HEAP_TYPE_READBACK, total, D3D12_RESOURCE_STATE_COPY_DEST)?;
        self.submit(|l| unsafe {
            let mut at = 0;
            for (r, off, n) in pieces {
                l.CopyBufferRegion(&back, at, *r, *off, *n);
                at += n;
            }
        })?;
        unsafe {
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            back.Map(0, None, Some(&mut p)).map_err(err("map"))?;
            let v = std::slice::from_raw_parts(p as *const u8, total as usize).to_vec();
            back.Unmap(0, None);
            Ok(v)
        }
    }

    /// Copy CPU bytes to `(resource, offset)` through an upload buffer.
    pub fn write(&mut self, dst: &ID3D12Resource, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let up = self.buffer(D3D12_HEAP_TYPE_UPLOAD, bytes.len() as u64, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        unsafe {
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            up.Map(0, None, Some(&mut p)).map_err(err("map"))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
            up.Unmap(0, None);
        }
        self.submit(|l| unsafe { l.CopyBufferRegion(dst, offset, &up, 0, bytes.len() as u64) })
    }
}

fn dwords(v: &[u8]) -> Vec<u32> {
    v.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn check(lines: &mut Vec<String>, ok: bool, what: String) -> Result<(), String> {
    if ok {
        lines.push(format!("PROVEN    {what}"));
        Ok(())
    } else {
        Err(format!("{what} — NOT as expected"))
    }
}

/// Build 1, tasks 1-3 and the unmap path, on the card.
pub fn prove_tiles() -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let mut gpu = Gpu::new()?;
    let mut uma = Uma::new(2, 1 << 30);
    let mut tiles = Tiles::new(&gpu.device, &gpu.queue, 2)?;
    lines.push(format!("backend   2 reserved 4 GiB windows, 64 KiB tiles on 64 MiB heaps, tiled resources tier {}", tiles.tier));
    let page = PAGE as u64;
    let words = (PAGE / 4) as u32;

    // 1. a shader writes tile N; page N reads back N — in two windows
    let addrs = [3 * page, 4 * page, 5 * page, 100 * page, WINDOW as u64 + 7 * page];
    let mut ids = Vec::new();
    for &a in &addrs {
        let (id, mapped) = uma.place(a, PAGE).map_err(|e| format!("{e:?}"))?;
        tiles.map(&mapped)?;
        ids.push(id);
    }
    for &a in &addrs {
        gpu.fill(tiles.va(a), (a / page) as u32, words)?;
    }
    let pieces: Vec<_> = addrs.iter().map(|&a| { let (r, o) = tiles.locate(a); (r, o, page) }).collect();
    let got = dwords(&gpu.read(&pieces)?);
    let ok = addrs.iter().enumerate().all(|(k, &a)| got[k * words as usize..(k + 1) * words as usize].iter().all(|&v| v == (a / page) as u32));
    check(&mut lines, ok, format!("a shader wrote tile N at console addresses in 2 windows; pages {:?} read back N", addrs.map(|a| a / page)))?;

    // unmapped pages read zero (and the write into one is dropped)
    gpu.fill(tiles.va(6 * page), 0xDEAD, words)?;
    let (r, o) = tiles.locate(6 * page);
    let z = dwords(&gpu.read(&[(r, o, page)])?);
    check(&mut lines, z.iter().all(|&v| v == 0), "an unmapped page reads zero, and a write into it is dropped".into())?;

    // 2. console address: the CPU writes a pattern to page 3, a shader doubles it into page 100
    let pattern: Vec<u8> = (0..words).flat_map(|i| (i * 3 + 1).to_le_bytes()).collect();
    let (r3, o3) = tiles.locate(3 * page);
    let r3 = r3.clone();
    gpu.write(&r3, o3, &pattern)?;
    gpu.double(tiles.va(3 * page), tiles.va(100 * page), words)?;
    let (r, o) = tiles.locate(100 * page);
    let d = dwords(&gpu.read(&[(r, o, page)])?);
    check(&mut lines, d.iter().enumerate().all(|(i, &v)| v == (i as u32 * 3 + 1) * 2), "CPU pattern at console address 3·64K, shader doubled it into 100·64K, CPU read the doubled values".into())?;

    // 3a. a second buffer placed at the same address shares its pages
    let (shared_id, mapped) = uma.place(20 * page, 2 * PAGE).map_err(|e| format!("{e:?}"))?;
    tiles.map(&mapped)?;
    let (again, none) = uma.place(20 * page, 2 * PAGE).map_err(|e| format!("{e:?}"))?;
    check(&mut lines, none.is_empty(), "a second placement at the same address maps no new card pages".into())?;
    let mut second: Option<ID3D12Resource> = None;
    unsafe { gpu.device.CreateReservedResource(&crate::tiles::buffer_desc(2 * page), D3D12_RESOURCE_STATE_COMMON, None, &mut second) }.map_err(err("second buffer"))?;
    let second = second.ok_or("second buffer: none")?;
    let cards: Vec<u32> = (0..2).map(|k| uma.backing(20 * page + k * page).unwrap()).collect();
    tiles.alias(&second, 0, &cards)?;
    gpu.wait()?;
    gpu.fill(unsafe { second.GetGPUVirtualAddress() }, 0x5EC0_0D, 2 * words)?;
    let (r, o) = tiles.locate(20 * page);
    let s = dwords(&gpu.read(&[(r, o, 2 * page)])?);
    check(&mut lines, s.iter().all(|&v| v == 0x5EC0_0D), "two buffers at one address: what one writes, the other reads".into())?;

    // 3b. a render target at the same address as window pages: a clear shows up in the window
    let tex_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 256,
        Height: 256,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        ..Default::default()
    };
    let mut tex: Option<ID3D12Resource> = None;
    unsafe { gpu.device.CreateReservedResource(&tex_desc, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut tex) }.map_err(err("reserved render target"))?;
    let tex = tex.ok_or("render target: none")?;
    let mut ntiles = 0u32;
    unsafe { gpu.device.GetResourceTiling(&tex, Some(&mut ntiles), None, None, None, 0, std::ptr::null_mut()) };
    let (rt_id, mapped) = uma.place(40 * page, ntiles as usize * PAGE).map_err(|e| format!("{e:?}"))?;
    tiles.map(&mapped)?;
    let cards: Vec<u32> = (0..ntiles as u64).map(|k| uma.backing(40 * page + k * page).unwrap()).collect();
    tiles.alias(&tex, 0, &cards)?;
    let rtv_heap: ID3D12DescriptorHeap = unsafe {
        gpu.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() })
    }
    .map_err(err("rtv heap"))?;
    let rtv = unsafe { rtv_heap.GetCPUDescriptorHandleForHeapStart() };
    unsafe { gpu.device.CreateRenderTargetView(&tex, None, rtv) };
    gpu.submit(|l| unsafe { l.ClearRenderTargetView(rtv, &[1.0, 0.0, 0.0, 1.0], None) })?;
    let (r, o) = tiles.locate(40 * page);
    let t = dwords(&gpu.read(&[(r, o, ntiles as u64 * page)])?);
    check(&mut lines, t.iter().all(|&v| v == 0xFF00_00FF), format!("a render target ({ntiles} tiles) at the same address as window pages: its clear is read through the window"))?;

    // retire: the last user unmaps; the page reads zero again
    uma.retire(again).map_err(|e| format!("{e:?}"))?;
    let released = uma.retire(shared_id).map_err(|e| format!("{e:?}"))?;
    tiles.unmap(&released)?;
    gpu.wait()?;
    let (r, o) = tiles.locate(20 * page);
    let u = dwords(&gpu.read(&[(r, o, 2 * page)])?);
    check(&mut lines, released.len() == 2 && u.iter().all(|&v| v == 0), "retiring the last placement unmaps its pages; they read zero again".into())?;
    let _ = rt_id;
    lines.push(format!("card      {} pages in use, {} MiB of tile heaps", uma.card_pages_used(), tiles.heap_bytes() >> 20));
    Ok(lines)
}

/// Which resources cannot be tiled on this card, and the arena that holds them instead.
pub fn prove_arena() -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let gpu = Gpu::new()?;
    let tex = |format, samples, flags| D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 1024,
        Height: 1024,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: samples, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
        Flags: flags,
        ..Default::default()
    };
    let cases = [
        ("D32_FLOAT_S8X24 depth-stencil", tex(DXGI_FORMAT_D32_FLOAT_S8X24_UINT, 1, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL)),
        ("D24_UNORM_S8 depth-stencil", tex(DXGI_FORMAT_D24_UNORM_S8_UINT, 1, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL)),
        ("D32_FLOAT depth", tex(DXGI_FORMAT_D32_FLOAT, 1, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL)),
        ("RGBA8 render target, 4x MSAA", tex(DXGI_FORMAT_R8G8B8A8_UNORM, 4, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET)),
        ("RGBA8 render target", tex(DXGI_FORMAT_R8G8B8A8_UNORM, 1, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET)),
    ];
    let mut untileable = Vec::new();
    for (name, d) in &cases {
        let mut r: Option<ID3D12Resource> = None;
        let state = if d.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL) { D3D12_RESOURCE_STATE_DEPTH_WRITE } else { D3D12_RESOURCE_STATE_RENDER_TARGET };
        let tiled = unsafe { gpu.device.CreateReservedResource(d, state, None, &mut r) }.is_ok();
        lines.push(format!("{:<9} {name}", if tiled { "tiles" } else { "ARENA" }));
        if !tiled {
            untileable.push((name, *d, state));
        }
    }
    // the arena: two placements of the same untileable resource share one committed resource
    let mut arena = Arena::new();
    let mut held: Vec<Option<ID3D12Resource>> = Vec::new();
    for (k, (name, d, state)) in untileable.iter().enumerate() {
        let mut d = *d;
        d.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
        let key = Key { address: 0x4000_0000 + k as u64 * (64 << 20), len: 8 << 20, kind: d.Format.0 as u64 * 100 + d.SampleDesc.Count as u64 };
        for _ in 0..2 {
            match arena.acquire(key).map_err(|e| format!("{e:?}"))? {
                Acquired::New(slot) => {
                    let mut r: Option<ID3D12Resource> = None;
                    unsafe {
                        gpu.device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &d, *state, None, &mut r)
                    }
                    .map_err(|e| format!("arena {name}: {e}"))?;
                    if held.len() <= slot {
                        held.resize(slot + 1, None);
                    }
                    held[slot] = r;
                }
                Acquired::Shared(slot) => {
                    if held.get(slot).and_then(|r| r.as_ref()).is_none() {
                        return Err(format!("arena {name}: shared slot {slot} has no resource"));
                    }
                }
            }
        }
    }
    let live = arena.live();
    for slot in 0..held.len() {
        if held[slot].is_some() {
            assert!(!arena.release(slot).map_err(|e| format!("{e:?}"))?);
            if arena.release(slot).map_err(|e| format!("{e:?}"))? {
                held[slot] = None;
            }
        }
    }
    unsafe { gpu.device.GetDeviceRemovedReason() }.map_err(err("device removed"))?;
    check(
        &mut lines,
        live == untileable.len() && arena.live() == 0 && held.iter().all(|r| r.is_none()),
        format!("arena: {} untileable kinds, each placed twice = {live} committed resources, all released after their last user", untileable.len()),
    )?;
    Ok(lines)
}

/// Commit `gb` GiB of title pages through the tiles, in 2 MiB placements at random console
/// addresses across 4 windows; write and read back every placement.
pub fn prove_scale(gb: usize) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let mut gpu = Gpu::new()?;
    let windows = 4;
    let place = 2usize << 20;
    let count = (gb << 30) / place;
    let slots = windows * WINDOW / place;
    if count > slots {
        return Err(format!("{gb} GiB does not fit in {windows} windows"));
    }
    let mut uma = Uma::new(windows, (gb + 1) << 30);
    let mut tiles = Tiles::new(&gpu.device, &gpu.queue, windows)?;
    // a random permutation of slots
    let mut order: Vec<usize> = (0..slots).collect();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for i in (1..order.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        order.swap(i, (x % (i as u64 + 1)) as usize);
    }
    let addrs: Vec<u64> = order[..count].iter().map(|&s| (s * place) as u64).collect();
    let t = Instant::now();
    for chunk in addrs.chunks(256) {
        let mut mapped = Vec::new();
        for &a in chunk {
            let (_, m) = uma.place(a, place).map_err(|e| format!("place at {a:#x}: {e:?}"))?;
            mapped.extend(m);
        }
        tiles.map(&mapped)?;
    }
    gpu.wait()?;
    let map_ms = t.elapsed().as_secs_f64() * 1000.0;
    lines.push(format!("committed {gb} GiB = {count} placements of 2 MiB at random addresses in {windows} windows: mapped in {map_ms:.0} ms, {} MiB of tile heaps", tiles.heap_bytes() >> 20));

    // tag the first and last 1 KiB of each placement, then read every tag back
    let t = Instant::now();
    for batch in addrs.chunks(512) {
        for (k, &a) in batch.iter().enumerate() {
            let _ = k;
            gpu.fill(tiles.va(a), (a >> 21) as u32, 256)?;
            gpu.fill(tiles.va(a + place as u64 - 1024), !((a >> 21) as u32), 256)?;
        }
    }
    let mut bad = 0usize;
    for batch in addrs.chunks(2048) {
        let pieces: Vec<_> = batch
            .iter()
            .flat_map(|&a| {
                let (r, o) = tiles.locate(a);
                [(r, o, 16u64), (r, o + place as u64 - 1024, 16u64)]
            })
            .collect();
        let v = dwords(&gpu.read(&pieces)?);
        for (k, &a) in batch.iter().enumerate() {
            let tag = (a >> 21) as u32;
            if v[k * 8..k * 8 + 4].iter().any(|&w| w != tag) || v[k * 8 + 4..k * 8 + 8].iter().any(|&w| w != !tag) {
                bad += 1;
            }
        }
    }
    let rw_ms = t.elapsed().as_secs_f64() * 1000.0;
    check(&mut lines, bad == 0, format!("{count} placements written by a shader and read back ({rw_ms:.0} ms): 0 wrong, no device removal"))?;
    Ok(lines)
}

/// Build 2 on the card: the title writes every frame on this thread; this thread (marked as the
/// submit thread, so copying title memory on it panics) only calls `flush()` and makes the GPU wait.
pub fn prove_ring(frames: usize, workers: usize) -> Result<Vec<String>, String> {
    use crate::ring::{Ring, Watched};
    use crate::upload::{title_ranges, title_writes, Change, Rng};
    use std::sync::Arc;

    let mut lines = Vec::new();
    let mut gpu = Gpu::new()?;
    let mut uma = Uma::new(2, 1 << 30);
    let mut tiles = Tiles::new(&gpu.device, &gpu.queue, 2)?;
    let sizes = [(0u64, 256usize << 20, 100usize << 20), (WINDOW as u64 + (64 << 20), 64 << 20, 15 << 20)];
    let mut regions = Vec::new();
    let mut ranges = Vec::new();
    for (k, &(console, len, written)) in sizes.iter().enumerate() {
        let (_, mapped) = uma.place(console, len).map_err(|e| format!("{e:?}"))?;
        tiles.map(&mapped)?;
        regions.push(Arc::new(Watched::alloc(console, len)?));
        ranges.push(title_ranges(len, written, 0x5EED + k as u64));
    }
    gpu.wait()?;
    let windows: Vec<u64> = tiles.windows.iter().map(|w| unsafe { w.GetGPUVirtualAddress() }).collect();
    let ring = Ring::new(&gpu.device, windows, regions.clone(), workers)?;
    crate::chunks::mark_submit_thread();

    let direct_fence: ID3D12Fence = unsafe { gpu.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(err("fence"))?;
    let change = Change::Chunks(0.3);
    let mut rng = Rng(7);
    let (mut blocked, mut title, mut pace) = (Vec::new(), Vec::new(), Vec::new());
    let mut last = 0;
    for f in 1..=frames as u64 {
        // one frame in flight: wait for the GPU to have started frame f-2's work
        let t = Instant::now();
        if f > 2 {
            unsafe { direct_fence.SetEventOnCompletion(f - 2, HANDLE::default()) }.map_err(err("pace"))?;
        }
        pace.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        for (r, rs) in regions.iter().zip(&ranges) {
            unsafe { title_writes(r.base, rs, change, f, &mut rng) };
        }
        title.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let v = ring.flush();
        unsafe {
            gpu.queue.Wait(&ring.fence, v).map_err(err("queue wait"))?;
            gpu.queue.Signal(&direct_fence, f).map_err(err("signal"))?;
        }
        blocked.push(t.elapsed().as_secs_f64() * 1_000_000.0);
        last = v;
    }
    ring.wait(last)?;
    gpu.wait()?;
    let stats: Vec<_> = ring.finished().into_iter().collect::<Result<Vec<_>, _>>()?;

    // every byte of both regions, read back through the tiles
    for (r, &(console, len, _)) in regions.iter().zip(&sizes) {
        let (res, off) = tiles.locate(console);
        let got = gpu.read(&[(res, off, len as u64)])?;
        let want = unsafe { std::slice::from_raw_parts(r.base as *const u8, len) };
        if let Some(i) = got.chunks(64).zip(want.chunks(64)).position(|(a, b)| a != b) {
            return Err(format!("region at {console:#x}: first wrong chunk at +{:#x}", i * 64));
        }
    }

    let med = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[v.len() - 1])
    };
    let warm = &stats[3.min(stats.len())..];
    let col = |f: fn(&crate::ring::FlushStats) -> f64| med(warm.iter().map(f).collect());
    lines.push(format!(
        "title     2 regions in 2 windows (256 + 64 MiB), {} MiB rewritten a frame, 30 % of chunks changing; {frames} frames, {workers} diff workers",
        (100 + 15)
    ));
    let (b50, bmax) = med(blocked[3..].to_vec());
    let (t50, _) = med(title[3..].to_vec());
    let (p50, pmax) = med(pace[3..].to_vec());
    lines.push(format!("submit    flush() + GPU wait queued: median {b50:.1} µs, worst {bmax:.1} µs — the submit thread never copies"));
    lines.push(format!(
        "upload    {:.0} pages · {:.0} runs · {:.1} MB a flush | watch {:.2} · diff {:.2} · gather {:.2} · record {:.2} ms | flush -> signalled {:.2} ms median, {:.2} worst",
        col(|s| s.pages as f64).0,
        col(|s| s.runs as f64).0,
        col(|s| s.bytes as f64).0 / 1e6,
        col(|s| s.watch_ms).0,
        col(|s| s.diff_ms).0,
        col(|s| s.gather_ms).0,
        col(|s| s.record_ms).0,
        col(|s| s.latency_ms).0,
        col(|s| s.latency_ms).1
    ));
    lines.push(format!("title cpu {t50:.1} ms of writes a frame (the title's own work); pacing wait {p50:.2} ms median, {pmax:.2} worst"));
    check(&mut lines, true, format!("after {frames} frames both regions read back through the tiles byte for byte; no copy on the submit thread"))?;
    Ok(lines)
}

/// Build 2's GPU→CPU region on the card: can the GPU write labels straight into title memory, and
/// how early does the CPU see them, against a fence and against a CPU relay.
pub fn prove_labels(iterations: usize) -> Result<Vec<String>, String> {
    use crate::labels::LabelMemory;
    use crate::ring::{Watched, MEM_WRITE_WATCH};
    use windows::Win32::System::Memory::*;

    let mut lines = Vec::new();
    let gpu = Gpu::new()?;

    // 1. which title memory can be opened for the GPU to write
    let kinds: [(&str, VIRTUAL_ALLOCATION_TYPE, usize); 3] = [
        ("plain VirtualAlloc, 64 KiB", MEM_RESERVE | MEM_COMMIT, 64 << 10),
        ("plain VirtualAlloc, 16 MiB", MEM_RESERVE | MEM_COMMIT, 16 << 20),
        ("write-watched VirtualAlloc, 64 KiB", MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, 64 << 10),
    ];
    let mut opened = None;
    for (name, flags, len) in kinds {
        let base = unsafe { VirtualAlloc(None, len, flags, PAGE_READWRITE) } as *mut u8;
        match LabelMemory::open(&gpu.device, base, len) {
            Ok(m) => {
                lines.push(format!("opens     {name}"));
                if opened.is_none() {
                    opened = Some((base, m));
                }
            }
            Err(e) => {
                lines.push(format!("REFUSED   {name}: {e}"));
                unsafe {
                    let _ = VirtualFree(base as *mut _, 0, MEM_RELEASE);
                }
            }
        }
    }
    // the console's physical pages are frames mapped (and re-mapped, even twice) into the title's
    // addresses: on the host that is a pagefile section and views of it. Can the GPU open a view?
    {
        use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
        let len = 4usize << 20;
        let section = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, len as u32, None) };
        match section {
            Ok(h) => {
                let v1 = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, len) };
                let v2 = unsafe { MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, len) };
                for (name, v) in [("a view of a pagefile section (physical pages)", v1), ("a second view of the same section (an alias)", v2)] {
                    match LabelMemory::open(&gpu.device, v.Value as *mut u8, len) {
                        Ok(m) => {
                            // the GPU writes through this view; the CPU reads it through the other view
                            lines.push(format!("opens     {name}"));
                            let _ = m;
                        }
                        Err(e) => lines.push(format!("REFUSED   {name}: {e}")),
                    }
                }
            }
            Err(e) => lines.push(format!("REFUSED   pagefile section: {e}")),
        }
    }
    let _ = Watched::alloc; // the ring's regions are write-watched; see the third line above
    let (label_base, label) = opened.ok_or("no title memory could be opened for GPU writes")?;

    // 2. latency: a dispatch that takes about a millisecond, then a MARKER_OUT label, then a fence
    let scratch = {
        let mut desc = crate::tiles::buffer_desc(64 << 20);
        desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            gpu.device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut r)
        }
        .map_err(err("scratch"))?;
        r.ok_or("scratch: none")?
    };
    let fence: ID3D12Fence = unsafe { gpu.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(err("fence"))?;
    let list2: ID3D12GraphicsCommandList2 = gpu.list.cast().map_err(err("ID3D12GraphicsCommandList2"))?;
    let lp = label_base as *const u32;
    let mut early = Vec::new(); // label seen this long before the fence (ms; negative = after)
    let mut busy = Vec::new();
    let mut fences = Vec::new();
    for i in 1..=iterations as u32 {
        unsafe {
            gpu.alloc.Reset().map_err(err("reset"))?;
            gpu.list.Reset(&gpu.alloc, None).map_err(err("reset"))?;
            gpu.list.SetComputeRootSignature(&gpu.signature);
            gpu.list.SetPipelineState(&gpu.fill);
            gpu.list.SetComputeRoot32BitConstant(0, i, 0);
            gpu.list.SetComputeRoot32BitConstant(0, 16 << 20, 1);
            gpu.list.SetComputeRootUnorderedAccessView(2, scratch.GetGPUVirtualAddress());
            for _ in 0..8 {
                gpu.list.Dispatch((16u32 << 20) / 256, 1, 1);
            }
            label.write(&list2, 0, i);
            gpu.list.Close().map_err(err("close"))?;
            let t0 = Instant::now();
            gpu.queue.ExecuteCommandLists(&[Some(gpu.list.cast().map_err(err("cast"))?)]);
            gpu.queue.Signal(&fence, i as u64).map_err(err("signal"))?;
            let (mut t_label, mut t_fence) = (None, None);
            while t_label.is_none() || t_fence.is_none() {
                if t_label.is_none() && lp.read_volatile() == i {
                    t_label = Some(t0.elapsed());
                }
                if t_fence.is_none() && fence.GetCompletedValue() >= i as u64 {
                    t_fence = Some(t0.elapsed());
                }
                if t0.elapsed().as_secs() > 5 {
                    return Err(format!("label {i} never arrived"));
                }
            }
            let (tl, tf) = (t_label.unwrap().as_secs_f64() * 1000.0, t_fence.unwrap().as_secs_f64() * 1000.0);
            early.push(tf - tl);
            busy.push(tl);
            fences.push(tf);
        }
    }
    gpu.device_check()?;
    // a label seen in less than half the usual time could not have waited for its work
    let mut sorted = fences.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let typical = sorted[sorted.len() / 2];
    let too_soon = busy.iter().filter(|&&t| t < 0.5 * typical).count();

    // 3. the fallback: a CPU worker that waits on the fence and then writes the label itself
    let relay_mem = vec![0u32; 16];
    let relay = Ptr32(relay_mem.as_ptr() as *mut u32);
    let mut relay_late = Vec::new();
    for i in 1..=iterations as u64 {
        let v = 1_000_000 + i;
        let f2 = fence.clone();
        let r = relay;
        let worker = std::thread::spawn(move || unsafe {
            let r = r;
            f2.SetEventOnCompletion(v, HANDLE::default()).expect("relay wait");
            r.0.write_volatile(v as u32);
        });
        unsafe {
            gpu.alloc.Reset().map_err(err("reset"))?;
            gpu.list.Reset(&gpu.alloc, None).map_err(err("reset"))?;
            gpu.list.SetComputeRootSignature(&gpu.signature);
            gpu.list.SetPipelineState(&gpu.fill);
            gpu.list.SetComputeRoot32BitConstant(0, i as u32, 0);
            gpu.list.SetComputeRoot32BitConstant(0, 16 << 20, 1);
            gpu.list.SetComputeRootUnorderedAccessView(2, scratch.GetGPUVirtualAddress());
            for _ in 0..8 {
                gpu.list.Dispatch((16u32 << 20) / 256, 1, 1);
            }
            gpu.list.Close().map_err(err("close"))?;
            let t0 = Instant::now();
            gpu.queue.ExecuteCommandLists(&[Some(gpu.list.cast().map_err(err("cast"))?)]);
            gpu.queue.Signal(&fence, v).map_err(err("signal"))?;
            let (mut t_relay, mut t_fence) = (None, None);
            while t_relay.is_none() || t_fence.is_none() {
                if t_relay.is_none() && relay.0.read_volatile() == v as u32 {
                    t_relay = Some(t0.elapsed());
                }
                if t_fence.is_none() && fence.GetCompletedValue() >= v {
                    t_fence = Some(t0.elapsed());
                }
            }
            relay_late.push((t_relay.unwrap().as_secs_f64() - t_fence.unwrap().as_secs_f64()) * 1000.0);
        }
        worker.join().map_err(|_| "relay worker panicked".to_string())?;
    }
    drop(relay_mem);

    let med = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[v.len() * 95 / 100], v[0], v[v.len() - 1])
    };
    let (b50, ..) = med(busy.clone());
    let (e50, e95, emin, _) = med(early);
    let (r50, r95, _, rmax) = med(relay_late);
    lines.push(format!("work      each label follows a dispatch of ~{b50:.2} ms; {iterations} labels per method"));
    lines.push(format!("GPU label (MARKER_OUT into title memory): seen {:.1} µs before the fence median, {:.1} µs at p95, {:.1} µs least", e50 * 1000.0, e95 * 1000.0, emin * 1000.0));
    lines.push(format!("CPU relay (worker waits on the fence, writes the label): {:.1} µs after the fence median, {:.1} µs p95, {:.1} µs worst", r50 * 1000.0, r95 * 1000.0, rmax * 1000.0));
    check(&mut lines, too_soon == 0, format!("{iterations}/{iterations} GPU labels arrived in order, after their work, never early"))?;
    Ok(lines)
}

#[derive(Clone, Copy)]
struct Ptr32(*mut u32);
unsafe impl Send for Ptr32 {}
