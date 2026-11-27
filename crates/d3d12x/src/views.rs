//! The host side of descriptors: what a host view is, and the device that writes them.
//!
//! [`HostDevice`] is the one place host descriptors, heaps, the shadow table and host textures
//! are made. [`DeviceHost`] is the real one (a host `ID3D12Device`); the unit tests use a
//! recording stand-in, so every translation is checked without a GPU.

use std::ffi::c_void;

use windows::core::{Interface, HRESULT};
use windows::Win32::Foundation::E_OUTOFMEMORY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::Raw;

/// `D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING`
pub const MAPPING: u32 = 0x1688;

/// A host descriptor to write. Resources are borrowed (the console objects keep them alive).
#[derive(Clone, Copy)]
pub enum HostView {
    /// A null raw buffer SRV: reads 0 (every view slot starts as this).
    NullBuffer,
    /// A null CBV.
    NullCbv,
    /// A null buffer UAV: writes dropped, reads 0.
    NullUav,
    /// A null texture SRV of a dimension (`D3D12_SRV_DIMENSION`).
    NullTexture(i32),
    Cbv { va: u64, size: u32 },
    Srv { resource: Raw, desc: D3D12_SHADER_RESOURCE_VIEW_DESC },
    Uav { resource: Raw, desc: D3D12_UNORDERED_ACCESS_VIEW_DESC },
    Rtv { resource: Raw, desc: D3D12_RENDER_TARGET_VIEW_DESC },
    Dsv { resource: Raw, desc: D3D12_DEPTH_STENCIL_VIEW_DESC },
    Sampler(D3D12_SAMPLER_DESC),
}
// SAFETY: borrowed COM pointers of free-threaded D3D12 objects.
unsafe impl Send for HostView {}
unsafe impl Sync for HostView {}

impl HostView {
    /// A raw (`ByteAddressBuffer`) SRV of `elements` dwords from dword `first` of `resource`.
    pub fn raw_srv(resource: Raw, first: u64, elements: u32) -> HostView {
        HostView::Srv {
            resource,
            desc: D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV { FirstElement: first, NumElements: elements, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_RAW },
                },
            },
        }
    }

    pub fn raw_uav(resource: Raw, first: u64, elements: u32) -> HostView {
        HostView::Uav {
            resource,
            desc: D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV { FirstElement: first, NumElements: elements, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_RAW },
                },
            },
        }
    }

    pub fn typed_srv(resource: Raw, format: u32, first: u64, elements: u32) -> HostView {
        HostView::Srv {
            resource,
            desc: D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(format as i32),
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV { FirstElement: first, NumElements: elements, StructureByteStride: 0, Flags: D3D12_BUFFER_SRV_FLAG_NONE },
                },
            },
        }
    }

    pub fn typed_uav(resource: Raw, format: u32, first: u64, elements: u32) -> HostView {
        HostView::Uav {
            resource,
            desc: D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT(format as i32),
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV { FirstElement: first, NumElements: elements, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_NONE },
                },
            },
        }
    }

    /// For tests and logs: what the view is, in a few words.
    pub fn describe(&self) -> String {
        // SAFETY: union members read by the dimension that selects them.
        unsafe {
            match self {
                HostView::NullBuffer => "null buffer".into(),
                HostView::NullCbv => "null cbv".into(),
                HostView::NullUav => "null uav".into(),
                HostView::NullTexture(d) => format!("null texture dim {d}"),
                HostView::Cbv { va, size } => format!("cbv {va:#x} {size}"),
                HostView::Srv { resource, desc } if desc.ViewDimension == D3D12_SRV_DIMENSION_BUFFER => {
                    let b = desc.Anonymous.Buffer;
                    let raw = b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
                    format!("srv {} {:?} fmt {} first {} n {}", if raw { "raw" } else { "typed" }, resource, desc.Format.0, b.FirstElement, b.NumElements)
                }
                HostView::Srv { resource, desc } => format!("srv texture {:?} fmt {} dim {}", resource, desc.Format.0, desc.ViewDimension.0),
                HostView::Uav { resource, desc } if desc.ViewDimension == D3D12_UAV_DIMENSION_BUFFER => {
                    let b = desc.Anonymous.Buffer;
                    let raw = b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
                    format!("uav {} {:?} fmt {} first {} n {}", if raw { "raw" } else { "typed" }, resource, desc.Format.0, b.FirstElement, b.NumElements)
                }
                HostView::Uav { resource, desc } => format!("uav texture {:?} fmt {} dim {}", resource, desc.Format.0, desc.ViewDimension.0),
                HostView::Rtv { resource, desc } => format!("rtv {:?} fmt {}", resource, desc.Format.0),
                HostView::Dsv { resource, desc } => format!("dsv {:?} fmt {}", resource, desc.Format.0),
                HostView::Sampler(s) => format!("sampler filter {} addr {}", s.Filter.0, s.AddressU.0),
            }
        }
    }

    pub fn is_raw_srv(&self) -> bool {
        // SAFETY: the buffer member, selected by the dimension.
        matches!(self, HostView::Srv { desc, .. } if desc.ViewDimension == D3D12_SRV_DIMENSION_BUFFER && unsafe { desc.Anonymous.Buffer.Flags.0 } & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0)
    }
}

