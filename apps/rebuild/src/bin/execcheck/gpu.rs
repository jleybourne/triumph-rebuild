//! The card: one device, a direct queue, a shader-visible resource heap and sampler heap, and resources that
//! carry their starting contents (uploaded before each run) and, when written, a readback copy.

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Descriptors in the resource heap and the sampler heap.
pub const HEAP_SLOTS: u32 = 8192;
pub const SAMPLER_SLOTS: u32 = 2048;

pub struct Gpu {
    info: Option<ID3D12InfoQueue>,
    pub device: ID3D12Device,
    queue: ID3D12CommandQueue,
    alloc: ID3D12CommandAllocator,
    pub list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    pub heap: ID3D12DescriptorHeap,
    pub samplers: ID3D12DescriptorHeap,
    rtvs: ID3D12DescriptorHeap,
    dsvs: ID3D12DescriptorHeap,
    rtv_inc: usize,
    inc: usize,
    sinc: usize,
    lost: bool,
}

/// A borrowed resource pointer for descriptor structs that hold one without owning it.
pub fn weak(r: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    unsafe { std::mem::transmute_copy(r) }
}

pub fn transition(r: &ID3D12Resource, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: weak(r),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: from,
                StateAfter: to,
            }),
        },
    }
}

/// A resource with its starting contents.
pub struct Res {
    pub r: ID3D12Resource,
    pub desc: D3D12_RESOURCE_DESC,
    /// the state the shader reads or writes it in
    pub state: D3D12_RESOURCE_STATES,
    /// copied back after a run
    pub written: bool,
    /// per subresource: footprint, rows (per slice), bytes per row
    layout: Vec<(D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32, u64)>,
    total: u64,
    up: ID3D12Resource,
    back: Option<ID3D12Resource>,
    /// the starting contents, tightly packed, subresource after subresource
    pub init: Vec<u8>,
}

impl Res {
    pub fn is_buffer(&self) -> bool {
        self.desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER
    }
}

