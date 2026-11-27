//! Build 0 (a): what it costs to send the CPU's changes to the card, at the old layer's volume.
//!
//! One frame, as decision 1 of `plan.md` has it:
//!
//! ```text
//! title    writes its CPU-side data (a fixed set of ranges, like the buffers it rewrites each frame)
//! watch    GetWriteWatch (with reset) names the 4 KiB pages it wrote
//! diff     each written page, 64 bytes at a time, against what was last sent (shadow or hash)
//! gather   changed chunks packed into an upload-heap staging buffer
//! record   copy path:    one CopyBufferRegion per run, on a copy queue
//!          scatter path: packed chunks + a chunk-index list, one compute dispatch puts them back
//! gpu      timestamps around the copy / dispatch
//! wall     watch start -> fence complete
//! ```
//!
//! The destination is a default-heap (video memory) buffer the size of the region, standing in
//! for the tiled windows of Build 1. After the timed frames the destination is read back whole
//! and compared with the region: a run is only reported as verified when every byte arrived.

#![cfg(windows)]

use std::time::Instant;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Memory::*;

use crate::chunks::{self, Run, CHUNK, CHUNKS_PER_PAGE, OS_PAGE};

/// winnt.h: MEM_WRITE_WATCH, and GetWriteWatch's WRITE_WATCH_FLAG_RESET (not exported by the crate)
const MEM_WRITE_WATCH_FLAG: VIRTUAL_ALLOCATION_TYPE = VIRTUAL_ALLOCATION_TYPE(0x0020_0000);
const WRITE_WATCH_FLAG_RESET: u32 = 1;

/// staging capacity: the most changed bytes one frame may send
pub const STAGING: usize = 192 << 20;
const GROUP: u32 = 256;
const GX: u32 = 32768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diff {
    Shadow,
    Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Copy,
    Scatter,
}

/// How the title's writes change the bytes they touch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    /// each 64-byte chunk the title writes changes with this probability, independently
    Chunks(f64),
    /// each range the title writes is rewritten whole with new values with this probability,
    /// otherwise rewritten with the values it already had
    Ranges(f64),
}

impl Change {
    pub fn label(&self) -> String {
        match self {
            Change::Chunks(p) => format!("{:>3.0}% of chunks", p * 100.0),
            Change::Ranges(p) => format!("{:>3.0}% of ranges", p * 100.0),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub region: usize,
    pub written: usize,
    pub change: Change,
    pub diff: Diff,
    pub path: Path,
    pub threads: usize,
    pub frames: usize,
    pub warmup: usize,
    pub seed: u64,
    /// destination is a tiled window (Build 1) instead of a committed default-heap buffer
    pub tiled: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub dirty_pages: usize,
    pub runs: usize,
    pub bytes: usize,
    pub title_ms: f64,
    pub watch_ms: f64,
    pub diff_ms: f64,
    pub gather_ms: f64,
    pub record_ms: f64,
    pub gpu_ms: f64,
    pub wall_ms: f64,
}

pub struct Summary {
    pub median: Sample,
    pub p95_wall_ms: f64,
    pub ranges: usize,
    pub verified: Result<(), String>,
}

// ---------------------------------------------------------------- the title's write pattern

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The ranges the title rewrites every frame: sizes log-uniform between 256 B and 64 KiB
/// (mean ~11.8 KiB; the old layer made ~11,500 copies for 83-115 MB, ~10 KB each), placed at
/// random 64-byte-aligned offsets, until `written` bytes are covered.
pub fn title_ranges(region: usize, written: usize, seed: u64) -> Vec<Run> {
    let mut rng = Rng(seed);
    let (lo, hi) = ((256f64).ln(), (65536f64).ln());
    let mut out = Vec::new();
    let mut total = 0;
    while total < written {
        let len = ((lo + (hi - lo) * rng.unit()).exp() as usize / CHUNK).max(1) * CHUNK;
        let offset = (rng.next() as usize % ((region - len) / CHUNK)) * CHUNK;
        out.push(Run { offset, len });
        total += len;
    }
    out.sort_by_key(|r| r.offset);
    out
}

/// One frame of title writes into `base`. Not part of our cost; timed only for context.
pub unsafe fn title_writes(base: *mut u8, ranges: &[Run], change: Change, frame: u64, rng: &mut Rng) {
    for r in ranges {
        let whole = match change {
            Change::Ranges(p) => Some(rng.unit() < p),
            Change::Chunks(_) => None,
        };
        for c in (r.offset..r.offset + r.len).step_by(CHUNK) {
            let fresh = match (whole, change) {
                (Some(w), _) => w,
                (None, Change::Chunks(p)) => rng.unit() < p,
                _ => unreachable!(),
            };
            let p = base.add(c) as *mut u64;
            if fresh {
                for i in 0..8u64 {
                    let v = (frame << 40) ^ ((c as u64) << 3) ^ i ^ 0x5A5A_0000_0000_0001;
                    p.add(i as usize).write_volatile(v);
                }
            } else {
                // touched but not changed: the page is written, the bytes stay as they were
                p.write_volatile(p.read_volatile());
            }
        }
    }
}

// ---------------------------------------------------------------- the GPU side

fn buffer(bytes: u64, uav: bool) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        },
    }
}