/// Host descriptor heap kinds, as D3D12 numbers them.
pub const VIEWS: i32 = 0;
pub const SAMPLERS: i32 = 1;
pub const RTV: i32 = 2;
pub const DSV: i32 = 3;

/// A host descriptor heap.
pub struct HostHeap {
    pub heap: Option<ID3D12DescriptorHeap>,
    pub kind: i32,
    pub count: u32,
    pub cpu: usize,
    /// 0 unless shader-visible
    pub gpu: u64,
    pub step: u32,
}
// SAFETY: D3D12 heaps are free-threaded; descriptor writes to distinct slots need no lock.
unsafe impl Send for HostHeap {}
unsafe impl Sync for HostHeap {}

impl HostHeap {
    #[inline]
    pub fn cpu_at(&self, i: u32) -> usize {
        self.cpu + i as usize * self.step as usize
    }
    #[inline]
    pub fn gpu_at(&self, i: u32) -> u64 {
        self.gpu + u64::from(i) * u64::from(self.step)
    }
    pub fn raw(&self) -> Raw {
        self.heap.as_ref().map_or(std::ptr::null_mut(), |h| h.as_raw())
    }
}

/// The descriptor shadow table of one shader-visible view heap (plan decision 9): 16 bytes per
/// console index, `(stride, records, alternate index, raw-view lead bytes)`, read by translated
/// shaders at `register(t0, space101)` from GPU address `va`.
pub struct Shadow {
    pub ptr: *mut [u32; 4],
    pub entries: u32,
    pub va: u64,
    pub resource: Option<ID3D12Resource>,
    /// the stand-in's memory (tests): owned here
    _keep: Option<Box<[[u32; 4]]>>,
}
// SAFETY: written per index by whoever mirrors that index; read by the GPU.
unsafe impl Send for Shadow {}
unsafe impl Sync for Shadow {}

impl Shadow {
    /// A shadow table in host memory (no GPU): for the recording stand-in.
    pub fn in_memory(entries: u32) -> Shadow {
        let mut keep = vec![[0u32; 4]; entries as usize].into_boxed_slice();
        Shadow { ptr: keep.as_mut_ptr(), entries, va: 0, resource: None, _keep: Some(keep) }
    }

    #[inline]
    pub fn write(&self, i: u32, entry: [u32; 4]) {
        if i < self.entries {
            // SAFETY: inside the table; one writer per index.
            unsafe { self.ptr.add(i as usize).write_volatile(entry) };
        }
    }

