//! The Windows side: a buffer the GPU writes and the CPU reads, with no copy.
//!
//! A committed buffer in a custom heap (`WRITE_BACK`, L0) is addressable by
//! both: the GPU writes it as an unordered-access resource, and the CPU reads
//! the same bytes through a persistent map. Nothing is staged and nothing is
//! read back through a second resource.

#![cfg(windows)]

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const LEN: usize = 64 << 10;
const WANT: u32 = 0xBABE_FACA;

pub struct Report {
    pub backend: String,
    pub read: Vec<u8>,
    pub want: u32,
    pub zero_copy: bool,
}

fn desc_buffer(bytes: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    }
}

fn blob_bytes(blob: &ID3DBlob) -> Vec<u8> {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec()
    }
}

fn compile_shader() -> Result<Vec<u8>, String> {
    let hlsl = br#"
RWByteAddressBuffer out_buffer : register(u0);
[numthreads(1, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    out_buffer.Store(0, 0xBABEFACAu);
    out_buffer.Store(8, 42u);
}
"#;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    unsafe {
        D3DCompile(
            hlsl.as_ptr().cast(),
            hlsl.len(),
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
    }
    .map_err(|e| match &errors {
        Some(b) => format!("compile: {e}: {}", String::from_utf8_lossy(&blob_bytes(b))),
        None => format!("compile: {e}"),
    })?;
    Ok(blob_bytes(&code.ok_or("compile: no code")?))
}

fn root_signature(device: &ID3D12Device) -> Result<ID3D12RootSignature, String> {
    unsafe {
        let range = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        };
        let parameter = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: &range,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: 1,
            pParameters: &parameter,
            NumStaticSamplers: 0,
            pStaticSamplers: std::ptr::null(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
        };
        let mut blob: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, Some(&mut errors))
            .map_err(|e| match &errors {
                Some(b) => format!("signature: {e}: {}", String::from_utf8_lossy(&blob_bytes(b))),
                None => format!("signature: {e}"),
            })?;
        let blob = blob.ok_or("signature: no blob")?;
        device
            .CreateRootSignature::<ID3D12RootSignature>(0, &blob_bytes(&blob))
            .map_err(|e| format!("signature: {e}"))
    }
}

fn pipeline(
    device: &ID3D12Device,
    signature: &ID3D12RootSignature,
    code: &[u8],
) -> Result<ID3D12PipelineState, String> {
    unsafe {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(signature),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            NodeMask: 0,
            ..Default::default()
        };
        device
            .CreateComputePipelineState::<ID3D12PipelineState>(&desc)
            .map_err(|e| format!("pipeline: {e}"))
    }
}

pub fn prove_shared_memory() -> Result<Report, String> {
    prove(false)
}

pub fn prove_on_vram() -> Result<Report, String> {
    prove(true)
}

fn prove(vram: bool) -> Result<Report, String> {
    unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(
            None::<&windows::core::IUnknown>,
            D3D_FEATURE_LEVEL_11_0,
            &mut device,
        )
        .map_err(|e| format!("device: {e}"))?;
        let device = device.ok_or("device: none")?;

        let queue = device
            .CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
                ..Default::default()
            })
            .map_err(|e| format!("queue: {e}"))?;

        let heap = if vram {
            D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            }
        } else {
            D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                CreationNodeMask: 1,
                VisibleNodeMask: 1,
            }
        };

        let mut resource: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc_buffer(LEN as u64),
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
            .map_err(|e| format!("buffer: {e}"))?;
        let resource = resource.ok_or("buffer: none")?;

        let mut mapped: *mut std::ffi::c_void = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: 0 };
        if !vram {
            resource
                .Map(0, Some(&range), Some(&mut mapped))
                .map_err(|e| format!("map: {e}"))?;
        }
        let cpu = mapped as *const u8;

        let code = compile_shader()?;
        let signature = root_signature(&device)?;
        let pso = pipeline(&device, &signature, &code)?;

        let allocator = device
            .CreateCommandAllocator::<ID3D12CommandAllocator>(D3D12_COMMAND_LIST_TYPE_COMPUTE)
            .map_err(|e| format!("allocator: {e}"))?;
        let list = device
            .CreateCommandList::<_, _, ID3D12GraphicsCommandList>(
                0,
                D3D12_COMMAND_LIST_TYPE_COMPUTE,
                &allocator,
                None,
            )
            .map_err(|e| format!("list: {e}"))?;

        let uav_heap = device
            .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .map_err(|e| format!("heap: {e}"))?;

        let mut uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        };
        uav.Anonymous.Buffer = D3D12_BUFFER_UAV {
            FirstElement: 0,
            NumElements: (LEN / 4) as u32,
            StructureByteStride: 0,
            CounterOffsetInBytes: 0,
            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
        };
        device.CreateUnorderedAccessView(
            &resource,
            None,
            Some(&uav),
            uav_heap.GetCPUDescriptorHandleForHeapStart(),
        );

        list.SetDescriptorHeaps(&[Some(uav_heap.clone())]);
        list.SetComputeRootSignature(&signature);
        list.SetPipelineState(&pso);
        list.SetComputeRootDescriptorTable(0, uav_heap.GetGPUDescriptorHandleForHeapStart());
        list.Dispatch(1, 1, 1);
        list.Close().map_err(|e| format!("close: {e}"))?;

        let command_list: ID3D12CommandList = list.cast().map_err(|e| format!("cast: {e}"))?;
        queue.ExecuteCommandLists(&[Some(command_list)]);

        let fence = device
            .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
            .map_err(|e| format!("fence: {e}"))?;
        queue.Signal(&fence, 1).map_err(|e| format!("signal: {e}"))?;
        while fence.GetCompletedValue() < 1 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let read = if vram {
            Vec::new()
        } else {
            std::slice::from_raw_parts(cpu, 16).to_vec()
        };
        let zero_copy = !vram && read[0..4] == WANT.to_le_bytes() && read[8] == 42;

        Ok(Report {
            backend: if vram {
                "D3D12 default heap (video memory)".to_string()
            } else {
                "D3D12 custom heap (WRITE_BACK, L0), persistently mapped".to_string()
            },
            read,
            want: WANT,
            zero_copy,
        })
    }
}

