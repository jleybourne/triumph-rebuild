//! One small D3D12 pass: the scene's quads over the target, alpha-blended.
//!
//! - **No vertex buffers, no descriptor heaps.** The quads (32 bytes each) are copied into the frame slot's upload
//!   buffer and read through a root SRV; the atlas is a raw buffer (one byte a pixel) behind a second root SRV; five
//!   root constants carry the target size, the atlas pitch, whether the target is linear (scRGB) and the atlas's
//!   last byte (every atlas read is clamped to it: a root SRV has no bounds check). One
//!   `DrawInstanced(4, quads)` as a triangle strip.
//! - **The atlas** is copied once into a default-heap buffer when the font changes (a new bake for a new height);
//!   the old one is freed when the GPU is past the frame that last read it.
//! - **Nothing of the title's**: the pass binds only its own root signature, pipeline, buffers and the target's
//!   RTV, and writes colour only (the target's alpha is untouched).
//! - **Timing**: two timestamps around the draw in each slot, read when the slot comes round again.
//!
//! The caller owns submission: [`Renderer::record`] returns the slot's list, open, with the target in
//! `RENDER_TARGET`; the caller adds its own transition (and a capture copy, if it wants the overlay in it) and
//! closes it.

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::core::PCSTR;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::font::PITCH;
use crate::layout::{Quad, Scene};

const HLSL: &str = r"
struct Quad { float4 rect; uint2 atlas; uint color; float radius; };
StructuredBuffer<Quad> quads : register(t0);
ByteAddressBuffer glyphs : register(t1);
cbuffer Frame : register(b0) { float2 to_clip; uint pitch; uint linear_out; uint atlas_last; };

struct V {
    float4 pos : SV_Position;
    nointerpolation float4 color : COLOR;
    nointerpolation uint2 atlas : ATLAS;
    nointerpolation float3 shape : SHAPE;
    float2 local : LOCAL;
};

V vs(uint v : SV_VertexID, uint i : SV_InstanceID) {
    Quad q = quads[i];
    float2 c = float2(v & 1, v >> 1);
    float2 p = q.rect.xy + c * q.rect.zw;
    V o;
    o.pos = float4(p * to_clip * float2(1, -1) + float2(-1, 1), 0, 1);
    o.color = float4(q.color & 255, (q.color >> 8) & 255, (q.color >> 16) & 255, q.color >> 24) / 255.0;
    o.atlas = q.atlas;
    o.shape = float3(q.rect.zw * 0.5, q.radius);
    o.local = c * q.rect.zw;
    return o;
}

float3 to_linear(float3 c) {
    return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float4 ps(V i) : SV_Target {
    float a = i.color.a;
    // [branch] and the clamp: a root SRV has no bounds check, and a flattened branch would load from the solid
    // quads' wrapped coordinates too (an out-of-bounds read: a page fault on a card)
    [branch] if (i.atlas.x != 0xffffffff) {
        uint2 t = i.atlas + uint2(i.local);
        uint addr = min(t.y * pitch + t.x, atlas_last);
        uint w = glyphs.Load(addr & ~3u);
        a *= ((w >> ((addr & 3u) * 8u)) & 255u) / 255.0;
    } else if (i.shape.z > 0) {
        float2 d = abs(i.local - i.shape.xy) - (i.shape.xy - i.shape.z);
        float dist = length(max(d, 0)) + min(max(d.x, d.y), 0) - i.shape.z;
        a *= saturate(0.5 - dist);
    }
    float3 rgb = linear_out != 0 ? to_linear(i.color.rgb) : i.color.rgb;
    return float4(rgb, a);
}
";

struct Slot {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    /// the quads' upload buffer, mapped, and its size in quads
    upload: Option<(ID3D12Resource, *mut u8, usize)>,
    timed: bool,
}

pub struct Renderer {
    device: ID3D12Device,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    format: DXGI_FORMAT,
    linear: bool,
    slots: Vec<Slot>,
    /// the atlas in use: its font's id and buffer
    atlas: Option<(u64, ID3D12Resource)>,
    /// resources kept until the fence passes the value
    retired: Vec<(u64, ID3D12Resource)>,
    queries: ID3D12QueryHeap,
    stamps: ID3D12Resource,
    cells: *const u64,
    frequency: u64,
    gpu_us: Option<f64>,
    /// an atlas made ahead on another thread ([`Renderer::prepare`]): font id, GPU buffer, filled upload buffer
    pending: Option<(u64, ID3D12Resource, ID3D12Resource)>,
}

/// Quads each slot's upload buffer holds from the start (a menu page is ~250).
const QUADS: usize = 1024;

// SAFETY: D3D12 objects are free-threaded; the mapped pointers belong to buffers the renderer owns, and the
// renderer is used by one thread at a time (behind the runtime's lock).
unsafe impl Send for Renderer {}

fn transition(r: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer D3D12 only reads; ManuallyDrop never releases it.
                pResource: unsafe { std::mem::transmute_copy(r) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A buffer in `kind` heap: upload buffers start in GENERIC_READ, others in COMMON.
fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: u64) -> Result<ID3D12Resource, String> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(4),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let state = match kind {
        D3D12_HEAP_TYPE_UPLOAD => D3D12_RESOURCE_STATE_GENERIC_READ,
        D3D12_HEAP_TYPE_READBACK => D3D12_RESOURCE_STATE_COPY_DEST,
        _ => D3D12_RESOURCE_STATE_COMMON,
    };
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }
        .map_err(|e| format!("buffer: {e}"))?;
    r.ok_or_else(|| "buffer".to_string())
}