fn committed(
    device: &ID3D12Device,
    kind: D3D12_HEAP_TYPE,
    bytes: usize,
    uav: bool,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource, String> {
    let mut r: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &buffer(bytes as u64, uav),
            state,
            None,
            &mut r,
        )
    }
    .map_err(|e| format!("buffer ({} MiB): {e}", bytes >> 20))?;
    r.ok_or_else(|| "buffer: none".into())
}

fn map(r: &ID3D12Resource) -> Result<*mut u8, String> {
    let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe { r.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p)) }
        .map_err(|e| format!("map: {e}"))?;
    Ok(p as *mut u8)
}

fn blob(b: &ID3DBlob) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() }
}

const SCATTER: &[u8] = br#"
ByteAddressBuffer   packed : register(t0);
ByteAddressBuffer   index  : register(t1);
RWByteAddressBuffer dst    : register(u0);
cbuffer C : register(b0) { uint count; uint gx; };

// one thread per 16 bytes: four threads carry one 64-byte chunk
[numthreads(256, 1, 1)]
void main(uint3 g : SV_GroupID, uint t : SV_GroupIndex) {
    uint lane = (g.y * gx + g.x) * 256 + t;
    uint chunk = lane >> 2;
    if (chunk >= count) return;
    uint part = (lane & 3) * 16;
    dst.Store4(index.Load(chunk * 4) * 64 + part, packed.Load4(chunk * 64 + part));
}
"#;

pub struct Gpu {
    device: ID3D12Device,
    copy_queue: ID3D12CommandQueue,
    compute_queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    fence_value: u64,
    copy_alloc: ID3D12CommandAllocator,
    copy_list: ID3D12GraphicsCommandList,
    compute_alloc: ID3D12CommandAllocator,
    compute_list: ID3D12GraphicsCommandList,
    signature: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    copy_stamps: Option<ID3D12QueryHeap>,
    compute_stamps: ID3D12QueryHeap,
    copy_freq: f64,
    compute_freq: f64,
    stamps: ID3D12Resource,
    stamps_ptr: *const u64,
    staging: ID3D12Resource,
    staging_ptr: *mut u8,
    index: ID3D12Resource,
    index_ptr: *mut u32,
    pub adapter: String,
}

impl Gpu {
    pub fn new() -> Result<Gpu, String> {
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None::<&windows::core::IUnknown>, D3D_FEATURE_LEVEL_11_0, &mut device)
                .map_err(|e| format!("device: {e}"))?;
            let device = device.ok_or("device: none")?;

