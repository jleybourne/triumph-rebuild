//! Just enough Direct3D 12 for the upscaler tests: a device, one direct queue, 2D textures, uploads
//! and read-backs.

use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::Size;

pub struct Gpu {
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    pub alloc: ID3D12CommandAllocator,
    pub list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
}

/// A 2D texture and the format facts the copies need.
pub struct Texture {
    pub resource: ID3D12Resource,
    pub size: Size,
    pub format: DXGI_FORMAT,
    pub bytes_per_pixel: u32,
    pub state: D3D12_RESOURCE_STATES,
}

fn bpp(f: DXGI_FORMAT) -> u32 {
    match f {
        DXGI_FORMAT_R16G16B16A16_FLOAT => 8,
        DXGI_FORMAT_R32_FLOAT | DXGI_FORMAT_R16G16_FLOAT | DXGI_FORMAT_R8G8B8A8_UNORM => 4,
        _ => panic!("format {f:?} not used here"),
    }
}

fn pitch(size: Size, bpp: u32) -> u32 {
    (size.width * bpp + 255) & !255
}

impl Gpu {
    /// The default adapter's device. Call after anything that must hook device creation (Streamline's
    /// `slInit`).
    pub fn new() -> Result<Gpu, String> {
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(None::<&windows::core::IUnknown>, D3D_FEATURE_LEVEL_12_0, &mut device).map_err(|e| format!("device: {e}"))?;
            let device = device.unwrap();
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })
                .map_err(|e| e.to_string())?;
            let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|e| e.to_string())?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(|e| e.to_string())?;
            list.Close().map_err(|e| e.to_string())?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| e.to_string())?;
            Ok(Gpu { device, queue, alloc, list, fence, value: 0 })
        }
    }

    /// The adapter's LUID, for SDK support queries.
    pub fn luid(&self) -> [u8; 8] {
        let l = unsafe { self.device.GetAdapterLuid() };
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&l.LowPart.to_le_bytes());
        out[4..].copy_from_slice(&l.HighPart.to_le_bytes());
        out
    }

    pub fn begin(&self) {
        unsafe {
            self.alloc.Reset().unwrap();
            self.list.Reset(&self.alloc, None).unwrap();
        }
    }

    /// Close, submit and wait.
    pub fn finish(&mut self) -> Result<(), String> {
        unsafe {
            self.list.Close().map_err(|e| format!("close: {e}"))?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.value += 1;
            self.queue.Signal(&self.fence, self.value).map_err(|e| e.to_string())?;
            self.fence.SetEventOnCompletion(self.value, HANDLE::default()).map_err(|e| e.to_string())?;
            self.device.GetDeviceRemovedReason().map_err(|e| format!("device removed: {e}"))
        }
    }

    fn buffer(&self, kind: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
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
        let mut r: Option<ID3D12Resource> = None;
        unsafe { self.device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }
            .expect("buffer");
        r.unwrap()
    }

    pub fn texture(&self, size: Size, format: DXGI_FORMAT, uav: bool, state: D3D12_RESOURCE_STATES) -> Texture {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: size.width as u64,
            Height: size.height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
            ..Default::default()
        };
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            self.device
                .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r)
        }
        .expect("texture");
        Texture { resource: r.unwrap(), size, format, bytes_per_pixel: bpp(format), state }
    }

    pub fn barrier(&self, t: &mut Texture, to: D3D12_RESOURCE_STATES) {
        if t.state == to {
            return;
        }
        let b = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: unsafe { std::mem::transmute_copy(&t.resource) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: t.state,
                    StateAfter: to,
                }),
            },
        };
        unsafe { self.list.ResourceBarrier(&[b]) };
        t.state = to;
    }

    fn footprint(t: &Texture) -> D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: t.format, Width: t.size.width, Height: t.size.height, Depth: 1, RowPitch: pitch(t.size, t.bytes_per_pixel) },
        }
    }

    /// Upload tightly packed rows into `t` (recorded into the open list; the staging buffer is returned
    /// and must live until the list has run).
    pub fn upload(&self, t: &mut Texture, data: &[u8]) -> ID3D12Resource {
        let row = (t.size.width * t.bytes_per_pixel) as usize;
        assert_eq!(data.len(), row * t.size.height as usize);
        let p = pitch(t.size, t.bytes_per_pixel) as usize;
        let staging = self.buffer(D3D12_HEAP_TYPE_UPLOAD, (p * t.size.height as usize) as u64, D3D12_RESOURCE_STATE_GENERIC_READ);
        unsafe {
            let mut m: *mut std::ffi::c_void = std::ptr::null_mut();
            staging.Map(0, None, Some(&mut m)).unwrap();
            for y in 0..t.size.height as usize {
                std::ptr::copy_nonoverlapping(data[y * row..].as_ptr(), (m as *mut u8).add(y * p), row);
            }
            staging.Unmap(0, None);
        }
        let old = t.state;
        self.barrier(t, D3D12_RESOURCE_STATE_COPY_DEST);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(&t.resource) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(&staging) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: Self::footprint(t) },
        };
        unsafe { self.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None) };
        self.barrier(t, old);
        staging
    }

    /// Record a copy of `t` into a read-back buffer; after `finish`, `read` gives the rows.
    pub fn copy_out(&self, t: &mut Texture) -> ID3D12Resource {
        let p = pitch(t.size, t.bytes_per_pixel) as usize;
        let back = self.buffer(D3D12_HEAP_TYPE_READBACK, (p * t.size.height as usize) as u64, D3D12_RESOURCE_STATE_COPY_DEST);
        let old = t.state;
        self.barrier(t, D3D12_RESOURCE_STATE_COPY_SOURCE);
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(&t.resource) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { std::mem::transmute_copy(&back) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: Self::footprint(t) },
        };
        unsafe { self.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None) };
        self.barrier(t, old);
        back
    }

    /// Tightly packed rows from a finished `copy_out`.
    pub fn read(t: &Texture, back: &ID3D12Resource) -> Vec<u8> {
        let row = (t.size.width * t.bytes_per_pixel) as usize;
        let p = pitch(t.size, t.bytes_per_pixel) as usize;
        let mut out = vec![0u8; row * t.size.height as usize];
        unsafe {
            let mut m: *mut std::ffi::c_void = std::ptr::null_mut();
            back.Map(0, None, Some(&mut m)).unwrap();
            for y in 0..t.size.height as usize {
                std::ptr::copy_nonoverlapping((m as *const u8).add(y * p), out[y * row..].as_mut_ptr(), row);
            }
            back.Unmap(0, None);
        }
        out
    }
}

/// RGBA f32 → R16G16B16A16_FLOAT bytes.
pub fn rgba16f(px: &[[f32; 4]]) -> Vec<u8> {
    px.iter().flat_map(|c| c.iter().flat_map(|v| crate::image::f16(*v).to_le_bytes())).collect()
}

/// R16G16B16A16_FLOAT bytes → RGBA f32.
pub fn from_rgba16f(b: &[u8]) -> Vec<[f32; 4]> {
    b.chunks_exact(8)
        .map(|c| {
            let h = |i: usize| crate::image::f32_of(u16::from_le_bytes([c[i], c[i + 1]]));
            [h(0), h(2), h(4), h(6)]
        })
        .collect()
}

/// Two-channel f32 → R16G16_FLOAT bytes.
pub fn rg16f(px: &[[f32; 2]]) -> Vec<u8> {
    px.iter().flat_map(|c| c.iter().flat_map(|v| crate::image::f16(*v).to_le_bytes())).collect()
}

/// f32 → R32_FLOAT bytes.
pub fn r32f(px: &[f32]) -> Vec<u8> {
    px.iter().flat_map(|v| v.to_le_bytes()).collect()
}
