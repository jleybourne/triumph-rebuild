//! The host side: the root signature serializer (no device), and a device check that root signatures and
//! pipelines are accepted. The device is created only by `pipelines check`, only while the measurement lock is
//! absent.

use std::ffi::CString;
use std::mem::ManuallyDrop;

use rebuild_shader::signature::Element;
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::layout::{Param, RootDesc};

fn blob_bytes(b: &ID3DBlob) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() }
}

/// Serializes a root signature (version 1.1) with the host's serializer.
pub fn serialize(d: &RootDesc) -> Result<Vec<u8>, String> {
    let params: Vec<D3D12_ROOT_PARAMETER1> = d
        .params
        .iter()
        .map(|p| match *p {
            Param::Constants { register, space, count, vis } => D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: register, RegisterSpace: space, Num32BitValues: count },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY(vis as i32),
            },
            Param::Srv { register, space, vis } => D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 { ShaderRegister: register, RegisterSpace: space, Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY(vis as i32),
            },
        })
        .collect();
    let samplers: Vec<D3D12_STATIC_SAMPLER_DESC> = d
        .samplers
        .iter()
        .map(|s| {
            let x = &s.desc;
            D3D12_STATIC_SAMPLER_DESC {
                Filter: D3D12_FILTER(x.filter as i32),
                AddressU: D3D12_TEXTURE_ADDRESS_MODE(x.address[0] as i32),
                AddressV: D3D12_TEXTURE_ADDRESS_MODE(x.address[1] as i32),
                AddressW: D3D12_TEXTURE_ADDRESS_MODE(x.address[2] as i32),
                MipLODBias: x.mip_lod_bias,
                MaxAnisotropy: x.max_anisotropy,
                ComparisonFunc: D3D12_COMPARISON_FUNC(x.comparison as i32),
                BorderColor: D3D12_STATIC_BORDER_COLOR(x.border as i32),
                MinLOD: x.min_lod,
                MaxLOD: x.max_lod,
                ShaderRegister: s.register,
                RegisterSpace: s.space,
                ShaderVisibility: D3D12_SHADER_VISIBILITY(s.vis as i32),
            }
        })
        .collect();
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: D3D12_ROOT_SIGNATURE_FLAGS(d.flags as i32),
            },
        },
    };
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    unsafe {
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut errors)) {
            let msg = errors.map(|b| String::from_utf8_lossy(&blob_bytes(&b)).trim_end_matches('\0').trim().to_string());
            return Err(format!("{e} {}", msg.unwrap_or_default()));
        }
    }
    Ok(blob_bytes(&blob.ok_or("no blob")?))
}

/// What a serialized root signature says, read back through the host's deserializer: (flags, parameter
/// types and dword counts, static sampler registers/spaces/visibilities). For tests.
pub fn describe(blob: &[u8]) -> Result<(u32, Vec<(i32, u32)>, Vec<(u32, u32, i32)>), String> {
    unsafe {
        let mut de: Option<ID3D12VersionedRootSignatureDeserializer> = None;
        D3D12CreateVersionedRootSignatureDeserializer(blob.as_ptr() as *const _, blob.len(), &ID3D12VersionedRootSignatureDeserializer::IID, &mut de as *mut _ as *mut *mut _)
            .map_err(|e| e.to_string())?;
        let de = de.ok_or("no deserializer")?;
        let v = &*de.GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1).map_err(|e| e.to_string())?;
        let d = &v.Anonymous.Desc_1_1;
        let params = std::slice::from_raw_parts(d.pParameters, d.NumParameters as usize)
            .iter()
            .map(|p| {
                let n = if p.ParameterType == D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS { p.Anonymous.Constants.Num32BitValues } else { 2 };
                (p.ParameterType.0, n)
            })
            .collect();
        let samplers = std::slice::from_raw_parts(d.pStaticSamplers, d.NumStaticSamplers as usize)
            .iter()
            .map(|s| (s.ShaderRegister, s.RegisterSpace, s.ShaderVisibility.0))
            .collect();
        Ok((d.Flags.0 as u32, params, samplers))
    }
}

/// A device for the acceptance check.
pub struct Gpu {
    pub device: ID3D12Device,
    info: Option<ID3D12InfoQueue>,
    pub adapter: String,
}

/// The lanes' exclusive hold on the card (`agents\CARD`, created new; removed on drop, so also on a panic). Taken
/// only while `agents\MEASURING` is absent.
pub struct CardLock(std::path::PathBuf);

/// The agents folder of the lead's tree, where `MEASURING` and `CARD` live.
pub const AGENTS: &str = r"D:\Developer\triumph-rebuild\agents";

