//! The vertex path: seeded vertex attributes through the input assembler (every attribute either side declares),
//! points drawn with two instances, and every output-signature component captured by stream output, compared
//! word by word (with the filled-size counter).

use std::ffi::CString;

use rebuild_shader::signature::Element;
use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::gpu::{buffer_desc, Gpu, Res};
use super::run::{bytes_of, word};

pub const VERTICES: u32 = 64;
pub const INSTANCES: u32 = 2;

/// One attribute in the vertex buffer.
pub struct Attr {
    pub name: CString,
    pub index: u32,
    pub format: DXGI_FORMAT,
    pub offset: u32,
    /// (component type, components)
    pub comp: u32,
    pub n: u32,
}

/// One captured output: (semantic, index, first component, components).
pub struct Captured {
    pub name: CString,
    pub index: u32,
    pub start: u8,
    pub count: u8,
}

pub struct Setup {
    pub attrs: Vec<Attr>,
    pub stride: u32,
    pub outs: Vec<Captured>,
    pub so_stride: u32,
}

fn bytes_per(comp: u32) -> u32 {
    if (4..=6).contains(&comp) {
        2
    } else {
        4
    }
}

/// The attributes are the union of both sides' non-system inputs; the capture is our output signature.
pub fn setup(ours_in: &[Element], oracle_in: &[Element], outs: &[Element]) -> Result<Setup, String> {
    let mut attrs: Vec<Attr> = Vec::new();
    let mut at = 0u32;
    for e in ours_in.iter().chain(oracle_in) {
        if e.system != 0 {
            continue;
        }
        let n = 8 - e.mask.leading_zeros();
        if let Some(a) = attrs.iter().find(|a| a.name.as_bytes() == e.name.as_bytes() && a.index == e.index) {
            if a.n < n || a.comp != e.comp {
                return Err(format!("attribute {}{} declared differently by the two sides", e.name, e.index));
            }
            continue;
        }
        let format = rebuild_pipelines::d3d::format(e.comp, n);
        // 16-bit formats come in 1, 2 or 4 components
        let stored = if bytes_per(e.comp) == 2 && n == 3 { 4 } else { n };
        attrs.push(Attr { name: CString::new(e.name.as_str()).map_err(|e| e.to_string())?, index: e.index, format, offset: at, comp: e.comp, n: stored });
        at += (stored * bytes_per(e.comp)).next_multiple_of(4);
    }
    let mut caps = Vec::new();
    let mut so_stride = 0;
    for e in outs {
        if e.mask == 0 {
            continue;
        }
        if bytes_per(e.comp) == 2 {
            return Err("16-bit vertex outputs are not captured by stream output".into());
        }
        // stream output names a semantic's own components from 0, wherever the element sits in its register
        let (start, count) = (0u8, e.mask.count_ones() as u8);
        caps.push(Captured { name: CString::new(e.name.as_str()).map_err(|e| e.to_string())?, index: e.index, start, count });
        so_stride += 4 * count as u32;
    }
    if caps.is_empty() {
        return Err("no outputs to capture".into());
    }
    Ok(Setup { attrs, stride: at.max(4), outs: caps, so_stride })
}

/// f32 → f16 bits for the exactly representable values the attributes use.
fn half(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xff) as i32 - 127 + 15;
    if x == 0.0 {
        return sign;
    }
    if e <= 0 {
        return sign;
    }
    sign | ((e as u16) << 10) | ((b >> 13) & 0x3ff) as u16
}

