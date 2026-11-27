//! The overlay pass under the D3D12 debug layer with GPU-based validation, on WARP, in a process of its own (the
//! debug layer must be on before the first device is made).
//!
//! It guards the bug this caught: the pixel shader's glyph branch was flattened, so solid quads loaded the atlas at
//! wrapped coordinates through a root SRV (no bounds check): WARP removed the device, a card would page-fault.

#![cfg(windows)]

use std::sync::Arc;

use rebuild_config::{Config, Value};
use rebuild_overlay::render::Renderer;
use rebuild_overlay::{layout, theme, Backend, Font, Menu, Nav, Registry, Stats};
use windows::core::Interface;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, IDXGIAdapter, IDXGIFactory4, DXGI_CREATE_FACTORY_FLAGS};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

struct Host;
impl Backend for Host {
    fn applies(&self, key: &str) -> bool {
        key.starts_with("display.") || key == "diagnostics.overlay"
    }
    fn current(&self, _: &str) -> Option<Value> {
        None
    }
    fn apply(&mut self, _: &str, _: &Config) {}
    fn stats(&self) -> Stats {
        Stats { seq: 1, fps: Some(30.0), ms: Some(33.3), draws: Some(4512), ..Stats::default() }
    }
}

fn messages(device: &ID3D12Device) -> Vec<String> {
    let Ok(q) = device.cast::<ID3D12InfoQueue>() else { return Vec::new() };
    let mut out = Vec::new();
    unsafe {
        for i in 0..q.GetNumStoredMessages() {
            let mut len = 0usize;
            let _ = q.GetMessage(i, None, &mut len);
            let mut buf = vec![0u64; len.div_ceil(8)];
            let m = buf.as_mut_ptr() as *mut D3D12_MESSAGE;
            if q.GetMessage(i, Some(m), &mut len).is_ok() {
                out.push(String::from_utf8_lossy(std::ffi::CStr::from_ptr((*m).pDescription as *const _).to_bytes()).into_owned());
            }
        }
    }
    out
}

#[test]
fn warp_gpu_validation_finds_nothing_in_the_overlay_pass() {
    unsafe {
        let mut debug: Option<ID3D12Debug> = None;
        D3D12GetDebugInterface(&mut debug).expect("debug layer (Graphics Tools)");
        let debug = debug.unwrap();
        debug.EnableDebugLayer();
        debug.cast::<ID3D12Debug1>().expect("ID3D12Debug1").SetEnableGPUBasedValidation(true);
        let f: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter = f.EnumWarpAdapter().unwrap();
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device).unwrap();
        let device = device.unwrap();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        let value = std::cell::Cell::new(0u64);

        let (w, h) = (1280u32, 720u32);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(w),
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let clear = D3D12_CLEAR_VALUE { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Anonymous: D3D12_CLEAR_VALUE_0 { Color: [0.2, 0.4, 0.6, 1.0] } };
        let mut target: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_RENDER_TARGET, Some(&clear), &mut target)
            .unwrap();
        let target = target.unwrap();
        let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() }).unwrap();
        let rtv = heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&target, None, rtv);
        let a: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let l: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &a, None).unwrap();
        l.ClearRenderTargetView(rtv, &[0.2, 0.4, 0.6, 1.0], None);
        l.Close().unwrap();
        let submit = |list: &ID3D12GraphicsCommandList| {
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            value.set(value.get() + 1);
            queue.Signal(&fence, value.get()).unwrap();
            fence.SetEventOnCompletion(value.get(), event).unwrap();
            WaitForSingleObject(event, INFINITE);
        };
        submit(&l);

        let mut r = Renderer::new(&device, DXGI_FORMAT_R8G8B8A8_UNORM, 3, 0).expect("renderer");
        let mut b = Host;
        let mut m = Menu::new(Config::default(), &b, Registry::default().with_placeholders());
        let font = Arc::new(Font::bake(theme::FONT_FACE, theme::FONT_WEIGHT, theme::font_px(h)).expect("font"));
        let mut frame = 0;
        for (open, hud) in [(true, false), (true, false), (true, false), (false, true)] {
            let scene = layout::build(&m, &b.stats(), open, hud, (w, h), font.clone(), "F10");
            let list = r.record(frame % 3, &scene, &target, D3D12_RESOURCE_STATE_RENDER_TARGET, rtv, fence.GetCompletedValue(), value.get() + 1).expect("recorded");
            list.Close().unwrap();
            submit(&list);
            assert!(device.GetDeviceRemovedReason().is_ok(), "device removed: {:#?}", messages(&device));
            m.nav(Nav::NextPage, &mut b);
            frame += 1;
        }
        let _ = CloseHandle(event);
        let bad: Vec<String> = messages(&device).into_iter().filter(|m| m.contains("VALIDATION") || m.contains("removal") || m.contains("ERROR")).collect();
        assert!(bad.is_empty(), "{bad:#?}");
    }
}