impl CardLock {
    /// The lock, or why not (the measurement lock is present, or another lane holds the card).
    pub fn take() -> Result<CardLock, String> {
        let dir = std::path::Path::new(AGENTS);
        if dir.join("MEASURING").exists() {
            return Err("the measurement lock is present".into());
        }
        let p = dir.join("CARD");
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "rebuild-pipelines pid {}", std::process::id());
                Ok(CardLock(p))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err("another lane holds the card (agents\\CARD)".into()),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }
}

impl Drop for CardLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Gpu {
    /// `debug`: the D3D12 debug layer, whose messages give refusal reasons (slower; not for timing).
    pub fn new(debug: bool) -> Result<Gpu, String> {
        Self::create(debug, false)
    }

    /// WARP, the software rasterizer: correctness checks without the card.
    pub fn warp() -> Result<Gpu, String> {
        Self::create(false, true)
    }

    fn create(debug: bool, warp: bool) -> Result<Gpu, String> {
        unsafe {
            if debug {
                let mut d: Option<ID3D12Debug> = None;
                D3D12GetDebugInterface(&mut d).map_err(|e| format!("debug layer: {e}"))?;
                d.ok_or("debug layer")?.EnableDebugLayer();
            }
            let mut device: Option<ID3D12Device> = None;
            if warp {
                let f: windows::Win32::Graphics::Dxgi::IDXGIFactory4 = windows::Win32::Graphics::Dxgi::CreateDXGIFactory1().map_err(|e| format!("DXGI factory: {e}"))?;
                let a: windows::Win32::Graphics::Dxgi::IDXGIAdapter = f.EnumWarpAdapter().map_err(|e| format!("WARP adapter: {e}"))?;
                D3D12CreateDevice(&a, D3D_FEATURE_LEVEL_12_0, &mut device).map_err(|e| format!("WARP device: {e}"))?;
            } else {
                D3D12CreateDevice(None::<&windows::core::IUnknown>, D3D_FEATURE_LEVEL_12_0, &mut device).map_err(|e| format!("device: {e}"))?;
            }
            let device = device.ok_or("no device")?;
            let mut sm = D3D12_FEATURE_DATA_SHADER_MODEL { HighestShaderModel: D3D_SHADER_MODEL_6_6 };
            device
                .CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, &mut sm as *mut _ as *mut _, std::mem::size_of_val(&sm) as u32)
                .map_err(|e| format!("shader model query: {e}"))?;
            if sm.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0 {
                return Err(format!("device offers shader model {:#x}, below 6.6", sm.HighestShaderModel.0));
            }
            let info = if debug { device.cast::<ID3D12InfoQueue>().ok() } else { None };
            if let Some(i) = &info {
                let _ = i.SetMuteDebugOutput(true);
            }
            let luid = device.GetAdapterLuid();
            Ok(Gpu { device, info, adapter: format!("luid {:08x}:{:08x}", luid.HighPart, luid.LowPart) })
        }
    }

    /// The debug layer's messages since the last call (errors and warnings).
    pub fn messages(&self) -> Vec<String> {
        let Some(q) = &self.info else { return Vec::new() };
        let mut out = Vec::new();
        unsafe {
            let n = q.GetNumStoredMessages();
            for i in 0..n {
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
        out
    }

    fn reason(&self, e: windows::core::Error) -> String {
        let m = self.messages().join(" | ");
        if m.is_empty() {
            format!("{}", e.code())
        } else {
            m
        }
    }

    pub fn root_signature(&self, blob: &[u8]) -> Result<ID3D12RootSignature, String> {
        unsafe { self.device.CreateRootSignature(0, blob).map_err(|e| self.reason(e)) }
    }

    pub fn compute(&self, root: &ID3D12RootSignature, cs: &[u8]) -> Result<ID3D12PipelineState, String> {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: cs.as_ptr() as *const _, BytecodeLength: cs.len() },
            ..Default::default()
        };
        let r = unsafe { self.device.CreateComputePipelineState(&desc) };
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        r.map_err(|e| self.reason(e))
    }

    /// A graphics pipeline with a default state: an input layout from the vertex shader's inputs (one slot,
    /// packed), render target formats from the pixel shader's outputs, D32 depth if it writes depth, triangles.
    pub fn graphics(&self, root: &ID3D12RootSignature, vs: &[u8], vs_in: &[Element], ps: &[u8], ps_out: &[Element]) -> Result<ID3D12PipelineState, String> {
        let names: Vec<CString> = vs_in.iter().map(|e| CString::new(e.name.as_str()).unwrap_or_default()).collect();
        let input: Vec<D3D12_INPUT_ELEMENT_DESC> = vs_in
            .iter()
            .zip(&names)
            .filter(|(e, _)| e.system == 0)
            .map(|(e, n)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(n.as_ptr() as *const u8),
                SemanticIndex: e.index,
                Format: format(e.comp, 8 - e.mask.leading_zeros()),
                InputSlot: 0,
                AlignedByteOffset: D3D12_APPEND_ALIGNED_ELEMENT,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            })
            .collect();
        let mut rtv = [DXGI_FORMAT_UNKNOWN; 8];
        let mut targets = 0;
        let mut depth = DXGI_FORMAT_UNKNOWN;
        for e in ps_out {
            match e.system {
                64 if e.index < 8 => {
                    rtv[e.index as usize] = target_format(e.comp, 8 - e.mask.leading_zeros());
                    targets = targets.max(e.index + 1);
                }
                65 | 67 | 68 => depth = DXGI_FORMAT_D32_FLOAT,
                _ => {}
            }
        }
        let blend = D3D12_RENDER_TARGET_BLEND_DESC { RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8, ..Default::default() };
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
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
            InputLayout: D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: input.as_ptr(), NumElements: input.len() as u32 },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: targets,
            RTVFormats: rtv,
            DSVFormat: depth,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let r = unsafe { self.device.CreateGraphicsPipelineState(&desc) };
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        r.map_err(|e| self.reason(e))
    }
}