/// The vertex buffer for input set `set`.
pub fn vertices(s: &Setup, set: u32) -> Vec<u8> {
    let mut v = vec![0u8; (s.stride * VERTICES) as usize];
    for i in 0..VERTICES as u64 {
        for (k, a) in s.attrs.iter().enumerate() {
            for c in 0..a.n as u64 {
                let salt = 300 + k as u64;
                let idx = i * 4 + c;
                let at = (i as u32 * s.stride + a.offset) as usize;
                match a.comp {
                    1 | 2 => v[at + 4 * c as usize..at + 4 * c as usize + 4].copy_from_slice(&word(0, salt ^ set as u64, idx).to_le_bytes()),
                    4 | 5 => v[at + 2 * c as usize..at + 2 * c as usize + 2].copy_from_slice(&((word(0, salt ^ set as u64, idx) & 0xff) as u16).to_le_bytes()),
                    6 => {
                        let x = (word(0, salt ^ set as u64, idx) as f32 - 3.5) * 0.25;
                        v[at + 2 * c as usize..at + 2 * c as usize + 2].copy_from_slice(&half(x).to_le_bytes());
                    }
                    _ => {
                        let x = f32::from_bits(word(1, salt, idx ^ (set as u64) << 32));
                        v[at + 4 * c as usize..at + 4 * c as usize + 4].copy_from_slice(&x.to_bits().to_le_bytes());
                    }
                }
            }
        }
    }
    v
}

/// Stream output: a 16-byte filled-size counter (starts at 0), then the records (start seeded, so a write shows).
pub fn so_bytes(s: &Setup) -> u64 {
    16 + (s.so_stride * VERTICES * INSTANCES) as u64
}

pub fn make(g: &Gpu, s: &Setup, set: u32) -> Result<(Res, Res), String> {
    let vb = g.resource(buffer_desc(s.stride as u64 * VERTICES as u64, false), D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER, false, vertices(s, set))?;
    let n = so_bytes(s);
    let mut init = vec![0u8; 16];
    init.extend(bytes_of((0..(n - 16) / 4).map(|i| word(set, 777, i))));
    let so = g.resource(buffer_desc(n, false), D3D12_RESOURCE_STATE_STREAM_OUT, true, init)?;
    Ok((vb, so))
}

pub fn pipeline(g: &Gpu, root: &ID3D12RootSignature, vs: &[u8], s: &Setup) -> Result<ID3D12PipelineState, String> {
    let input: Vec<D3D12_INPUT_ELEMENT_DESC> = s
        .attrs
        .iter()
        .map(|a| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(a.name.as_ptr() as *const u8),
            SemanticIndex: a.index,
            Format: a.format,
            InputSlot: 0,
            AlignedByteOffset: a.offset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        })
        .collect();
    let decl: Vec<D3D12_SO_DECLARATION_ENTRY> = s
        .outs
        .iter()
        .map(|c| D3D12_SO_DECLARATION_ENTRY { Stream: 0, SemanticName: PCSTR(c.name.as_ptr() as *const u8), SemanticIndex: c.index, StartComponent: c.start, ComponentCount: c.count, OutputSlot: 0 })
        .collect();
    let strides = [s.so_stride];
    let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: unsafe { std::mem::transmute_copy(root) },
        VS: D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr() as *const _, BytecodeLength: vs.len() },
        StreamOutput: D3D12_STREAM_OUTPUT_DESC {
            pSODeclaration: decl.as_ptr(),
            NumEntries: decl.len() as u32,
            pBufferStrides: strides.as_ptr(),
            NumStrides: 1,
            RasterizedStream: D3D12_SO_NO_RASTERIZED_STREAM,
        },
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC { FillMode: D3D12_FILL_MODE_SOLID, CullMode: D3D12_CULL_MODE_NONE, DepthClipEnable: true.into(), ..Default::default() },
        InputLayout: D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: input.as_ptr(), NumElements: input.len() as u32 },
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_POINT,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    g.graphics(&desc)
}

/// Records the draw: the vertices in, the outputs streamed out.
pub fn draw(g: &Gpu, s: &Setup, vb: &Res, so: &Res) {
    unsafe {
        let l = &g.list;
        let base = so.r.GetGPUVirtualAddress();
        l.SOSetTargets(0, Some(&[D3D12_STREAM_OUTPUT_BUFFER_VIEW { BufferLocation: base + 16, SizeInBytes: so_bytes(s) - 16, BufferFilledSizeLocation: base }]));
        l.IASetVertexBuffers(0, Some(&[D3D12_VERTEX_BUFFER_VIEW { BufferLocation: vb.r.GetGPUVirtualAddress(), SizeInBytes: s.stride * VERTICES, StrideInBytes: s.stride }]));
        l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        l.DrawInstanced(VERTICES, INSTANCES, 0, 0);
        l.SOSetTargets(0, None);
    }
}