            let queue = |kind| {
                device
                    .CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                        Type: kind,
                        ..Default::default()
                    })
                    .map_err(|e| format!("queue: {e}"))
            };
            let copy_queue = queue(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let compute_queue = queue(D3D12_COMMAND_LIST_TYPE_COMPUTE)?;
            let list = |kind| -> Result<(ID3D12CommandAllocator, ID3D12GraphicsCommandList), String> {
                let a = device
                    .CreateCommandAllocator::<ID3D12CommandAllocator>(kind)
                    .map_err(|e| format!("allocator: {e}"))?;
                let l = device
                    .CreateCommandList::<_, _, ID3D12GraphicsCommandList>(0, kind, &a, None)
                    .map_err(|e| format!("list: {e}"))?;
                l.Close().map_err(|e| format!("close: {e}"))?;
                Ok((a, l))
            };
            let (copy_alloc, copy_list) = list(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let (compute_alloc, compute_list) = list(D3D12_COMMAND_LIST_TYPE_COMPUTE)?;
            let fence = device
                .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
                .map_err(|e| format!("fence: {e}"))?;

            // the scatter shader and its signature: two root constants, two root SRVs, one root UAV
            let mut code: Option<ID3DBlob> = None;
            let mut errors: Option<ID3DBlob> = None;
            D3DCompile(
                SCATTER.as_ptr().cast(),
                SCATTER.len(),
                PCSTR::null(),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_0"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
            .map_err(|e| match &errors {
                Some(b) => format!("compile: {e}: {}", String::from_utf8_lossy(&blob(b))),
                None => format!("compile: {e}"),
            })?;
            let code = blob(&code.ok_or("compile: no code")?);
            let root = |kind, reg| D3D12_ROOT_PARAMETER {
                ParameterType: kind,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: reg, RegisterSpace: 0 },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            };
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 2 },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                root(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                root(D3D12_ROOT_PARAMETER_TYPE_SRV, 1),
                root(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
            ];
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: 0,
                pStaticSamplers: std::ptr::null(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
            };
            let mut sig_blob: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut sig_blob, Some(&mut errors))
                .map_err(|e| format!("signature: {e}"))?;
            let signature = device
                .CreateRootSignature::<ID3D12RootSignature>(0, &blob(&sig_blob.ok_or("signature: none")?))
                .map_err(|e| format!("signature: {e}"))?;
            let pso = device
                .CreateComputePipelineState::<ID3D12PipelineState>(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&signature),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .map_err(|e| format!("pipeline: {e}"))?;

            // timestamps: the copy queue needs its own heap type, and not every driver has it
            let mut o3 = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
            let copy_ts = device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS3,
                    &mut o3 as *mut _ as *mut _,
                    std::mem::size_of_val(&o3) as u32,
                )
                .is_ok()
                && o3.CopyQueueTimestampQueriesSupported.as_bool();
            let heap = |kind| {
                let mut h: Option<ID3D12QueryHeap> = None;
                device
                    .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: kind, Count: 2, NodeMask: 0 }, &mut h)
                    .map_err(|e| format!("query heap: {e}"))?;
                h.ok_or_else(|| "query heap: none".to_string())
            };
            let copy_stamps = if copy_ts { Some(heap(D3D12_QUERY_HEAP_TYPE_COPY_QUEUE_TIMESTAMP)?) } else { None };
            let compute_stamps = heap(D3D12_QUERY_HEAP_TYPE_TIMESTAMP)?;
            let copy_freq = copy_queue.GetTimestampFrequency().map_err(|e| format!("freq: {e}"))? as f64;
            let compute_freq = compute_queue.GetTimestampFrequency().map_err(|e| format!("freq: {e}"))? as f64;

            let stamps = committed(&device, D3D12_HEAP_TYPE_READBACK, 256, false, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let stamps_ptr = map(&stamps)? as *const u64;
            let staging = committed(&device, D3D12_HEAP_TYPE_UPLOAD, STAGING, false, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let staging_ptr = map(&staging)?;
            let index = committed(&device, D3D12_HEAP_TYPE_UPLOAD, STAGING / CHUNK * 4, false, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let index_ptr = map(&index)? as *mut u32;

            let adapter = {
                let luid = device.GetAdapterLuid();
                let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory4 =
                    windows::Win32::Graphics::Dxgi::CreateDXGIFactory1().map_err(|e| format!("dxgi: {e}"))?;
                match factory.EnumAdapterByLuid::<windows::Win32::Graphics::Dxgi::IDXGIAdapter1>(luid) {
                    Ok(a) => {
                        let d = a.GetDesc1().map_err(|e| format!("adapter: {e}"))?;
                        let n = d.Description.iter().position(|c| *c == 0).unwrap_or(d.Description.len());
                        format!(
                            "{} ({} MiB VRAM)",
                            String::from_utf16_lossy(&d.Description[..n]),
                            d.DedicatedVideoMemory >> 20
                        )
                    }
                    Err(_) => "unknown adapter".into(),
                }
            };

            Ok(Gpu {
                device,
                copy_queue,
                compute_queue,
                fence,
                fence_value: 0,
                copy_alloc,
                copy_list,
                compute_alloc,
                compute_list,
                signature,
                pso,
                copy_stamps,
                compute_stamps,
                copy_freq,
                compute_freq,
                stamps,
                stamps_ptr,
                staging,
                staging_ptr,
                index,
                index_ptr,
                adapter,
            })
        }
    }

    pub fn copy_timestamps(&self) -> bool {
        self.copy_stamps.is_some()
    }

    /// Submit a closed list and block until the GPU has finished it.
    unsafe fn run(&mut self, queue: &ID3D12CommandQueue, list: &ID3D12GraphicsCommandList) -> Result<(), String> {
        let l: ID3D12CommandList = list.cast().map_err(|e| format!("cast: {e}"))?;
        queue.ExecuteCommandLists(&[Some(l)]);
        self.fence_value += 1;
        queue.Signal(&self.fence, self.fence_value).map_err(|e| format!("signal: {e}"))?;
        // a null event makes this call block until the fence reaches the value
        self.fence
            .SetEventOnCompletion(self.fence_value, HANDLE::default())
            .map_err(|e| format!("wait: {e}"))?;
        if let Err(e) = self.device.GetDeviceRemovedReason() {
            return Err(format!("device removed: {e}"));
        }
        Ok(())
    }

    unsafe fn stamp_ms(&self, freq: f64) -> f64 {
        let t0 = self.stamps_ptr.read_volatile();
        let t1 = self.stamps_ptr.add(1).read_volatile();
        t1.saturating_sub(t0) as f64 * 1000.0 / freq
    }

    /// Record the copy path: one CopyBufferRegion per run. Returns GPU ms after it ran.
    unsafe fn copy_runs(&mut self, dst: &ID3D12Resource, slices: &[(Vec<Run>, usize)]) -> Result<(f64, f64), String> {
        let t = Instant::now();
        let list = self.copy_list.clone();
        self.copy_alloc.Reset().map_err(|e| format!("reset: {e}"))?;
        list.Reset(&self.copy_alloc, None).map_err(|e| format!("reset: {e}"))?;
        if let Some(h) = &self.copy_stamps {
            list.EndQuery(h, D3D12_QUERY_TYPE_TIMESTAMP, 0);
        }
        for (runs, base) in slices {
            let mut at = *base as u64;
            for r in runs {
                list.CopyBufferRegion(dst, r.offset as u64, &self.staging, at, r.len as u64);
                at += r.len as u64;
            }
        }
        if let Some(h) = &self.copy_stamps {
            list.EndQuery(h, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list.ResolveQueryData(h, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.stamps, 0);
        }
        list.Close().map_err(|e| format!("close: {e}"))?;
        let record = t.elapsed().as_secs_f64() * 1000.0;
        let q = self.copy_queue.clone();
        self.run(&q, &list)?;
        let gpu = if self.copy_stamps.is_some() { self.stamp_ms(self.copy_freq) } else { f64::NAN };
        Ok((record, gpu))
    }

    /// Record the scatter path: one dispatch over the packed chunks.
    unsafe fn scatter(&mut self, dst: &ID3D12Resource, chunks: usize) -> Result<(f64, f64), String> {
        let t = Instant::now();
        let list = self.compute_list.clone();
        self.compute_alloc.Reset().map_err(|e| format!("reset: {e}"))?;
        list.Reset(&self.compute_alloc, &self.pso).map_err(|e| format!("reset: {e}"))?;
        list.SetComputeRootSignature(&self.signature);
        let groups = ((chunks * 4) as u32).div_ceil(GROUP);
        let (gx, gy) = (groups.min(GX), groups.div_ceil(GX));
        list.SetComputeRoot32BitConstant(0, chunks as u32, 0);
        list.SetComputeRoot32BitConstant(0, gx, 1);
        list.SetComputeRootShaderResourceView(1, self.staging.GetGPUVirtualAddress());
        list.SetComputeRootShaderResourceView(2, self.index.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(3, dst.GetGPUVirtualAddress());
        list.EndQuery(&self.compute_stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
        list.Dispatch(gx, gy, 1);
        list.EndQuery(&self.compute_stamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
        list.ResolveQueryData(&self.compute_stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.stamps, 0);
        list.Close().map_err(|e| format!("close: {e}"))?;
        let record = t.elapsed().as_secs_f64() * 1000.0;
        let q = self.compute_queue.clone();
        self.run(&q, &list)?;
        Ok((record, self.stamp_ms(self.compute_freq)))
    }

    /// The link's ceiling: `bytes` of staging into video memory in one copy, `reps` times.
    /// Returns (median GPU ms, GB/s).
    pub fn ceiling(&mut self, bytes: usize, reps: usize) -> Result<(f64, f64), String> {
        let dst = committed(&self.device, D3D12_HEAP_TYPE_DEFAULT, bytes, true, D3D12_RESOURCE_STATE_COMMON)?;
        let mut ms = Vec::new();
        for _ in 0..reps {
            let (_, gpu) = unsafe { self.copy_runs(&dst, &[(vec![Run { offset: 0, len: bytes }], 0)])? };
            ms.push(gpu);
        }
        let m = median(&mut ms);
        Ok((m, bytes as f64 / (m / 1000.0) / 1e9))
    }

    /// Read the whole destination back and compare it with the region.
    unsafe fn verify(&mut self, dst: &ID3D12Resource, region: &[u8]) -> Result<(), String> {
        let back = committed(&self.device, D3D12_HEAP_TYPE_READBACK, region.len(), false, D3D12_RESOURCE_STATE_COPY_DEST)?;
        let list = self.copy_list.clone();
        self.copy_alloc.Reset().map_err(|e| format!("reset: {e}"))?;
        list.Reset(&self.copy_alloc, None).map_err(|e| format!("reset: {e}"))?;
        list.CopyBufferRegion(&back, 0, dst, 0, region.len() as u64);
        list.Close().map_err(|e| format!("close: {e}"))?;
        let q = self.copy_queue.clone();
        self.run(&q, &list)?;
        let p = map(&back)?;
        let got = std::slice::from_raw_parts(p as *const u8, region.len());
        let bad = region
            .chunks(CHUNK)
            .zip(got.chunks(CHUNK))
            .position(|(a, b)| a != b);
        back.Unmap(0, None);
        match bad {
            None => Ok(()),
            Some(i) => Err(format!("first wrong chunk at offset 0x{:X}", i * CHUNK)),
        }
    }
}

// ---------------------------------------------------------------- one configuration

struct Region {
    base: *mut u8,
}

impl Region {
    fn new(len: usize) -> Result<Region, String> {
        let base = unsafe { VirtualAlloc(None, len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH_FLAG, PAGE_READWRITE) };
        if base.is_null() {
            return Err(format!("VirtualAlloc with write-watch ({} MiB) failed", len >> 20));
        }
        Ok(Region { base: base as *mut u8 })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFree(self.base as *mut _, 0, MEM_RELEASE);
        }
    }
}

/// A raw pointer handed to worker threads that touch disjoint pages of it.
#[derive(Clone, Copy)]
struct Shared<T>(*mut T);
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn p95(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * 0.95).ceil() as usize).min(v.len()) - 1]
}