impl Gpu {
    /// `card`: the real GPU (the smoke set only); otherwise WARP, where a program that loops cannot hang the
    /// display driver. `debug`: the D3D12 debug layer, whose messages name refusals.
    pub fn new(card: bool, debug: bool) -> Result<Gpu, String> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, IDXGIAdapter, IDXGIFactory4, DXGI_CREATE_FACTORY_FLAGS};
        unsafe {
            if debug {
                let mut d: Option<ID3D12Debug> = None;
                D3D12GetDebugInterface(&mut d).map_err(|e| format!("debug layer: {e}"))?;
                d.ok_or("debug layer")?.EnableDebugLayer();
            }
            let mut device: Option<ID3D12Device> = None;
            if card {
                D3D12CreateDevice(None::<&windows::core::IUnknown>, D3D_FEATURE_LEVEL_12_0, &mut device).map_err(|e| format!("device: {e}"))?;
            } else {
                let f: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(|e| format!("dxgi: {e}"))?;
                let warp: IDXGIAdapter = f.EnumWarpAdapter().map_err(|e| format!("warp: {e}"))?;
                D3D12CreateDevice(&warp, D3D_FEATURE_LEVEL_12_0, &mut device).map_err(|e| format!("warp device: {e}"))?;
            }
            let device = device.ok_or("no device")?;
            let info = if debug { device.cast::<ID3D12InfoQueue>().ok() } else { None };
            if let Some(i) = &info {
                let _ = i.SetMuteDebugOutput(true);
            }
            let ty = D3D12_COMMAND_LIST_TYPE_DIRECT;
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: ty, ..Default::default() }).map_err(|e| e.to_string())?;
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(ty).map_err(|e| e.to_string())?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, ty, &alloc, None).map_err(|e| e.to_string())?;
            list.Close().map_err(|e| e.to_string())?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| e.to_string())?;
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: HEAP_SLOTS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .map_err(|e| e.to_string())?;
            let samplers: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                    NumDescriptors: SAMPLER_SLOTS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .map_err(|e| e.to_string())?;
            let plain = |ty, n| device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC { Type: ty, NumDescriptors: n, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE, NodeMask: 0 });
            let rtvs = plain(D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 8).map_err(|e| e.to_string())?;
            let dsvs = plain(D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 1).map_err(|e| e.to_string())?;
            let rtv_inc = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
            let inc = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let sinc = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER) as usize;
            let g = Gpu { info, device, queue, alloc, list, fence, value: 0, heap, samplers, rtvs, dsvs, rtv_inc, inc, sinc, lost: false };
            g.clear_heaps();
            Ok(g)
        }
    }

    /// Every slot a null 2D texture view and every sampler a point sampler, so a stray index reads zeros.
    pub fn clear_heaps(&self) {
        let null = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() } },
        };
        for k in 0..HEAP_SLOTS {
            unsafe { self.device.CreateShaderResourceView(None, Some(&null), self.slot(k)) };
        }
        for k in 0..SAMPLER_SLOTS {
            self.sampler(k, false);
        }
    }

    /// The debug layer's errors since the last call, or the call's own error.
    fn reason(&self, e: windows::core::Error) -> String {
        let mut out = Vec::new();
        if let Some(q) = &self.info {
            unsafe {
                for i in 0..q.GetNumStoredMessages() {
                    let mut len = 0usize;
                    if q.GetMessage(i, None, &mut len).is_err() || len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u64; len.div_ceil(8)];
                    let m = buf.as_mut_ptr() as *mut D3D12_MESSAGE;
                    if q.GetMessage(i, Some(m), &mut len).is_ok() && (*m).Severity.0 <= D3D12_MESSAGE_SEVERITY_WARNING.0 {
                        out.push(std::ffi::CStr::from_ptr((*m).pDescription as *const std::ffi::c_char).to_string_lossy().into_owned());
                    }
                }
                q.ClearStoredMessages();
            }
        }
        if out.is_empty() {
            format!("{}", e.code())
        } else {
            out.join(" | ")
        }
    }

    pub fn root_signature(&self, blob: &[u8]) -> Result<ID3D12RootSignature, String> {
        unsafe { self.device.CreateRootSignature(0, blob).map_err(|e| self.reason(e)) }
    }

    pub fn compute(&self, root: &ID3D12RootSignature, cs: &[u8]) -> Result<ID3D12PipelineState, String> {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: unsafe { std::mem::transmute_copy(root) },
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: cs.as_ptr() as *const _, BytecodeLength: cs.len() },
            ..Default::default()
        };
        unsafe { self.device.CreateComputePipelineState(&desc).map_err(|e| self.reason(e)) }
    }

    pub fn graphics(&self, desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Result<ID3D12PipelineState, String> {
        unsafe { self.device.CreateGraphicsPipelineState(desc).map_err(|e| self.reason(e)) }
    }

    pub fn rtv(&self, k: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: unsafe { self.rtvs.GetCPUDescriptorHandleForHeapStart() }.ptr + k as usize * self.rtv_inc }
    }

    pub fn dsv(&self) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        unsafe { self.dsvs.GetCPUDescriptorHandleForHeapStart() }
    }

    pub fn slot(&self, k: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: unsafe { self.heap.GetCPUDescriptorHandleForHeapStart() }.ptr + k as usize * self.inc }
    }

    pub fn sampler_slot(&self, k: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: unsafe { self.samplers.GetCPUDescriptorHandleForHeapStart() }.ptr + k as usize * self.sinc }
    }

    /// A linear, wrapping sampler (a comparison one: less-or-equal) at sampler slot `k`.
    pub fn sampler(&self, k: u32, cmp: bool) {
        let d = D3D12_SAMPLER_DESC {
            Filter: if cmp { D3D12_FILTER_COMPARISON_MIN_MAG_MIP_LINEAR } else { D3D12_FILTER_MIN_MAG_MIP_LINEAR },
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
        };
        unsafe { self.device.CreateSampler(&d, self.sampler_slot(k)) };
    }

    pub fn lost(&mut self) -> bool {
        if !self.lost {
            self.lost = unsafe { self.device.GetDeviceRemovedReason() }.is_err();
        }
        self.lost
    }

    fn committed(&self, kind: D3D12_HEAP_TYPE, desc: &D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES, clear: Option<&D3D12_CLEAR_VALUE>) -> Result<ID3D12Resource, String> {
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            self.device
                .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, desc, state, clear.map(|c| c as *const _), &mut r)
                .map_err(|e| format!("resource: {e}"))?;
        }
        r.ok_or_else(|| "no resource".into())
    }

    fn plain_buffer(&self, kind: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES) -> Result<ID3D12Resource, String> {
        self.committed(kind, &buffer_desc(bytes, false), state, None)
    }

    /// A resource with `init` as its contents (tightly packed subresources; for a texture, `init` may be empty to
    /// mean zeros). `written`: read back after each run.
    pub fn resource(&self, desc: D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES, written: bool, init: Vec<u8>) -> Result<Res, String> {
        let r = self.committed(D3D12_HEAP_TYPE_DEFAULT, &desc, D3D12_RESOURCE_STATE_COMMON, None)?;
        self.finish(r, desc, state, written, init)
    }

    fn finish(&self, r: ID3D12Resource, desc: D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES, written: bool, init: Vec<u8>) -> Result<Res, String> {
        let subs = if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            1
        } else {
            let depth_slices = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { desc.DepthOrArraySize as u32 };
            desc.MipLevels as u32 * depth_slices
        };
        let mut fp = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); subs as usize];
        let mut rows = vec![0u32; subs as usize];
        let mut rowbytes = vec![0u64; subs as usize];
        let mut total = 0u64;
        unsafe {
            self.device.GetCopyableFootprints(&desc, 0, subs, 0, Some(fp.as_mut_ptr()), Some(rows.as_mut_ptr()), Some(rowbytes.as_mut_ptr()), Some(&mut total));
        }
        let layout: Vec<_> = (0..subs as usize).map(|i| (fp[i], rows[i], rowbytes[i])).collect();
        let up = self.plain_buffer(D3D12_HEAP_TYPE_UPLOAD, total.max(256), D3D12_RESOURCE_STATE_GENERIC_READ)?;
        // lay the tight contents out at the footprints
        unsafe {
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            up.Map(0, None, Some(&mut p)).map_err(|e| e.to_string())?;
            let dst = std::slice::from_raw_parts_mut(p as *mut u8, total as usize);
            dst.fill(0);
            let mut at = 0usize;
            for (f, n, rb) in &layout {
                for z in 0..f.Footprint.Depth as usize {
                    for y in 0..*n as usize {
                        let o = f.Offset as usize + (z * *n as usize + y) * f.Footprint.RowPitch as usize;
                        let rb = *rb as usize;
                        if at + rb <= init.len() {
                            dst[o..o + rb].copy_from_slice(&init[at..at + rb]);
                        }
                        at += rb;
                    }
                }
            }
            up.Unmap(0, None);
        }
        let back = if written { Some(self.plain_buffer(D3D12_HEAP_TYPE_READBACK, total.max(256), D3D12_RESOURCE_STATE_COPY_DEST)?) } else { None };
        Ok(Res { r, desc, state, written, layout, total, up, back, init })
    }

    fn copy_location_sub(r: &ID3D12Resource, sub: u32) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION { pResource: weak(r), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub } }
    }

    fn copy_location_fp(r: &ID3D12Resource, fp: D3D12_PLACED_SUBRESOURCE_FOOTPRINT) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION { pResource: weak(r), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } }
    }

    /// Records: the starting contents into the resource, then the resource into its shader state.
    pub fn upload(&self, res: &Res) {
        let l = &self.list;
        unsafe {
            l.ResourceBarrier(&[transition(&res.r, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
            if res.is_buffer() {
                l.CopyBufferRegion(&res.r, 0, &res.up, 0, res.desc.Width);
            } else {
                for (i, (f, _, _)) in res.layout.iter().enumerate() {
                    l.CopyTextureRegion(&Self::copy_location_sub(&res.r, i as u32), 0, 0, 0, &Self::copy_location_fp(&res.up, *f), None);
                }
            }
            l.ResourceBarrier(&[transition(&res.r, D3D12_RESOURCE_STATE_COPY_DEST, res.state)]);
        }
    }

    /// Records: written resources copied to their readback buffers; every resource back to COMMON.
    pub fn download(&self, res: &Res) {
        let l = &self.list;
        unsafe {
            if let Some(back) = &res.back {
                l.ResourceBarrier(&[transition(&res.r, res.state, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                if res.is_buffer() {
                    l.CopyBufferRegion(back, 0, &res.r, 0, res.desc.Width);
                } else {
                    for (i, (f, _, _)) in res.layout.iter().enumerate() {
                        l.CopyTextureRegion(&Self::copy_location_fp(back, *f), 0, 0, 0, &Self::copy_location_sub(&res.r, i as u32), None);
                    }
                }
                l.ResourceBarrier(&[transition(&res.r, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
            } else {
                l.ResourceBarrier(&[transition(&res.r, res.state, D3D12_RESOURCE_STATE_COMMON)]);
            }
        }
    }

    /// The contents read back, tightly packed like `init`.
    pub fn read(&self, res: &Res) -> Vec<u8> {
        let Some(back) = &res.back else { return Vec::new() };
        let mut out = Vec::with_capacity(res.init.len());
        unsafe {
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            if back.Map(0, None, Some(&mut p)).is_err() {
                return out;
            }
            let src = std::slice::from_raw_parts(p as *const u8, res.total as usize);
            for (f, n, rb) in &res.layout {
                for z in 0..f.Footprint.Depth as usize {
                    for y in 0..*n as usize {
                        let o = f.Offset as usize + (z * *n as usize + y) * f.Footprint.RowPitch as usize;
                        out.extend_from_slice(&src[o..o + *rb as usize]);
                    }
                }
            }
            back.Unmap(0, None);
        }
        out
    }

    /// Begins recording.
    pub fn begin(&self) {
        unsafe {
            let _ = self.alloc.Reset();
            let _ = self.list.Reset(&self.alloc, None);
        }
    }

    /// Ends recording, runs, waits.
    pub fn end(&mut self) -> Result<(), String> {
        unsafe {
            self.list.Close().map_err(|e| format!("close: {e}"))?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).map_err(|e| e.to_string())?;
            // a program that never finishes: the worker ends, and the supervisor names it
            let ev = windows::Win32::System::Threading::CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
            self.fence.SetEventOnCompletion(self.value, ev).map_err(|e| e.to_string())?;
            let r = windows::Win32::System::Threading::WaitForSingleObject(ev, 30_000);
            let _ = windows::Win32::Foundation::CloseHandle(ev);
            if r != windows::Win32::Foundation::WAIT_OBJECT_0 {
                eprintln!("execcheck: no completion in 30 s");
                std::process::exit(4);
            }
        }
        if self.lost() {
            let why = unsafe { self.device.GetDeviceRemovedReason() };
            return Err(format!("device removed: {why:?}"));
        }
        Ok(())
    }

    /// An upload buffer holding `bytes` (for root data such as the shadow table).
    pub fn upload_buffer(&self, bytes: &[u8]) -> Result<ID3D12Resource, String> {
        let b = self.plain_buffer(D3D12_HEAP_TYPE_UPLOAD, (bytes.len() as u64).max(256), D3D12_RESOURCE_STATE_GENERIC_READ)?;
        unsafe {
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            b.Map(0, None, Some(&mut p)).map_err(|e| e.to_string())?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
            b.Unmap(0, None);
        }
        Ok(b)
    }
}

pub fn buffer_desc(bytes: u64, uav: bool) -> D3D12_RESOURCE_DESC {
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

pub fn texture_desc(dim: D3D12_RESOURCE_DIMENSION, w: u64, h: u32, d: u16, mips: u16, format: DXGI_FORMAT, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: dim,
        Alignment: 0,
        Width: w,
        Height: h,
        DepthOrArraySize: d,
        MipLevels: mips,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: flags,
    }
}