    pub fn read(&self, i: u32) -> [u32; 4] {
        assert!(i < self.entries);
        // SAFETY: inside the table (a CPU-readable mapping).
        unsafe { self.ptr.add(i as usize).read_volatile() }
    }
}

/// The host device as this crate uses it.
pub trait HostDevice: Send + Sync {
    /// A host descriptor heap of `count` descriptors of `kind`, every slot made null (views:
    /// [`HostView::NullBuffer`]; samplers: point-clamp).
    fn heap(&self, kind: i32, count: u32, visible: bool) -> Result<HostHeap, HRESULT>;
    /// Write `view` at CPU handle `at`.
    fn write(&self, at: usize, view: &HostView);
    /// Copy one descriptor of `kind` from CPU handle `src` (a CPU-only heap) to `dst`.
    fn copy(&self, dst: usize, src: usize, kind: i32);
    /// A shadow table of `entries` entries, zeroed.
    fn shadow(&self, entries: u32) -> Result<Shadow, HRESULT>;
    /// A host resource for a console description already turned host (`desc`); `None` with no
    /// GPU (the stand-in).
    fn resource(&self, desc: &D3D12_RESOURCE_DESC, heap: D3D12_HEAP_TYPE, flags: D3D12_HEAP_FLAGS, clear: Option<&D3D12_CLEAR_VALUE>) -> Result<Option<ID3D12Resource>, HRESULT>;
    /// A host query heap; `None` with no GPU (the stand-in).
    fn query_heap(&self, _desc: &D3D12_QUERY_HEAP_DESC) -> Result<Option<ID3D12QueryHeap>, HRESULT> {
        Ok(None)
    }
    /// A host command signature (`root`: the host root signature it changes, if any); `None` with no GPU.
    fn command_signature(&self, _desc: &D3D12_COMMAND_SIGNATURE_DESC, _root: Option<&ID3D12RootSignature>) -> Result<Option<ID3D12CommandSignature>, HRESULT> {
        Ok(None)
    }
    /// A host root signature from a serialized blob; `None` with no GPU (the stand-in).
    fn root_signature(&self, _blob: &[u8]) -> Result<Option<ID3D12RootSignature>, HRESULT> {
        Ok(None)
    }
    /// `GetCopyableFootprints` on the host's rules.
    #[allow(clippy::too_many_arguments)]
    fn footprints(&self, desc: &D3D12_RESOURCE_DESC, first: u32, count: u32, base: u64, layouts: *mut D3D12_PLACED_SUBRESOURCE_FOOTPRINT, rows: *mut u32, row_bytes: *mut u64, total: *mut u64);
}

/// The real host device.
pub struct DeviceHost {
    pub device: ID3D12Device,
    /// `D3D12_HEAP_TYPE_GPU_UPLOAD` works (resizable BAR): the shadow table lives in VRAM, the
    /// CPU writes it through the BAR
    pub gpu_upload: bool,
}
// SAFETY: D3D12 devices are free-threaded.
unsafe impl Send for DeviceHost {}
unsafe impl Sync for DeviceHost {}

impl DeviceHost {
    pub fn new(device: &ID3D12Device) -> windows::core::Result<DeviceHost> {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
        let gpu_upload = unsafe {
            device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS16, (&mut o as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS16).cast(), size_of_val(&o) as u32)
        }
        .is_ok()
            && o.GPUUploadHeapSupported.as_bool();
        Ok(DeviceHost { device: device.clone(), gpu_upload })
    }

    fn null_fill(&self, heap: &HostHeap) -> windows::core::Result<()> {
        // one null in a CPU heap, doubled into a block, the block copied over the heap
        const BLOCK: u32 = 4096;
        let n = BLOCK.min(heap.count.max(1));
        let stage: ID3D12DescriptorHeap = unsafe {
            self.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE(heap.kind), NumDescriptors: n, ..Default::default() })
        }?;
        let base = unsafe { stage.GetCPUDescriptorHandleForHeapStart() }.ptr;
        let null = if heap.kind == SAMPLERS { HostView::Sampler(crate::sampler::default_desc()) } else { HostView::NullBuffer };
        self.write(base, &null);
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE(heap.kind);
        let h = |p: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: p };
        let mut have = 1;
        while have < n {
            let k = have.min(n - have);
            unsafe { self.device.CopyDescriptorsSimple(k, h(base + (have * heap.step) as usize), h(base), kind) };
            have += k;
        }
        let mut done = 0;
        while done < heap.count {
            let k = n.min(heap.count - done);
            unsafe { self.device.CopyDescriptorsSimple(k, h(heap.cpu_at(done)), h(base), kind) };
            done += k;
        }
        Ok(())
    }
}