pub fn run(gpu: &mut Gpu, cfg: &Config) -> Result<Summary, String> {
    let region = Region::new(cfg.region)?;
    let pages = cfg.region / OS_PAGE;
    let mut shadow: Vec<u8> = if cfg.diff == Diff::Shadow { vec![0u8; cfg.region] } else { Vec::new() };
    let mut sent: Vec<u64> = if cfg.diff == Diff::Hash { vec![chunks::zero_hash(); cfg.region / CHUNK] } else { Vec::new() };
    // keep the tiles alive as long as the destination
    let (dst, _tiles) = if cfg.tiled {
        let mut uma = crate::Uma::new(1, cfg.region);
        let (_, mapped) = uma.place(0, cfg.region).map_err(|e| format!("place: {e:?}"))?;
        let q = gpu.copy_queue.clone();
        let mut tiles = crate::tiles::Tiles::new(&gpu.device, &q, 1)?;
        tiles.map(&mapped)?;
        unsafe {
            gpu.fence_value += 1;
            q.Signal(&gpu.fence, gpu.fence_value).map_err(|e| format!("signal: {e}"))?;
            gpu.fence.SetEventOnCompletion(gpu.fence_value, HANDLE::default()).map_err(|e| format!("wait: {e}"))?;
        }
        (tiles.windows[0].clone(), Some(tiles))
    } else {
        (committed(&gpu.device, D3D12_HEAP_TYPE_DEFAULT, cfg.region, true, D3D12_RESOURCE_STATE_COMMON)?, None)
    };
    let ranges = title_ranges(cfg.region, cfg.written, cfg.seed);
    let mut rng = Rng(cfg.seed ^ 0xA5A5);
    let mut addresses: Vec<*mut std::ffi::c_void> = vec![std::ptr::null_mut(); pages];
    let mut dirty: Vec<usize> = Vec::with_capacity(pages);
    let mut slices: Vec<(Vec<Run>, usize)> = (0..cfg.threads).map(|_| (Vec::new(), 0)).collect();
    unsafe { ResetWriteWatch(region.base as *const _, cfg.region) };

    let mut samples = Vec::new();
    for frame in 0..(cfg.warmup + cfg.frames) as u64 {
        let mut s = Sample::default();

        let t = Instant::now();
        unsafe { title_writes(region.base, &ranges, cfg.change, frame + 1, &mut rng) };
        s.title_ms = ms(t);

        // watch
        let t0 = Instant::now();
        let mut count = pages;
        let mut gran = 0u32;
        let rc = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                region.base as *const _,
                cfg.region,
                Some(addresses.as_mut_ptr()),
                Some(&mut count),
                Some(&mut gran),
            )
        };
        if rc != 0 {
            return Err("GetWriteWatch failed".into());
        }
        dirty.clear();
        dirty.extend(addresses[..count].iter().map(|a| (*a as usize - region.base as usize) / OS_PAGE));
        s.dirty_pages = count;
        s.watch_ms = ms(t0);

        // diff, on `threads` workers over contiguous slices of the dirty pages
        let t = Instant::now();
        let per = dirty.len().div_ceil(cfg.threads).max(1);
        let base = Shared(region.base);
        let shadow_p = Shared(shadow.as_mut_ptr());
        let sent_p = Shared(sent.as_mut_ptr());
        let diff = cfg.diff;
        std::thread::scope(|scope| {
            for (i, (runs, _)) in slices.iter_mut().enumerate() {
                let mine = dirty.get(i * per..((i + 1) * per).min(dirty.len())).unwrap_or(&[]);
                let mut work = move || {
                    let (base, shadow_p, sent_p) = (base, shadow_p, sent_p);
                    runs.clear();
                    for &pg in mine {
                        let off = pg * OS_PAGE;
                        // SAFETY: each page belongs to exactly one worker
                        unsafe {
                            let page = std::slice::from_raw_parts(base.0.add(off), OS_PAGE);
                            match diff {
                                Diff::Shadow => {
                                    let sh = std::slice::from_raw_parts_mut(shadow_p.0.add(off), OS_PAGE);
                                    chunks::diff_shadow(page, sh, off, runs);
                                }
                                Diff::Hash => {
                                    let h = std::slice::from_raw_parts_mut(sent_p.0.add(off / CHUNK), CHUNKS_PER_PAGE);
                                    chunks::diff_hash(page, h, off, runs);
                                }
                            }
                        }
                    }
                };
                if cfg.threads == 1 {
                    work();
                } else {
                    scope.spawn(work);
                }
            }
        });
        s.diff_ms = ms(t);

        // gather: each worker packs its runs at its own offset in staging
        let t = Instant::now();
        let mut at = 0;
        for (runs, base) in slices.iter_mut() {
            *base = at;
            at += chunks::bytes(runs);
        }
        s.bytes = at;
        s.runs = slices.iter().map(|(r, _)| r.len()).sum();
        if at > STAGING {
            return Err(format!("{} MiB changed, more than the {} MiB staging", at >> 20, STAGING >> 20));
        }
        let staging = Shared(gpu.staging_ptr);
        let index = Shared(gpu.index_ptr);
        let scatter = cfg.path == Path::Scatter;
        let region_bytes = unsafe { std::slice::from_raw_parts(region.base as *const u8, cfg.region) };
        std::thread::scope(|scope| {
            for (runs, base) in slices.iter() {
                let n = chunks::bytes(runs);
                let work = move || {
                    let (staging, index) = (staging, index);
                    // SAFETY: each worker writes its own [base, base + n) of staging and index
                    unsafe {
                        let dst = std::slice::from_raw_parts_mut(staging.0.add(*base), n);
                        chunks::gather(region_bytes, runs, dst);
                        if scatter {
                            let idx = std::slice::from_raw_parts_mut(index.0.add(*base / CHUNK), n / CHUNK);
                            chunks::chunk_indices(runs, idx);
                        }
                    }
                };
                if cfg.threads == 1 {
                    work();
                } else {
                    scope.spawn(work);
                }
            }
        });
        s.gather_ms = ms(t);

        // record, submit, wait
        if s.bytes > 0 {
            let (record, gpu_ms) = unsafe {
                match cfg.path {
                    Path::Copy => gpu.copy_runs(&dst, &slices)?,
                    Path::Scatter => gpu.scatter(&dst, s.bytes / CHUNK)?,
                }
            };
            s.record_ms = record;
            s.gpu_ms = gpu_ms;
        }
        s.wall_ms = ms(t0);

        if frame as usize >= cfg.warmup {
            samples.push(s);
        }
    }

    let verified = unsafe {
        let r = std::slice::from_raw_parts(region.base as *const u8, cfg.region);
        gpu.verify(&dst, r)
    };

    let col = |f: fn(&Sample) -> f64| {
        let mut v: Vec<f64> = samples.iter().map(f).collect();
        median(&mut v)
    };
    let median_sample = Sample {
        dirty_pages: col(|s| s.dirty_pages as f64) as usize,
        runs: col(|s| s.runs as f64) as usize,
        bytes: col(|s| s.bytes as f64) as usize,
        title_ms: col(|s| s.title_ms),
        watch_ms: col(|s| s.watch_ms),
        diff_ms: col(|s| s.diff_ms),
        gather_ms: col(|s| s.gather_ms),
        record_ms: col(|s| s.record_ms),
        gpu_ms: col(|s| s.gpu_ms),
        wall_ms: col(|s| s.wall_ms),
    };
    let mut walls: Vec<f64> = samples.iter().map(|s| s.wall_ms).collect();
    Ok(Summary { median: median_sample, p95_wall_ms: p95(&mut walls), ranges: ranges.len(), verified })
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}
