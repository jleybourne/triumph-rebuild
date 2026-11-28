//! The pixel path: a vertex shader that feeds the pixel shader's inputs (built from our pixel shader's own input
//! struct, so both sides link to it), a quad over a 64 × 64 target with a varying w (perspective-correct and
//! linear interpolation differ, so a wrong interpolation mode shows), full 32-bit render targets and a 32-bit depth
//! buffer, read back and compared word by word.

use std::path::Path;

use rebuild_shader::signature::Element;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::gpu::{texture_desc, Gpu, Res};

pub const SIZE: u32 = 64;

/// System values the rasteriser makes: not vertex shader outputs.
const RASTER_MADE: [&str; 7] = ["SV_IsFrontFace", "SV_SampleIndex", "SV_PrimitiveID", "SV_Barycentrics", "SV_Coverage", "SV_InnerCoverage", "SV_ShadingRate"];

/// The vertex shader for a pixel shader whose HLSL declares `struct In { ... };`: every interpolant a seeded
/// affine function of the quad's (u, v), integers constant per field, SV_Position covering the target with
/// w in [1, 2.25].
pub fn feed_vs(src: &str) -> Result<String, String> {
    let start = src.find("struct In {").ok_or("no input struct")?;
    let body = &src[start + "struct In {".len()..];
    let body = &body[..body.find("};").ok_or("input struct not closed")?];
    let mut decl = String::new();
    let mut fill = String::new();
    let mut pos = None;
    for line in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (lhs, sem) = line.trim_end_matches(';').split_once(':').ok_or("field without semantic")?;
        let sem = sem.trim();
        if RASTER_MADE.iter().any(|r| sem.starts_with(r)) {
            continue;
        }
        let words: Vec<&str> = lhs.split_whitespace().collect();
        let (name, ty) = (*words.last().ok_or("field")?, *words.get(words.len().wrapping_sub(2)).ok_or("field type")?);
        decl.push_str(&format!("    {line}\n"));
        if sem == "SV_Position" {
            pos = Some(name.to_string());
            continue;
        }
        let n: usize = ty.chars().last().and_then(|c| c.to_digit(10)).map_or(1, |d| d as usize);
        let base = ty.trim_end_matches(|c: char| c.is_ascii_digit());
        // each component's value is a function of its semantic and its place in the element, so a feeding vertex
        // shader built from another packing of the same semantics (the oracle's) gives the same inputs
        let key = sem_key(sem);
        let comps: Vec<String> = (0..n)
            .map(|j| {
                let h = (key * 7 + j * 3) as f32;
                if base.contains("int") {
                    format!("{}", (key * 5 + j + 1) % 13)
                } else {
                    let (a, sx, sy) = ((h * 0.137).fract(), (h * 0.291).fract() - 0.5, (h * 0.173).fract() - 0.5);
                    format!("{a:?}f + {sx:?}f * uv.x + {sy:?}f * uv.y")
                }
            })
            .collect();
        fill.push_str(&format!("    o.{name} = ({ty})({}{}{});\n", if n > 1 { format!("{base}{n}(") } else { String::new() }, comps.join(", "), if n > 1 { ")" } else { "" }));
    }
    let pos = match pos {
        Some(p) => p,
        None => {
            decl.push_str("    float4 sir_pos : SV_Position;\n");
            "sir_pos".to_string()
        }
    };
    Ok(format!(
        "struct In {{\n{decl}}};\nIn main(uint id : SV_VertexID) {{\n    In o = (In)0;\n    float2 uv = float2(id & 1, id >> 1);\n    float w = 1.0f + 0.75f * uv.x + 0.5f * uv.y;\n    o.{pos} = float4((uv.x * 2.0f - 1.0f) * w, (1.0f - uv.y * 2.0f) * w, (0.2f + 0.6f * uv.x) * w, w);\n{fill}    return o;\n}}\n"
    ))
}

/// A small number for a semantic (name and index): what a component's value is seeded from.
fn sem_key(sem: &str) -> usize {
    let u = sem.to_ascii_uppercase();
    let name = u.trim_end_matches(|c: char| c.is_ascii_digit());
    let index: usize = u[name.len()..].parse().unwrap_or(0);
    let h = name.bytes().fold(0usize, |h, b| (h * 31 + b as usize) % 1009);
    (h + index * 17) % 1009
}