fn map(r: &ID3D12Resource) -> Result<*mut u8, String> {
    let mut p: *mut c_void = std::ptr::null_mut();
    unsafe { r.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut p)) }.map_err(|e| format!("map: {e}"))?;
    Ok(p as *mut u8)
}

fn compile(entry: &str, target: &str) -> Result<ID3DBlob, String> {
    let entry = std::ffi::CString::new(entry).map_err(|e| e.to_string())?;
    let target_c = std::ffi::CString::new(target).map_err(|e| e.to_string())?;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // optimisation level 3
    let flags = 1 << 15;
    let r = unsafe {
        D3DCompile(HLSL.as_ptr().cast(), HLSL.len(), PCSTR::null(), None, None, PCSTR(entry.as_ptr().cast()), PCSTR(target_c.as_ptr().cast()), flags, 0, &mut code, Some(&mut errors))
    };
    match (r, code) {
        (Ok(()), Some(c)) => Ok(c),
        (r, _) => {
            let msg = errors.map(|e| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize())).into_owned() });
            Err(format!("compile {target}: {r:?} {}", msg.unwrap_or_default()))
        }
    }
}

fn bytecode(b: &ID3DBlob) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE { pShaderBytecode: unsafe { b.GetBufferPointer() }, BytecodeLength: unsafe { b.GetBufferSize() } }
}

/// Report a failed step of `record` once per process (the frame then goes without the overlay).
fn failed<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) {
    move |e| {
        static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !ONCE.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("overlay   {what}: {e}; the frame goes without the overlay");
        }
    }
}

/// Is a swap chain of this format linear (scRGB), so sRGB theme colours must be converted?
pub fn is_linear(format: DXGI_FORMAT) -> bool {
    format == DXGI_FORMAT_R16G16B16A16_FLOAT
}

