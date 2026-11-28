//! The measurement behind [`crate::fullscreen`]'s decision: present latency (the present call to the vblank that
//! showed it, from DXGI's frame statistics), the CPU time inside `Present`, and tearing support, through the
//! presenter's own swap chain ([`crate::present`]'s `Swap`), per window mode and vsync on/off.
//!
//! By hand, one GPU run at a time: on the card, holding `agents\CARD`, with `agents\MEASURING` made create-new
//! for the run (other lanes stay quiet) and removed at the end; about 10 s a mode.
//!
//! ```text
//! cargo test -p rebuild-runtime --release -- --ignored --nocapture --test-threads=1 fullscreen_probe
//! ```
//!
//! By default only the **windowed** case runs (a 640x360 window that never takes the focus). The cases that cover
//! the screen, **borderless** and **exclusive**, run only with `REBUILD_FULLSCREEN_PROBE=all`: for the owner, at the
//! desk, never from an automated run.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::present::{Swap, WindowMode};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
}

fn qpc() -> i64 {
    let mut c = 0;
    unsafe { QueryPerformanceCounter(&mut c) };
    c
}

/// `agents\MEASURING` for the run, removed on drop (made create-new: an existing one is not ours to remove).
struct Measuring(std::path::PathBuf);
impl Drop for Measuring {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn transition(r: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: borrowed for the call; ManuallyDrop never releases it
                pResource: unsafe { std::mem::transmute_copy(r) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

#[test]
#[ignore = "GPU and the screen: by hand with --test-threads=1, while agents\\MEASURING is absent"]
fn fullscreen_probe_present_latency_per_window_mode() {
    // what DXGI says without a device: tearing support and the outputs' modes
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).expect("factory");
        let mut allow = windows::Win32::Foundation::BOOL(0);
        let ok = factory.CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING, &mut allow as *mut _ as *mut _, 4).is_ok();
        println!("probe     tearing support (DXGI_FEATURE_PRESENT_ALLOW_TEARING): {}", ok && allow.as_bool());
        for a in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(a) else { break };
            let name = adapter.GetDesc1().map(|d| String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string()).unwrap_or_default();
            for o in 0.. {
                let Ok(output) = adapter.EnumOutputs(o) else { break };
                let d = output.GetDesc().unwrap_or_default();
                let r = d.DesktopCoordinates;
                let mut n = 0u32;
                let _ = output.GetDisplayModeList(DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_ENUM_MODES(0), &mut n, None);
                println!("probe     adapter {a} ({name}) output {o}: desktop {}x{} at ({}, {}), {n} RGBA8 modes", r.right - r.left, r.bottom - r.top, r.left, r.top);
            }
        }
    }
    let lead = std::path::Path::new(rebuild_pipelines::d3d::AGENTS);
    if lead.join("MEASURING").exists() {
        println!("skipped: agents\\MEASURING is present (the lead is timing)");
        return;
    }
    let _card = match rebuild_pipelines::d3d::CardLock::take() {
        Ok(c) => c,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    let measuring = lead.join("MEASURING");
    let _measuring = match std::fs::OpenOptions::new().write(true).create_new(true).open(&measuring) {
        Ok(_) => Measuring(measuring),
        Err(e) => {
            println!("skipped: agents\\MEASURING: {e}");
            return;
        }
    };
    let mut device: Option<ID3D12Device> = None;
    if unsafe { D3D12CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device) }.is_err() {
        println!("skipped: no D3D12 device");
        return;
    }
    let device = device.unwrap();
    let mut freq = 0i64;
    unsafe { QueryPerformanceFrequency(&mut freq) };
    let all = std::env::var("REBUILD_FULLSCREEN_PROBE").is_ok_and(|v| v.trim() == "all");
    let host = crate::dxgi::host_mode();
    let mut modes = vec![(WindowMode::Windowed(640, 360), (640u32, 360u32))];
    if all {
        modes.push((WindowMode::Borderless, (host.width, host.height)));
        modes.push((WindowMode::Fullscreen, (host.width, host.height)));
    } else {
        println!("probe     windowed only (REBUILD_FULLSCREEN_PROBE=all adds borderless and exclusive: they cover the screen)");
    }
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
        list.Close().unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        let mut v = 0u64;
        let started = Instant::now();
        for (mode, image) in modes {
            let mut s = match Swap::new(&device, &queue, mode, image, DXGI_FORMAT_R8G8B8A8_UNORM, false) {
                Ok(s) => s,
                Err(e) => {
                    println!("probe     {mode:?}: {e}");
                    continue;
                }
            };
            for vsync in [true, false] {
                let mut sent: HashMap<u32, i64> = HashMap::new();
                let mut latency = Vec::new();
                let mut cpu = Vec::new();
                let mut seen = 0u32;
                let mut occluded = 0;
                let t0 = Instant::now();
                let mut frames = 0;
                while t0.elapsed() < Duration::from_secs(4) && started.elapsed() < Duration::from_secs(100) {
                    if s.pump() {
                        sent.clear();
                    }
                    let _ = WaitForSingleObject(s.waitable, 100);
                    let back = s.swap.GetCurrentBackBufferIndex() as usize;
                    let Some(target) = s.buffers.get(back).cloned() else { break };
                    allocator.Reset().unwrap();
                    list.Reset(&allocator, None).unwrap();
                    list.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
                    let c = if frames % 2 == 0 { [0.1, 0.1, 0.1, 1.0] } else { [0.2, 0.2, 0.2, 1.0] };
                    list.ClearRenderTargetView(s.rtv(back), &c, None);
                    list.ResourceBarrier(&[transition(&target, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT)]);
                    list.Close().unwrap();
                    queue.ExecuteCommandLists(&[Some(windows::core::Interface::cast::<ID3D12CommandList>(&list).unwrap())]);
                    let at = qpc();
                    let call = Instant::now();
                    let hr = s.present(vsync);
                    cpu.push(call.elapsed().as_secs_f64() * 1e6);
                    if hr.0 == 0x087a_0001 {
                        occluded += 1;
                    }
                    if let Ok(id) = s.swap.GetLastPresentCount() {
                        sent.insert(id, at);
                    }
                    let mut st = DXGI_FRAME_STATISTICS::default();
                    if s.swap.GetFrameStatistics(&mut st).is_ok() && st.PresentCount != seen {
                        seen = st.PresentCount;
                        if let Some(t) = sent.get(&st.PresentCount) {
                            latency.push((st.SyncQPCTime - t) as f64 * 1e3 / freq as f64);
                        }
                    }
                    v += 1;
                    queue.Signal(&fence, v).unwrap();
                    fence.SetEventOnCompletion(v, event).unwrap();
                    assert_eq!(WaitForSingleObject(event, 5000), WAIT_OBJECT_0, "the GPU finished within 5 s");
                    frames += 1;
                }
                let fps = frames as f64 / t0.elapsed().as_secs_f64();
                // composed (the DWM in the way), frame statistics name a vblank from before the present they count
                // when presents outrun the display: such a sample says nothing, and a median of them is "n/a"
                let before = latency.len();
                latency.retain(|&l| l >= 0.0);
                let n = latency.len();
                let ms = |v: f64| if v.is_nan() { "n/a".to_string() } else { format!("{v:.2} ms") };
                println!(
                    "probe     {:<14} vsync {:<3}: {frames} frames ({fps:.0} fps), present to scan-out median {}, p95 {} ({n} of {before} statistics usable), Present CPU median {:.0} µs, {occluded} occluded, full screen {:?}",
                    format!("{mode:?}"),
                    if vsync { "on" } else { "off" },
                    ms(percentile(&mut latency, 0.5)),
                    ms(percentile(&mut latency, 0.95)),
                    percentile(&mut cpu, 0.5),
                    s.state()
                );
            }
            v += 1;
            queue.Signal(&fence, v).unwrap();
            fence.SetEventOnCompletion(v, event).unwrap();
            let _ = WaitForSingleObject(event, 5000);
            drop(s);
        }
        let _ = windows::Win32::Foundation::CloseHandle(event);
    }
}