/// The input struct of a pixel shader as the oracle's input signature packs it (element by element, in register
/// order), with the interpolation modes our struct gives the same semantics: a feeding vertex shader built from it
/// links to the oracle.
pub fn oracle_struct(isg1: &[Element], ours: &str) -> Result<String, String> {
    let start = ours.find("struct In {").ok_or("no input struct")?;
    let body = &ours[start + "struct In {".len()..];
    let body = &body[..body.find("};").ok_or("input struct not closed")?];
    let ours_mods = |sem: &str| -> String {
        for line in body.lines().map(str::trim) {
            if let Some((lhs, s)) = line.trim_end_matches(';').split_once(':') {
                if s.trim().eq_ignore_ascii_case(sem) {
                    let w: Vec<&str> = lhs.split_whitespace().collect();
                    return w[..w.len().saturating_sub(2)].iter().map(|m| format!("{m} ")).collect();
                }
            }
        }
        String::new()
    };
    let mut els: Vec<&Element> = isg1.iter().filter(|e| e.system == 0 || e.system == 1).collect();
    els.sort_by_key(|e| (e.register, e.mask.trailing_zeros()));
    let mut out = String::from("struct In {
");
    for (k, e) in els.iter().enumerate() {
        let n = e.mask.count_ones();
        let base = match e.comp {
            1 => "uint",
            2 => "int",
            4 => "uint16_t",
            5 => "int16_t",
            6 => "float16_t",
            _ => "float",
        };
        let ty = if n > 1 { format!("{base}{n}") } else { base.to_string() };
        let sem = if e.system == 1 { "SV_Position".to_string() } else { format!("{}{}", e.name, e.index) };
        let mods = if e.system == 1 { String::new() } else { ours_mods(&sem) };
        out.push_str(&format!("    {mods}{ty} o{k} : {sem};
"));
    }
    out.push_str("};
");
    Ok(out)
}

/// Compiles the feeding vertex shader (cached beside our program).
pub fn compile_vs(src: &str, out: &Path, hash: &str) -> Result<Vec<u8>, String> {
    let hl = out.join(format!("{hash}.feed.vs.hlsl"));
    let obj = out.join(format!("{hash}.feed.vs.dxil"));
    if std::fs::read_to_string(&hl).is_ok_and(|old| old == src) && obj.exists() {
        return std::fs::read(&obj).map_err(|e| e.to_string());
    }
    std::fs::write(&hl, src).map_err(|e| e.to_string())?;
    let dxc = super::dxc().ok_or("no dxc")?;
    rebuild_pipelines::translate::compile_file(&dxc, rebuild_pipelines::Stage::Vs, &hl, &obj)?;
    std::fs::read(&obj).map_err(|e| e.to_string())
}

/// What a pixel shader writes: render target n (format by component type) or depth.
#[derive(Clone, Copy)]
pub enum Target {
    Color(u32, DXGI_FORMAT),
    Depth,
}

pub fn targets_of(outputs: &[Element]) -> Vec<Target> {
    let mut t = Vec::new();
    for e in outputs {
        match e.system {
            64 if e.index < 8 => {
                let f = match e.comp {
                    1 | 4 => DXGI_FORMAT_R32G32B32A32_UINT,
                    2 | 5 => DXGI_FORMAT_R32G32B32A32_SINT,
                    _ => DXGI_FORMAT_R32G32B32A32_FLOAT,
                };
                t.push(Target::Color(e.index, f));
            }
            65 | 67 | 68 => t.push(Target::Depth),
            _ => {}
        }
    }
    t
}

pub fn pipeline(g: &Gpu, root: &ID3D12RootSignature, vs: &[u8], ps: &[u8], targets: &[Target]) -> Result<ID3D12PipelineState, String> {
    let mut rtv = [DXGI_FORMAT_UNKNOWN; 8];
    let mut n = 0;
    let mut depth = DXGI_FORMAT_UNKNOWN;
    for t in targets {
        match *t {
            Target::Color(i, f) => {
                rtv[i as usize] = f;
                n = n.max(i + 1);
            }
            Target::Depth => depth = DXGI_FORMAT_D32_FLOAT,
        }
    }
    let blend = D3D12_RENDER_TARGET_BLEND_DESC { RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8, ..Default::default() };
    let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: unsafe { std::mem::transmute_copy(root) },
        VS: D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr() as *const _, BytecodeLength: vs.len() },
        PS: D3D12_SHADER_BYTECODE { pShaderBytecode: ps.as_ptr() as *const _, BytecodeLength: ps.len() },
        BlendState: D3D12_BLEND_DESC { RenderTarget: [blend; 8], ..Default::default() },
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC { FillMode: D3D12_FILL_MODE_SOLID, CullMode: D3D12_CULL_MODE_NONE, DepthClipEnable: true.into(), ..Default::default() },
        DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
            DepthEnable: (depth != DXGI_FORMAT_UNKNOWN).into(),
            DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
            DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            ..Default::default()
        },
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: n,
        RTVFormats: rtv,
        DSVFormat: depth,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    g.graphics(&desc)
}

/// The targets with input set `set`'s starting contents.
pub fn make_targets(g: &Gpu, targets: &[Target], set: u32) -> Result<Vec<Res>, String> {
    targets
        .iter()
        .enumerate()
        .map(|(k, t)| {
            let n = (SIZE * SIZE) as u64;
            match *t {
                Target::Color(_, f) => {
                    let d = texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, SIZE as u64, SIZE, 1, 1, f, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET);
                    let init = super::run::bytes_of((0..n * 4).map(|i| super::run::word(set, 900 + k as u64, i)));
                    g.resource(d, D3D12_RESOURCE_STATE_RENDER_TARGET, true, init)
                }
                Target::Depth => {
                    let d = texture_desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, SIZE as u64, SIZE, 1, 1, DXGI_FORMAT_D32_FLOAT, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL);
                    let init = super::run::bytes_of((0..n).map(|i| super::run::texel(set, 950, i)));
                    g.resource(d, D3D12_RESOURCE_STATE_DEPTH_WRITE, true, init)
                }
            }
        })
        .collect()
}