impl Renderer {
    /// For targets of `format`, with `slots` frame slots; `frequency`: the queue's timestamp frequency (0 = no
    /// timing). Compiles its two shaders (a few ms): create it off the present thread.
    pub fn new(device: &ID3D12Device, format: DXGI_FORMAT, slots: usize, frequency: u64) -> Result<Renderer, String> {
        let vs = compile("vs", "vs_5_0")?;
        let ps = compile("ps", "ps_5_0")?;
        unsafe {
            let params = [
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 5 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_VERTEX,
                },
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                    Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 1, RegisterSpace: 0 } },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
                },
            ];
            let desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: params.len() as u32, pParameters: params.as_ptr(), NumStaticSamplers: 0, pStaticSamplers: std::ptr::null(), Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE };
            let mut blob: Option<ID3DBlob> = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None).map_err(|e| format!("root signature: {e}"))?;
            let blob = blob.ok_or("root signature")?;
            let root: ID3D12RootSignature = device
                .CreateRootSignature(0, std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()))
                .map_err(|e| format!("root signature: {e}"))?;
            let mut pso = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: bytecode(&vs),
                PS: bytecode(&ps),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC { FillMode: D3D12_FILL_MODE_SOLID, CullMode: D3D12_CULL_MODE_NONE, DepthClipEnable: true.into(), ..Default::default() },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            pso.BlendState.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                LogicOpEnable: false.into(),
                SrcBlend: D3D12_BLEND_SRC_ALPHA,
                DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D12_BLEND_OP_ADD,
                SrcBlendAlpha: D3D12_BLEND_ZERO,
                DestBlendAlpha: D3D12_BLEND_ONE,
                BlendOpAlpha: D3D12_BLEND_OP_ADD,
                LogicOp: D3D12_LOGIC_OP_NOOP,
                // colour only: the target's alpha stays the title's
                RenderTargetWriteMask: (D3D12_COLOR_WRITE_ENABLE_RED.0 | D3D12_COLOR_WRITE_ENABLE_GREEN.0 | D3D12_COLOR_WRITE_ENABLE_BLUE.0) as u8,
            };
            pso.RTVFormats[0] = format;
            let state: Result<ID3D12PipelineState, _> = device.CreateGraphicsPipelineState(&pso);
            // the pipeline holds its own reference: release the borrowed copy without a Release call
            let _: ManuallyDrop<Option<ID3D12RootSignature>> = std::mem::transmute_copy(&pso.pRootSignature);
            let pso = state.map_err(|e| format!("pipeline: {e}"))?;
            let mut list_slots = Vec::with_capacity(slots);
            for _ in 0..slots {
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|e| format!("allocator: {e}"))?;
                let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).map_err(|e| format!("list: {e}"))?;
                list.Close().map_err(|e| format!("list: {e}"))?;
                // warm: record (never execute) a frame's commands once, so the allocator's memory and the driver's
                // first-use work of the pipeline are paid here, not on the present thread's first overlay frame
                allocator.Reset().map_err(|e| format!("allocator: {e}"))?;
                list.Reset(&allocator, &pso).map_err(|e| format!("list: {e}"))?;
                list.SetGraphicsRootSignature(&root);
                list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                list.SetGraphicsRoot32BitConstants(0, 5, [0u32; 5].as_ptr() as *const c_void, 0);
                list.Close().map_err(|e| format!("list: {e}"))?;
                let b = buffer(device, D3D12_HEAP_TYPE_UPLOAD, (QUADS * std::mem::size_of::<Quad>()) as u64)?;
                let p = map(&b)?;
                list_slots.push(Slot { allocator, list, upload: Some((b, p, QUADS)), timed: false });
            }
            let queries: ID3D12QueryHeap = {
                let mut h: Option<ID3D12QueryHeap> = None;
                device
                    .CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2 * slots as u32, NodeMask: 0 }, &mut h)
                    .map_err(|e| format!("query heap: {e}"))?;
                h.ok_or("query heap")?
            };
            let stamps = buffer(device, D3D12_HEAP_TYPE_READBACK, 16 * slots as u64)?;
            let mut cells: *mut c_void = std::ptr::null_mut();
            stamps.Map(0, None, Some(&mut cells)).map_err(|e| format!("timestamps: {e}"))?;
            Ok(Renderer {
                device: device.clone(),
                root,
                pso,
                format,
                linear: is_linear(format),
                slots: list_slots,
                atlas: None,
                retired: Vec::new(),
                queries,
                stamps,
                cells: cells as *const u64,
                frequency,
                gpu_us: None,
                pending: None,
            })
        }
    }

    pub fn format(&self) -> DXGI_FORMAT {
        self.format
    }

    /// The GPU time of the last frame timed, µs (smoothed).
    pub fn gpu_us(&self) -> Option<f64> {
        self.gpu_us
    }

    /// Make `font`'s atlas buffers ahead, off the present thread (a new bake): [`Renderer::record`] then only
    /// records the copy. Creating them costs ~1 ms, more than the frame budget.
    pub fn prepare(&mut self, font: &crate::font::Font) -> Result<(), String> {
        if self.atlas.as_ref().is_some_and(|a| a.0 == font.id) || self.pending.as_ref().is_some_and(|p| p.0 == font.id) {
            return Ok(());
        }
        let bytes = font.atlas.len().next_multiple_of(4) as u64;
        let gpu = buffer(&self.device, D3D12_HEAP_TYPE_DEFAULT, bytes)?;
        let up = buffer(&self.device, D3D12_HEAP_TYPE_UPLOAD, bytes)?;
        let p = map(&up)?;
        unsafe {
            std::ptr::copy_nonoverlapping(font.atlas.as_ptr(), p, font.atlas.len());
            up.Unmap(0, None);
        }
        // a pending atlas never went to the GPU: dropped at once
        self.pending = Some((font.id, gpu, up));
        Ok(())
    }

    /// Record `scene` over `target` (in `state`, viewed by `rtv`) in frame slot `slot`. `done`: the fence value the
    /// GPU has passed; `next`: the value the submission of this list will signal. The slot's previous list must be
    /// done (the present only reuses a slot once its fence passed). Returns the list, open, the target in
    /// `RENDER_TARGET`.
    ///
    /// # Safety
    /// `target` and `rtv` are live and match; the list is executed (or dropped unexecuted) before `slot` comes round.
    pub unsafe fn record(
        &mut self,
        slot: usize,
        scene: &Scene,
        target: &ID3D12Resource,
        state: D3D12_RESOURCE_STATES,
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
        done: u64,
        next: u64,
    ) -> Option<ID3D12GraphicsCommandList> {
        let n = scene.quads.len();
        if n == 0 || slot >= self.slots.len() {
            return None;
        }
        self.retired.retain(|(v, _)| *v > done);
        // the slot's last frame is done: its GPU time
        if self.slots[slot].timed && self.frequency > 0 {
            let (a, b) = unsafe { (self.cells.add(2 * slot).read_volatile(), self.cells.add(2 * slot + 1).read_volatile()) };
            if b > a {
                let us = (b - a) as f64 * 1e6 / self.frequency as f64;
                self.gpu_us = Some(self.gpu_us.map_or(us, |g| g * 0.9 + us * 0.1));
            }
        }
        // the quads
        if self.slots[slot].upload.as_ref().is_none_or(|u| u.2 < n) {
            let cap = n.next_power_of_two().max(256);
            let b = buffer(&self.device, D3D12_HEAP_TYPE_UPLOAD, (cap * std::mem::size_of::<Quad>()) as u64).map_err(failed("quad buffer")).ok()?;
            let p = map(&b).map_err(failed("quad map")).ok()?;
            self.slots[slot].upload = Some((b, p, cap));
        }
        let (quads, mapped, _) = self.slots[slot].upload.clone()?;
        unsafe { std::ptr::copy_nonoverlapping(scene.quads.as_ptr() as *const u8, mapped, n * std::mem::size_of::<Quad>()) };

        // the atlas's buffers before the list is reset (made here only when no one prepared them)
        if self.atlas.as_ref().is_none_or(|a| a.0 != scene.font.id) && self.pending.as_ref().is_none_or(|p| p.0 != scene.font.id) {
            self.prepare(&scene.font).map_err(failed("atlas")).ok()?;
        }
        let s = &self.slots[slot];
        let (allocator, list) = (s.allocator.clone(), s.list.clone());
        unsafe {
            allocator.Reset().map_err(failed("allocator reset")).ok()?;
            list.Reset(&allocator, &self.pso).map_err(failed("list reset")).ok()?;
        }
        // the atlas, once per font
        if self.atlas.as_ref().is_none_or(|a| a.0 != scene.font.id) {
            let (_, gpu, up) = self.pending.take()?;
            unsafe {
                list.CopyBufferRegion(&gpu, 0, &up, 0, scene.font.atlas.len() as u64);
                list.ResourceBarrier(&[transition(&gpu, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE)]);
            }
            self.retired.push((next, up));
            if let Some((_, old)) = self.atlas.replace((scene.font.id, gpu)) {
                self.retired.push((next, old));
            }
        }
        let atlas = self.atlas.as_ref().map(|a| a.1.clone())?;
        let (w, h) = scene.size;
        unsafe {
            if state != D3D12_RESOURCE_STATE_RENDER_TARGET {
                list.ResourceBarrier(&[transition(target, state, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
            }
            list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot as u32);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 }]);
            list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }]);
            list.SetGraphicsRootSignature(&self.root);
            let last = (scene.font.atlas.len().max(1) - 1) as u32;
            let constants: [u32; 5] = [(2.0 / w.max(1) as f32).to_bits(), (2.0 / h.max(1) as f32).to_bits(), PITCH, u32::from(self.linear), last];
            list.SetGraphicsRoot32BitConstants(0, 5, constants.as_ptr() as *const c_void, 0);
            list.SetGraphicsRootShaderResourceView(1, quads.GetGPUVirtualAddress());
            list.SetGraphicsRootShaderResourceView(2, atlas.GetGPUVirtualAddress());
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            list.DrawInstanced(4, n as u32, 0, 0);
            list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot as u32 + 1);
            list.ResolveQueryData(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot as u32, 2, &self.stamps, 16 * slot as u64);
        }
        self.slots[slot].timed = true;
        Some(list)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { self.stamps.Unmap(0, None) };
    }
}