/// The smallest possible dispatch: a compute shader with no bindings at all.
/// If this also removes the device, nothing about our buffers is at fault.
pub fn prove_bare_dispatch() -> Result<String, String> {
    unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(
            None::<&windows::core::IUnknown>,
            D3D_FEATURE_LEVEL_11_0,
            &mut device,
        )
        .map_err(|e| format!("device: {e}"))?;
        let device = device.ok_or("device: none")?;
        let queue = device
            .CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
                ..Default::default()
            })
            .map_err(|e| format!("queue: {e}"))?;

        let hlsl = br#"
[numthreads(1, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) { }
"#;
        let mut code: Option<ID3DBlob> = None;
        D3DCompile(
            hlsl.as_ptr().cast(),
            hlsl.len(),
            PCSTR::null(),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_0"),
            0,
            0,
            &mut code,
            None,
        )
        .map_err(|e| format!("compile: {e}"))?;
        let bytes = blob_bytes(&code.ok_or("compile: no code")?);

        let mut blob: Option<ID3DBlob> = None;
        let desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: 0,
            pParameters: std::ptr::null(),
            NumStaticSamplers: 0,
            pStaticSamplers: std::ptr::null(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
        };
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)
            .map_err(|e| format!("signature: {e}"))?;
        let signature = device
            .CreateRootSignature::<ID3D12RootSignature>(0, &blob_bytes(&blob.ok_or("signature: no blob")?))
            .map_err(|e| format!("signature: {e}"))?;

        let pso = device
            .CreateComputePipelineState::<ID3D12PipelineState>(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&signature),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                NodeMask: 0,
                ..Default::default()
            })
            .map_err(|e| format!("pipeline: {e}"))?;

        let allocator = device
            .CreateCommandAllocator::<ID3D12CommandAllocator>(D3D12_COMMAND_LIST_TYPE_COMPUTE)
            .map_err(|e| format!("allocator: {e}"))?;
        let list = device
            .CreateCommandList::<_, _, ID3D12GraphicsCommandList>(
                0,
                D3D12_COMMAND_LIST_TYPE_COMPUTE,
                &allocator,
                None,
            )
            .map_err(|e| format!("list: {e}"))?;
        list.SetComputeRootSignature(&signature);
        list.SetPipelineState(&pso);
        list.Dispatch(1, 1, 1);
        list.Close().map_err(|e| format!("close: {e}"))?;
        let command_list: ID3D12CommandList = list.cast().map_err(|e| format!("cast: {e}"))?;
        queue.ExecuteCommandLists(&[Some(command_list)]);

        let fence = device
            .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
            .map_err(|e| format!("fence: {e}"))?;
        queue.Signal(&fence, 1).map_err(|e| format!("signal: {e}"))?;
        for _ in 0..5000 {
            if fence.GetCompletedValue() >= 1 {
                return Ok("a bare dispatch completes: the device is healthy".into());
            }
            if device.GetDeviceRemovedReason().is_err() {
                return Err(format!(
                    "device removed during a bare dispatch: {:?}",
                    device.GetDeviceRemovedReason().err()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err("a bare dispatch never completed".into())
    }
}