/// Records the draw: targets bound, the quad drawn.
pub fn draw(g: &Gpu, targets: &[Target], res: &[Res]) {
    let mut rtvs = [D3D12_CPU_DESCRIPTOR_HANDLE::default(); 8];
    let mut n = 0u32;
    let mut dsv = None;
    unsafe {
        for (t, r) in targets.iter().zip(res) {
            match *t {
                Target::Color(i, _) => {
                    let h = g.rtv(i);
                    g.device.CreateRenderTargetView(&r.r, None, h);
                    rtvs[i as usize] = h;
                    n = n.max(i + 1);
                }
                Target::Depth => {
                    let h = g.dsv();
                    g.device.CreateDepthStencilView(&r.r, None, h);
                    dsv = Some(h);
                }
            }
        }
        // unbound target slots below the highest get a null view (never written: the format is UNKNOWN)
        for (i, h) in rtvs.iter_mut().enumerate().take(n as usize) {
            if h.ptr == 0 {
                *h = g.rtv(i as u32);
                let d = D3D12_RENDER_TARGET_VIEW_DESC { Format: DXGI_FORMAT_R8G8B8A8_UNORM, ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D, ..Default::default() };
                g.device.CreateRenderTargetView(None, Some(&d), *h);
            }
        }
        let l = &g.list;
        l.OMSetRenderTargets(n, Some(rtvs.as_ptr()), false, dsv.as_ref().map(|d| d as *const _));
        l.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: SIZE as f32, Height: SIZE as f32, MinDepth: 0.0, MaxDepth: 1.0 }]);
        l.RSSetScissorRects(&[windows::Win32::Foundation::RECT { left: 0, top: 0, right: SIZE as i32, bottom: SIZE as i32 }]);
        l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
        l.DrawInstanced(4, 1, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(name: &str, index: u32, system: u32, comp: u32, register: u32, mask: u8) -> Element {
        Element { stream: 0, name: name.into(), index, system, comp, register, mask, rw: mask, min_precision: 0 }
    }

    #[test]
    fn oracle_feed_gives_the_same_values_per_semantic() {
        let ours = "struct In {\n    float4 pos : SV_Position;\n    float3 r1_0 : TEXCOORD0;\n    nointerpolation uint r2_0 : TEXCOORD1;\n    bool ff : SV_IsFrontFace;\n};\n";
        // the oracle: TEXCOORD1 first, packed into the same register as TEXCOORD0's tail
        let isg1 = [el("SV_Position", 0, 1, 3, 0, 0xf), el("TEXCOORD", 1, 0, 1, 1, 0x1), el("TEXCOORD", 0, 0, 3, 1, 0xe), el("SV_IsFrontFace", 0, 9, 1, 2, 1)];
        let st = oracle_struct(&isg1, ours).unwrap();
        assert!(st.contains("nointerpolation uint o1 : TEXCOORD1;"), "{st}");
        assert!(st.contains("float3 o2 : TEXCOORD0;"), "{st}");
        assert!(!st.contains("SV_IsFrontFace"), "{st}");
        let (a, b) = (feed_vs(ours).unwrap(), feed_vs(&st).unwrap());
        let value = |src: &str, field: &str| src.lines().find(|l| l.contains(&format!("o.{field} ="))).map(|l| l.split_once('=').unwrap().1.to_string());
        assert_eq!(value(&a, "r1_0"), value(&b, "o2"));
        assert_eq!(value(&a, "r2_0"), value(&b, "o1"));
    }
}