#[cfg(test)]
mod tests {
    //! On WARP: the overlay drawn into a texture changes pixels only inside its panels.

    use super::*;
    use crate::actions::Registry;
    use crate::font::Font;
    use crate::layout::{self, SOLID};
    use crate::menu::tests::{menu, Fake};
    use crate::menu::{Menu, Nav, Stats};
    use crate::theme;
    use rebuild_config::Config;
    use std::sync::Arc;
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, IDXGIAdapter, IDXGIFactory4, DXGI_CREATE_FACTORY_FLAGS};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

    pub(crate) struct Gpu {
        pub device: ID3D12Device,
        pub queue: ID3D12CommandQueue,
        pub fence: ID3D12Fence,
        pub value: u64,
    }

    impl Gpu {
        pub fn new(warp: bool) -> Gpu {
            unsafe {
                // REBUILD_OVERLAY_DEBUG=1: the D3D12 debug layer, its messages printed by `messages`
                if std::env::var_os("REBUILD_OVERLAY_DEBUG").is_some() {
                    let mut d: Option<ID3D12Debug> = None;
                    if D3D12GetDebugInterface(&mut d).is_ok() {
                        if let Some(d) = d {
                            d.EnableDebugLayer();
                            if let Ok(d1) = d.cast::<ID3D12Debug1>() {
                                d1.SetEnableGPUBasedValidation(true);
                            }
                        }
                    }
                }
                let f: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).expect("factory");
                let adapter: IDXGIAdapter = if warp { f.EnumWarpAdapter().expect("WARP") } else { f.EnumAdapters(0).expect("adapter 0") };
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device).expect("device");
                let device = device.unwrap();
                let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).expect("queue");
                let fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).expect("fence");
                Gpu { device, queue, fence, value: 0 }
            }
        }

        /// The debug layer's messages so far (with REBUILD_OVERLAY_DEBUG=1).
        pub fn messages(&self) -> Vec<String> {
            let Ok(q) = self.device.cast::<ID3D12InfoQueue>() else { return Vec::new() };
            let mut out = Vec::new();
            unsafe {
                for i in 0..q.GetNumStoredMessages() {
                    let mut len = 0usize;
                    let _ = q.GetMessage(i, None, &mut len);
                    let mut buf = vec![0u8; len];
                    let m = buf.as_mut_ptr() as *mut D3D12_MESSAGE;
                    if q.GetMessage(i, Some(m), &mut len).is_ok() {
                        out.push(String::from_utf8_lossy(std::ffi::CStr::from_ptr((*m).pDescription as *const _).to_bytes()).into_owned());
                    }
                }
            }
            out
        }

        pub fn submit(&mut self, lists: &[Option<ID3D12CommandList>]) -> u64 {
            unsafe {
                self.queue.ExecuteCommandLists(lists);
                self.value += 1;
                self.queue.Signal(&self.fence, self.value).unwrap();
            }
            self.value
        }

        pub fn wait(&self) {
            unsafe {
                if self.fence.GetCompletedValue() >= self.value {
                    return;
                }
                let e = CreateEventW(None, false, false, None).unwrap();
                self.fence.SetEventOnCompletion(self.value, e).unwrap();
                WaitForSingleObject(e, INFINITE);
                let _ = windows::Win32::Foundation::CloseHandle(e);
            }
        }

        pub fn texture(&self, w: u32, h: u32, format: DXGI_FORMAT) -> (ID3D12Resource, ID3D12DescriptorHeap) {
            unsafe {
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(w),
                    Height: h,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                    ..Default::default()
                };
                let mut r: Option<ID3D12Resource> = None;
                self.device
                    .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut r)
                    .expect("texture");
                let heap: ID3D12DescriptorHeap = self.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() }).unwrap();
                let r = r.unwrap();
                self.device.CreateRenderTargetView(&r, None, heap.GetCPUDescriptorHandleForHeapStart());
                (r, heap)
            }
        }
    }

    /// Copy `target` (COPY_SOURCE) into a readback buffer in `list`; read it after the GPU is done.
    unsafe fn readback(device: &ID3D12Device, list: &ID3D12GraphicsCommandList, target: &ID3D12Resource, w: u32, h: u32) -> (ID3D12Resource, u32) {
        let pitch = (w * 4).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
        let b = buffer(device, D3D12_HEAP_TYPE_READBACK, u64::from(pitch) * u64::from(h)).unwrap();
        let fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: 0, Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Width: w, Height: h, Depth: 1, RowPitch: pitch } };
        let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: unsafe { std::mem::transmute_copy(&b) }, Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: unsafe { std::mem::transmute_copy(target) }, Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
        unsafe { list.CopyTextureRegion(&dst, 0, 0, 0, &src, None) };
        (b, pitch)
    }

    fn pixels(b: &ID3D12Resource, pitch: u32, w: u32, h: u32) -> Vec<u32> {
        let p = map(b).unwrap();
        let mut out = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            let row = unsafe { std::slice::from_raw_parts(p.add((y * pitch) as usize) as *const u32, w as usize) };
            out.extend_from_slice(row);
        }
        out
    }

    /// The title's image stand-in: a gradient, so a blend that changes nothing would still be seen.
    const BACKGROUND: [f32; 4] = [0.25, 0.5, 0.75, 1.0];

    fn draw(gpu: &mut Gpu, r: &mut Renderer, scene: &Scene, slot: usize) -> (Vec<u32>, Vec<u32>) {
        let (w, h) = scene.size;
        let (target, rtv) = gpu.texture(w, h, DXGI_FORMAT_R8G8B8A8_UNORM);
        unsafe {
            let a: ID3D12CommandAllocator = gpu.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let pre: ID3D12GraphicsCommandList = gpu.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).unwrap();
            let handle = rtv.GetCPUDescriptorHandleForHeapStart();
            pre.ClearRenderTargetView(handle, &BACKGROUND, None);
            // a few stripes of other colours
            for i in 0..8 {
                let rect = RECT { left: i * w as i32 / 8, top: 0, right: i * w as i32 / 8 + 7, bottom: h as i32 };
                pre.ClearRenderTargetView(handle, &[i as f32 / 8.0, 0.1, 0.9, 1.0], Some(&[rect]));
            }
            pre.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let (before, pitch) = readback(&gpu.device, &pre, &target, w, h);
            pre.Close().unwrap();
            gpu.submit(&[Some(pre.cast().unwrap())]);
            let done = gpu.fence.GetCompletedValue();
            let list = r.record(slot, scene, &target, D3D12_RESOURCE_STATE_COPY_SOURCE, handle, done, gpu.value + 1).expect("recorded");
            list.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let (after, _) = readback(&gpu.device, &list, &target, w, h);
            list.Close().unwrap();
            gpu.submit(&[Some(list.cast().unwrap())]);
            gpu.wait();
            (pixels(&before, pitch, w, h), pixels(&after, pitch, w, h))
        }
    }

    fn check(scene: &Scene, before: &[u32], after: &[u32]) -> usize {
        let (w, _) = scene.size;
        let mut changed = 0;
        for (i, (a, b)) in before.iter().zip(after).enumerate() {
            if a == b {
                continue;
            }
            changed += 1;
            let (x, y) = ((i as u32 % w) as f32 + 0.5, (i as u32 / w) as f32 + 0.5);
            assert!(scene.panels.iter().any(|r| x >= r[0] && y >= r[1] && x <= r[0] + r[2] && y <= r[1] + r[3]), "pixel ({x}, {y}) changed outside every panel");
            assert_eq!(a >> 24, b >> 24, "alpha changed at ({x}, {y})");
        }
        changed
    }

    #[test]
    fn warp_the_overlay_changes_pixels_only_inside_its_panel() {
        let mut gpu = Gpu::new(true);
        let mut r = Renderer::new(&gpu.device, DXGI_FORMAT_R8G8B8A8_UNORM, 3, 0).expect("renderer");
        let mut b = Fake::default();
        let mut m = menu(&b);
        let stats = Stats { seq: 1, fps: Some(30.0), ms: Some(33.3), draws: Some(4512), ..Stats::default() };
        let size = (1280, 720);
        let font = Arc::new(Font::bake(theme::FONT_FACE, theme::FONT_WEIGHT, theme::font_px(size.1)).expect("font"));
        for (slot, page) in [0usize, 1, 2].into_iter().enumerate() {
            let scene = layout::build(&m, &stats, true, false, size, font.clone(), "F10");
            let (before, after) = draw(&mut gpu, &mut r, &scene, slot);
            let changed = check(&scene, &before, &after);
            let r = scene.panels[0];
            let area = (r[2] * r[3]) as usize;
            // the panel is translucent over everything inside it: nearly all of it changes
            assert!(changed > area * 9 / 10, "page {page}: {changed} of {area} panel pixels changed");
            // text was drawn: the panel holds pixels much brighter than its background
            let bright = after.iter().filter(|p| (*p & 0xFF) > 200 && (*p >> 8 & 0xFF) > 200).count();
            assert!(bright > 200, "page {page}: {bright} bright pixels");
            m.nav(Nav::NextPage, &mut b);
        }
        // the readout alone (menu closed)
        let m = Menu::new(Config::default(), &b, Registry::default());
        let scene = layout::build(&m, &stats, false, true, size, font.clone(), "F10");
        let (before, after) = draw(&mut gpu, &mut r, &scene, 0);
        assert!(check(&scene, &before, &after) > 0);
        // nothing to draw: no list
        let empty = Scene::empty(size, font);
        let (target, rtv) = gpu.texture(16, 16, DXGI_FORMAT_R8G8B8A8_UNORM);
        assert!(unsafe { r.record(1, &empty, &target, D3D12_RESOURCE_STATE_RENDER_TARGET, rtv.GetCPUDescriptorHandleForHeapStart(), 0, 1) }.is_none());
    }

    #[test]
    fn warp_a_glyph_is_drawn_exactly_from_its_atlas_pixels() {
        // one synthetic glyph (solid ink box) and one solid square: exact rectangles, exact colours
        let mut gpu = Gpu::new(true);
        let mut r = Renderer::new(&gpu.device, DXGI_FORMAT_R8G8B8A8_UNORM, 3, 0).expect("renderer");
        let font = Arc::new(Font::synthetic(20));
        let g = *font.glyph('A').unwrap();
        let red = theme::rgba(255, 0, 0, 255);
        let quads = vec![
            Quad { rect: [10.0, 12.0, f32::from(g.w), f32::from(g.h)], atlas: [u32::from(g.x), u32::from(g.y)], color: red, radius: 0.0 },
            Quad { rect: [40.0, 8.0, 6.0, 5.0], atlas: [SOLID, 0], color: theme::rgba(0, 255, 0, 255), radius: 0.0 },
        ];
        let panels = vec![[0.0, 0.0, 64.0, 32.0]];
        let scene = Scene { size: (64, 32), font, quads, panels };
        let (before, after) = draw(&mut gpu, &mut r, &scene, 2);
        let px = |x: u32, y: u32| after[(y * 64 + x) as usize] & 0x00FF_FFFF;
        let mut red_n = 0;
        let mut green_n = 0;
        for y in 0..32 {
            for x in 0..64 {
                match px(x, y) {
                    0x0000FF => red_n += 1,
                    0x00FF00 => green_n += 1,
                    _ => assert_eq!(after[(y * 64 + x) as usize], before[(y * 64 + x) as usize], "({x}, {y})"),
                }
            }
        }
        assert_eq!(red_n, u32::from(g.w) * u32::from(g.h));
        assert_eq!(green_n, 30);
        assert_eq!(px(10, 12), 0x0000FF);
        assert_eq!(px(40, 8), 0x00FF00);
    }

    /// Every page as a PNG over the test background, for the design pass: `REBUILD_OVERLAY_SNAPSHOTS=<folder>`
    /// `cargo test --release -p rebuild-overlay snapshots -- --ignored`.
    #[test]
    #[ignore = "writes PNGs to REBUILD_OVERLAY_SNAPSHOTS"]
    fn warp_snapshots() {
        let Some(dir) = std::env::var_os("REBUILD_OVERLAY_SNAPSHOTS").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        let mut gpu = Gpu::new(true);
        let mut r = Renderer::new(&gpu.device, DXGI_FORMAT_R8G8B8A8_UNORM, 3, 0).expect("renderer");
        let mut b = Fake::default();
        let mut m = menu(&b);
        m.nav(Nav::Down, &mut b);
        m.nav(Nav::Right, &mut b);
        let stats = Stats { seq: 1, fps: Some(30.04), ms: Some(33.29), gpu: Some(21.0), wait: Some(6.1), draws: Some(4512), dispatches: Some(120), dropped: Some(0), late: Some(0), internal: Some((2560, 1440)), output: Some((3840, 2160)), overlay_cpu_us: Some(21.0), overlay_gpu_us: Some(9.0), pacing: "frame rate title (30.0 fps, the title's interval) | vsync off (tearing allowed)".into(), cpu_pct: Some(23.0), ram_mb: Some(7300.0), commit_mb: Some(9100.0), vram_mb: Some((1100.0, 10900.0)), upload: Some((4.2, 0.03)) };
        for size in [(1920u32, 1080u32), (3840, 2160)] {
            let font = Arc::new(Font::bake(theme::FONT_FACE, theme::FONT_WEIGHT, theme::font_px(size.1)).expect("font"));
            for page in 0..3 {
                let scene = layout::build(&m, &stats, true, false, size, font.clone(), "F10");
                let (_, after) = draw(&mut gpu, &mut r, &scene, page);
                let path = dir.join(format!("overlay-{}p-{page}.png", size.1));
                let mut e = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(&path).unwrap()), size.0, size.1);
                e.set_color(png::ColorType::Rgba);
                e.set_depth(png::BitDepth::Eight);
                let bytes: Vec<u8> = after.iter().flat_map(|p| p.to_le_bytes()).collect();
                e.write_header().unwrap().write_image_data(&bytes).unwrap();
                m.nav(Nav::NextPage, &mut b);
            }
        }
    }

    /// The overlay's cost on the card at 4K: CPU to record a frame (what the present thread pays) and GPU time of
    /// the pass. Run by hand (the card lock is taken; `agents\MEASURING` must be absent):
    /// `cargo test --release -p rebuild-overlay card_ -- --ignored --nocapture`.
    #[test]
    #[ignore = "the card: run by hand"]
    fn card_overlay_cost_at_4k() {
        // REBUILD_OVERLAY_BENCH_WARP=1: the same loop on WARP (no card, no lock)
        let warp = std::env::var_os("REBUILD_OVERLAY_BENCH_WARP").is_some();
        let _lock = (!warp).then(|| rebuild_cardlock::CardLock::take().expect("card lock"));
        let mut gpu = Gpu::new(warp);
        let freq = unsafe { gpu.queue.GetTimestampFrequency() }.unwrap();
        let mut r = Renderer::new(&gpu.device, DXGI_FORMAT_R8G8B8A8_UNORM, 3, freq).expect("renderer");
        let size = (3840u32, 2160u32);
        let (target, rtv) = gpu.texture(size.0, size.1, DXGI_FORMAT_R8G8B8A8_UNORM);
        // a render target must be initialised (clear, discard or copy) before it is drawn on: the swap chain's
        // buffer always is (the title's image was copied or blitted into it)
        unsafe {
            let a: ID3D12CommandAllocator = gpu.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let l: ID3D12GraphicsCommandList = gpu.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).unwrap();
            l.ClearRenderTargetView(rtv.GetCPUDescriptorHandleForHeapStart(), &BACKGROUND, None);
            l.Close().unwrap();
            gpu.submit(&[Some(l.cast().unwrap())]);
            gpu.wait();
        }
        let t = std::time::Instant::now();
        let font = Arc::new(Font::bake(theme::FONT_FACE, theme::FONT_WEIGHT, theme::font_px(size.1)).expect("font"));
        let bake = t.elapsed();
        // as the runtime does: the atlas buffers made on the input thread, after the bake
        r.prepare(&font).expect("prepare");
        let mut b = Fake::default();
        let mut m = menu(&b);
        let stats = Stats { seq: 1, fps: Some(30.0), ms: Some(33.3), gpu: Some(21.0), draws: Some(4512), dispatches: Some(120), dropped: Some(0), pacing: "frame rate title | vsync off".into(), ..Stats::default() };
        let mut layout_us = Vec::new();
        let mut scenes = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            scenes.push(layout::build(&m, &stats, true, false, size, font.clone(), "F10"));
            layout_us.push(t.elapsed().as_secs_f64() * 1e6);
            m.nav(Nav::NextPage, &mut b);
        }
        let mut cpu = Vec::new();
        let t0 = std::time::Instant::now();
        for i in 0..600 {
            let slot = i % 3;
            let scene = &scenes[(i / 60) % 3];
            let t = std::time::Instant::now();
            let list = unsafe { r.record(slot, scene, &target, D3D12_RESOURCE_STATE_RENDER_TARGET, rtv.GetCPUDescriptorHandleForHeapStart(), gpu.fence.GetCompletedValue(), gpu.value + 1) }
                .unwrap_or_else(|| panic!("frame {i}: no list; device {:?}, {:.1} s in; {:#?}", unsafe { gpu.device.GetDeviceRemovedReason() }, t0.elapsed().as_secs_f64(), gpu.messages()));
            unsafe { list.Close().unwrap() };
            cpu.push(t.elapsed().as_secs_f64() * 1e6);
            gpu.submit(&[Some(list.cast().unwrap())]);
            gpu.wait();
            if let Err(e) = unsafe { gpu.device.GetDeviceRemovedReason() } {
                panic!("frame {i} removed the device: {e:?}; {:#?}", gpu.messages());
            }
        }
        let first = cpu[0];
        let mut steady = cpu[3..].to_vec();
        steady.sort_by(f64::total_cmp);
        let mean = steady.iter().sum::<f64>() / steady.len() as f64;
        let p99 = steady[steady.len() * 99 / 100];
        let g = r.gpu_us().expect("timed");
        println!(
            "overlay 4K: quads {} / {} / {}; record CPU mean {mean:.1} us, p99 {p99:.1} us, first frame (atlas upload) {first:.1} us; GPU {g:.1} us (smoothed); layout {:.1?} us; font bake {bake:?}",
            scenes[0].quads.len(),
            scenes[1].quads.len(),
            scenes[2].quads.len(),
            layout_us
        );
        let bad: Vec<String> = gpu.messages().into_iter().filter(|m| m.contains("VALIDATION") || m.contains("ERROR")).collect();
        assert!(bad.is_empty(), "{bad:#?}");
        if !warp {
            assert!(mean < 300.0 && g < 300.0, "over the 0.3 ms budget");
        }
    }
}