/// A render-target format with as many channels as the output writes (three float channels: R11G11B10).
pub fn target_format(comp: u32, n: u32) -> DXGI_FORMAT {
    match (comp, n) {
        (3, 3) => DXGI_FORMAT_R11G11B10_FLOAT,
        (6, 3) => DXGI_FORMAT_R11G11B10_FLOAT,
        (_, 3) => format(comp, 4),
        _ => format(comp, n),
    }
}

/// A vertex format for a signature component type and component count.
pub fn format(comp: u32, n: u32) -> DXGI_FORMAT {
    let n = n.clamp(1, 4) as usize;
    let pick = |f: [DXGI_FORMAT; 4]| f[n - 1];
    let pick16 = |f: [DXGI_FORMAT; 3]| f[if n == 1 { 0 } else if n == 2 { 1 } else { 2 }];
    match comp {
        1 => pick([DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R32G32_UINT, DXGI_FORMAT_R32G32B32_UINT, DXGI_FORMAT_R32G32B32A32_UINT]),
        2 => pick([DXGI_FORMAT_R32_SINT, DXGI_FORMAT_R32G32_SINT, DXGI_FORMAT_R32G32B32_SINT, DXGI_FORMAT_R32G32B32A32_SINT]),
        4 => pick16([DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R16G16_UINT, DXGI_FORMAT_R16G16B16A16_UINT]),
        5 => pick16([DXGI_FORMAT_R16_SINT, DXGI_FORMAT_R16G16_SINT, DXGI_FORMAT_R16G16B16A16_SINT]),
        6 => pick16([DXGI_FORMAT_R16_FLOAT, DXGI_FORMAT_R16G16_FLOAT, DXGI_FORMAT_R16G16B16A16_FLOAT]),
        _ => pick([DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32G32B32_FLOAT, DXGI_FORMAT_R32G32B32A32_FLOAT]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{flags, StageLayout, USER_DATA_DWORDS};

    #[test]
    fn host_serializer_keeps_the_layout() {
        let words = [0, 15 << 20, 1 << 20 | 1 << 22 | 2 << 26, 0];
        let vs = StageLayout { user_data: true, shadow: true, samplers: vec![words], compare: 0 };
        let ps = StageLayout { user_data: true, shadow: false, samplers: vec![words, words], compare: 0 };
        let blob = serialize(&RootDesc::graphics(&vs, &ps, USER_DATA_DWORDS)).unwrap();
        let (f, params, samplers) = describe(&blob).unwrap();
        assert_eq!(f, flags::GRAPHICS);
        assert_eq!(f & flags::SAMPLER_HEAP_DIRECTLY_INDEXED, flags::SAMPLER_HEAP_DIRECTLY_INDEXED, "the fork's 311 refusals");
        let c = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS.0;
        assert_eq!(params, [(c, 16), (c, 16), (D3D12_ROOT_PARAMETER_TYPE_SRV.0, 2)]);
        assert_eq!(samplers, [(0, 100, 1), (0, 100, 5), (1, 100, 5)]);

        let blob = serialize(&RootDesc::compute(&StageLayout::default(), USER_DATA_DWORDS)).unwrap();
        let (f, params, samplers) = describe(&blob).unwrap();
        assert_eq!((f, params.len(), samplers.len()), (flags::HEAP, 2, 0));
    }

    #[test]
    fn host_serializer_refusal_carries_its_reason() {
        let mut d = RootDesc::compute(&StageLayout::default(), 16);
        d.params.push(d.params[0]); // b0 twice, same visibility
        let e = serialize(&d).unwrap_err();
        assert!(!e.is_empty());
    }

    #[test]
    fn formats() {
        assert_eq!(format(3, 3), DXGI_FORMAT_R32G32B32_FLOAT);
        assert_eq!(format(6, 3), DXGI_FORMAT_R16G16B16A16_FLOAT);
        assert_eq!(format(1, 1), DXGI_FORMAT_R32_UINT);
    }
}