fn hr(e: windows::core::Error) -> HRESULT {
    e.code()
}

impl HostDevice for DeviceHost {
    fn heap(&self, kind: i32, count: u32, visible: bool) -> Result<HostHeap, HRESULT> {
        let visible = visible && matches!(kind, VIEWS | SAMPLERS);
        let heap: ID3D12DescriptorHeap = unsafe {
            self.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE(kind),
                NumDescriptors: count.max(1),
                Flags: if visible { D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE } else { D3D12_DESCRIPTOR_HEAP_FLAG_NONE },
                NodeMask: 0,
            })
        }
        .map_err(hr)?;
        let step = unsafe { self.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE(kind)) };
        let cpu = unsafe { heap.GetCPUDescriptorHandleForHeapStart() }.ptr;
        let gpu = if visible { unsafe { heap.GetGPUDescriptorHandleForHeapStart() }.ptr } else { 0 };
        let h = HostHeap { heap: Some(heap), kind, count: count.max(1), cpu, gpu, step };
        if matches!(kind, VIEWS | SAMPLERS) {
            self.null_fill(&h).map_err(hr)?;
        }
        Ok(h)
    }

    fn write(&self, at: usize, view: &HostView) {
        let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: at };
        let d = &self.device;
        fn res(r: &Raw) -> Option<&ID3D12Resource> {
            // SAFETY: a live resource or null.
            unsafe { ID3D12Resource::from_raw_borrowed(r) }
        }
        // SAFETY: a slot of one of our heaps; resources are live (their console objects hold them).
        unsafe {
            match view {
                HostView::NullBuffer => {
                    let HostView::Srv { desc, .. } = HostView::raw_srv(std::ptr::null_mut(), 0, 1) else { unreachable!() };
                    d.CreateShaderResourceView(None, Some(&desc), at)
                }
                HostView::NullCbv => d.CreateConstantBufferView(Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC::default()), at),
                HostView::NullUav => {
                    let HostView::Uav { desc, .. } = HostView::raw_uav(std::ptr::null_mut(), 0, 1) else { unreachable!() };
                    d.CreateUnorderedAccessView(None, None, Some(&desc), at)
                }
                HostView::NullTexture(dim) => {
                    let mut desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        ViewDimension: D3D12_SRV_DIMENSION(*dim),
                        Shader4ComponentMapping: MAPPING,
                        ..Default::default()
                    };
                    // one mip, one slice: what a null view still checks
                    desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_SRV { MipLevels: 1, ArraySize: 1, ..Default::default() };
                    d.CreateShaderResourceView(None, Some(&desc), at)
                }
                HostView::Cbv { va, size } => d.CreateConstantBufferView(Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: *va, SizeInBytes: *size }), at),
                HostView::Srv { resource, desc } => d.CreateShaderResourceView(res(resource), Some(desc), at),
                HostView::Uav { resource, desc } => d.CreateUnorderedAccessView(res(resource), None, Some(desc), at),
                HostView::Rtv { resource, desc } => d.CreateRenderTargetView(res(resource), Some(desc), at),
                HostView::Dsv { resource, desc } => d.CreateDepthStencilView(res(resource), Some(desc), at),
                HostView::Sampler(s) => d.CreateSampler(s, at),
            }
        }
    }

    fn copy(&self, dst: usize, src: usize, kind: i32) {
        // SAFETY: two slots of our heaps, the source CPU-only.
        unsafe { self.device.CopyDescriptorsSimple(1, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dst }, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: src }, D3D12_DESCRIPTOR_HEAP_TYPE(kind)) }
    }

    fn shadow(&self, entries: u32) -> Result<Shadow, HRESULT> {
        let bytes = u64::from(entries.max(1)) * 16;
        let kind = if self.gpu_upload { D3D12_HEAP_TYPE_GPU_UPLOAD } else { D3D12_HEAP_TYPE_UPLOAD };
        let desc = crate::resource::buffer_desc(bytes, false);
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            self.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                if self.gpu_upload { D3D12_RESOURCE_STATE_COMMON } else { D3D12_RESOURCE_STATE_GENERIC_READ },
                None,
                &mut r,
            )
        }
        .map_err(hr)?;
        let r = r.ok_or(E_OUTOFMEMORY)?;
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe { r.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p)) }.map_err(hr)?;
        // SAFETY: the whole mapping, written once.
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0, bytes as usize) };
        let va = unsafe { r.GetGPUVirtualAddress() };
        Ok(Shadow { ptr: p.cast(), entries: entries.max(1), va, resource: Some(r), _keep: None })
    }

    fn resource(&self, desc: &D3D12_RESOURCE_DESC, heap: D3D12_HEAP_TYPE, flags: D3D12_HEAP_FLAGS, clear: Option<&D3D12_CLEAR_VALUE>) -> Result<Option<ID3D12Resource>, HRESULT> {
        let mut r: Option<ID3D12Resource> = None;
        let state = if heap == D3D12_HEAP_TYPE_UPLOAD { D3D12_RESOURCE_STATE_GENERIC_READ } else if heap == D3D12_HEAP_TYPE_READBACK { D3D12_RESOURCE_STATE_COPY_DEST } else { D3D12_RESOURCE_STATE_COMMON };
        unsafe { self.device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() }, flags, desc, state, clear.map(|c| c as *const _), &mut r) }.map_err(hr)?;
        Ok(r)
    }

    fn query_heap(&self, desc: &D3D12_QUERY_HEAP_DESC) -> Result<Option<ID3D12QueryHeap>, HRESULT> {
        let mut q: Option<ID3D12QueryHeap> = None;
        unsafe { self.device.CreateQueryHeap(desc, &mut q) }.map_err(hr)?;
        Ok(q)
    }

    fn command_signature(&self, desc: &D3D12_COMMAND_SIGNATURE_DESC, root: Option<&ID3D12RootSignature>) -> Result<Option<ID3D12CommandSignature>, HRESULT> {
        let mut c: Option<ID3D12CommandSignature> = None;
        unsafe { self.device.CreateCommandSignature(desc, root, &mut c) }.map_err(hr)?;
        Ok(c)
    }

    fn root_signature(&self, blob: &[u8]) -> Result<Option<ID3D12RootSignature>, HRESULT> {
        // SAFETY: a serialized root signature of `blob.len()` bytes
        unsafe { self.device.CreateRootSignature::<ID3D12RootSignature>(0, blob) }.map(Some).map_err(hr)
    }

    fn footprints(&self, desc: &D3D12_RESOURCE_DESC, first: u32, count: u32, base: u64, layouts: *mut D3D12_PLACED_SUBRESOURCE_FOOTPRINT, rows: *mut u32, row_bytes: *mut u64, total: *mut u64) {
        // SAFETY: the title passes room for `count` of each, or null.
        unsafe {
            self.device.GetCopyableFootprints(
                desc,
                first,
                count,
                base,
                (!layouts.is_null()).then_some(layouts),
                (!rows.is_null()).then_some(rows),
                (!row_bytes.is_null()).then_some(row_bytes),
                (!total.is_null()).then_some(total),
            )
        }
    }
}
