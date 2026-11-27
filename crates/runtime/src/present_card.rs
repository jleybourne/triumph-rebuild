//! On the card: console lists recorded through the real path (the device's slots, crates/d3d12x resources and
//! views, crates/xlist lists), submitted through queue 0x50, shown through 0x100, captured to PNG and read back.
//! By hand, one GPU test at a time, while `agents\MEASURING` is absent (checked again right before the device is
//! made) and holding the card's lock `agents\CARD` (created create-new, removed at the end, pass or fail);
//! skipped with no D3D12 device. The window is an off-screen tool window ([`crate::present::hide_window`]).
//! Every fence wait is bounded (5 s). Pass or fail, the guard releases the bridge's waits, waits for the queue (at
//! most 5 s), releases the swap chain and destroys the window, so a failure leaves no queue waiting on the GPU.
//!
//! ```text
//! cargo test -p rebuild-runtime --release -- --ignored --test-threads=1 --nocapture present_card
//! ```

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::present::Plane;

type Raw = *mut c_void;

fn measuring() -> bool {
    let lead = std::path::Path::new(r"D:\Developer\triumph-rebuild\agents\MEASURING");
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/MEASURING");
    let m = lead.exists() || here.exists();
    if m {
        println!("skipped: agents\\MEASURING is present (the lead is timing)");
    }
    m
}

fn method<F: Copy>(obj: Raw, offset: usize) -> F {
    assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
    unsafe {
        let table = *obj.cast::<*const *const ()>();
        std::mem::transmute_copy(&*table.add(offset / 8))
    }
}

const IID: [u8; 16] = [0x44, 0xc3, 0x05, 0x54, 0x57, 0xd4, 0x4e, 0x44, 0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39];

#[repr(C)]
struct Barrier {
    kind: u32,
    flags: u32,
    a: Raw,
    b: u64,
    c: u64,
}

fn barrier(r: Raw, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> Barrier {
    Barrier { kind: 0, flags: 0, a: r, b: 0xffff_ffff | ((before.0 as u32 as u64) << 32), c: after.0 as u32 as u64 }
}

/// The title's view of one device.
struct Title {
    dev: Raw,
}

impl Title {
    fn queue(&self) -> Raw {
        let desc = D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() };
        let mut q: Raw = std::ptr::null_mut();
        let hr = unsafe { method::<unsafe extern "system" fn(Raw, *const D3D12_COMMAND_QUEUE_DESC, *const [u8; 16], *mut Raw) -> i32>(self.dev, 0x40)(self.dev, &desc, &IID, &mut q) };
        assert_eq!(hr, 0);
        q
    }

    fn fence(&self) -> Raw {
        let mut f: Raw = std::ptr::null_mut();
        assert_eq!(unsafe { method::<unsafe extern "system" fn(Raw, u64, u32, *const [u8; 16], *mut Raw) -> i32>(self.dev, 0x120)(self.dev, 0, 0, &IID, &mut f) }, 0);
        f
    }

    fn committed(&self, desc: &D3D12_RESOURCE_DESC, heap: u32) -> Raw {
        // HeapProperties as the console passes them: type, cpu page property, memory pool, creation and visible node masks
        let props: [u32; 5] = [heap, 0, 0, 1, 1];
        let mut r: Raw = std::ptr::null_mut();
        type F = unsafe extern "system" fn(Raw, *const [u32; 5], u32, *const D3D12_RESOURCE_DESC, u32, *const c_void, *const [u8; 16], *mut Raw) -> i32;
        let hr = unsafe { method::<F>(self.dev, 0xd8)(self.dev, &props, 0, desc, 0, std::ptr::null(), std::ptr::null(), &mut r) };
        assert_eq!(hr, 0, "CreateCommittedResource {hr:#x}");
        r
    }

    fn texture(&self, w: u64, h: u32, rt: bool) -> Raw {
        let d = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: if rt { D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET } else { D3D12_RESOURCE_FLAG_NONE },
            ..Default::default()
        };
        self.committed(&d, 1)
    }

    /// An RTV for `r` in a new console RTV heap: its console CPU handle.
    fn rtv(&self, r: Raw) -> usize {
        let desc: [u32; 4] = [2, 4, 0, 0];
        let mut h: Raw = std::ptr::null_mut();
        assert_eq!(unsafe { method::<unsafe extern "system" fn(Raw, *const [u32; 4], *const [u8; 16], *mut Raw) -> i32>(self.dev, 0x70)(self.dev, &desc, std::ptr::null(), &mut h) }, 0);
        let mut start = 0u64;
        unsafe { method::<unsafe extern "system" fn(Raw, *mut u64) -> u64>(h, 0x48)(h, &mut start) };
        unsafe { method::<unsafe extern "system" fn(Raw, Raw, *const c_void, u64)>(self.dev, 0xa0)(self.dev, r, std::ptr::null(), start) };
        start as usize
    }

    /// An open console list (and its allocator).
    fn list(&self) -> (Raw, Raw) {
        let mut a: Raw = std::ptr::null_mut();
        assert_eq!(unsafe { method::<unsafe extern "system" fn(Raw, u32, *const [u8; 16], *mut Raw) -> i32>(self.dev, 0x48)(self.dev, 0, &IID, &mut a) }, 0);
        let mut l: Raw = std::ptr::null_mut();
        type F = unsafe extern "system" fn(Raw, u32, u32, Raw, Raw, *const [u8; 16], *mut Raw) -> i32;
        assert_eq!(unsafe { method::<F>(self.dev, 0x60)(self.dev, 0, 0, a, std::ptr::null_mut(), &IID, &mut l) }, 0);
        (l, a)
    }
}

fn barriers(l: Raw, b: &[Barrier]) {
    unsafe { method::<unsafe extern "system" fn(Raw, u32, *const Barrier) -> u32>(l, 0xd0)(l, b.len() as u32, b.as_ptr()) };
}
fn clear(l: Raw, rtv: usize, color: [f32; 4]) {
    unsafe { method::<unsafe extern "system" fn(Raw, D3D12_CPU_DESCRIPTOR_HANDLE, *const f32, u32, *const RECT)>(l, 0x1a0)(l, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: rtv }, color.as_ptr(), 0, std::ptr::null()) };
}
fn copy(l: Raw, dst: Raw, src: Raw) {
    unsafe { method::<unsafe extern "system" fn(Raw, Raw, Raw)>(l, 0x88)(l, dst, src) };
}
fn close(l: Raw) {
    assert_eq!(unsafe { method::<unsafe extern "system" fn(Raw) -> i32>(l, 0x48)(l) }, 0);
}
fn execute(q: Raw, l: Raw) {
    unsafe { method::<unsafe extern "system" fn(Raw, u32, *const Raw)>(q, 0x50)(q, 1, &l) };
}
fn present(q: Raw, r: Raw) -> i32 {
    let res = [r];
    let plane = Plane { token: 0, count: 1, resources: res.as_ptr() };
    unsafe { method::<unsafe extern "system" fn(Raw, u32, *const Plane, *const c_void) -> i32>(q, 0x100)(q, 1, &plane, std::ptr::null()) }
}
/// Signal `f` to `v` on `q` and wait (at most 5 s) until the GPU passes it (fence 0x48 with an event).
fn finish(q: Raw, f: Raw, v: u64) {
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    unsafe {
        assert_eq!(method::<unsafe extern "system" fn(Raw, Raw, u64) -> i32>(q, 0x80)(q, f, v), 0);
        let event = CreateEventW(None, false, false, None).unwrap();
        assert_eq!(method::<unsafe extern "system" fn(Raw, u64, *mut c_void) -> i32>(f, 0x48)(f, v, event.0), 0);
        let r = WaitForSingleObject(event, 5000);
        let _ = windows::Win32::Foundation::CloseHandle(event);
        assert_eq!(r, windows::Win32::Foundation::WAIT_OBJECT_0, "the GPU did not reach {v} within 5 s");
    }
}

/// Releases the swap chain, the window and any held wait when the test ends, pass or fail.
struct Shutdown;
impl Drop for Shutdown {
    fn drop(&mut self) {
        crate::present::shutdown();
    }
}

/// Capture the next shown frame of `r` and read it back: its size and RGBA pixels.
fn capture(q: Raw, r: Raw) -> (u32, u32, Vec<u8>) {
    let before = crate::present::captures().len();
    crate::present::request_capture(None);
    let deadline = Instant::now() + Duration::from_secs(10);
    // the display may be full for a frame or two (late frames are not shown and not captured)
    while crate::present::captures().len() == before {
        assert!(Instant::now() < deadline, "no capture within 10 s");
        assert_eq!(present(q, r), 0);
        std::thread::sleep(Duration::from_millis(20));
    }
    let path = crate::present::captures().last().unwrap().clone();
    let dec = png::Decoder::new(std::fs::File::open(&path).unwrap());
    let mut reader = dec.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn every_pixel_is(px: &[u8], want: [u8; 3]) -> bool {
    px.chunks_exact(4).all(|p| p[..3].iter().zip(want).all(|(&a, b)| a.abs_diff(b) <= 1))
}

#[test]
#[ignore = "GPU: by hand with --test-threads=1, while agents\\MEASURING is absent"]
fn present_card_lists_run_in_order_and_the_frame_reaches_a_png() {
    // immediately before the device: no measurement, and the card's lock (dropped last, after the shutdown)
    if measuring() {
        return;
    }
    let _card = match rebuild_pipelines::d3d::CardLock::take() {
        Ok(c) => c,
        Err(why) => {
            println!("skipped: {why}");
            return;
        }
    };
    if measuring() {
        return;
    }
    let _shutdown = Shutdown;
    let mut dev: Raw = std::ptr::null_mut();
    if crate::device::create(&IID, &mut dev) != 0 {
        println!("skipped: no D3D12 device");
        return;
    }
    crate::present::hide_window();
    let dir = std::env::temp_dir().join(format!("rebuild-present-{}", std::process::id()));
    crate::present::set_capture_dir(dir.clone());
    let t = Title { dev };
    let q = t.queue();
    let q2 = t.queue();
    let fence = t.fence();
    let (w, h) = (64u32, 48u32);

    // 1. clear a render target, copy it into the back buffer; execute; present; capture
    let a = t.texture(w as u64, h, true);
    let back = t.texture(w as u64, h, false);
    let a_rtv = t.rtv(a);
    let (l, _alloc) = t.list();
    barriers(l, &[barrier(a, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
    clear(l, a_rtv, [0.25, 0.5, 0.75, 1.0]);
    barriers(l, &[barrier(a, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE), barrier(back, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
    copy(l, back, a);
    barriers(l, &[barrier(a, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON), barrier(back, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
    close(l);
    let st = rebuild_xlist::console::stats(l).unwrap();
    assert_eq!((st.refused, st.splits), (0, 0), "{st:?}");
    execute(q, l);
    let (cw, ch, px) = capture(q, back);
    assert_eq!((cw, ch), (w, h), "the shown region is the back buffer's");
    assert!(every_pixel_is(&px, [64, 128, 191]), "the clear reached the screen: first pixel {:?}", &px[..4]);
    println!("present   frame 1: {cw}x{ch}, every pixel [64, 128, 191]");

    // 2. a list cut by a memory wait whose value another queue's list writes later: the GPU keeps the order, the
    //    title's thread does not wait
    let label_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: 65536,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let labels = t.committed(&label_desc, 2);
    let label = unsafe { method::<unsafe extern "system" fn(Raw) -> u64>(labels, 0x58)(labels) };
    unsafe { std::ptr::write_volatile(label as *mut u64, 0) };
    let d = t.texture(w as u64, h, true);
    let d_rtv = t.rtv(d);
    let shown = t.texture(w as u64, h, false);
    // L1 (queue 1): clear `shown` red; wait for the label; copy d (green, once L2 ran) over it
    let (l1, _a1) = t.list();
    let s_rtv = {
        let rt = t.texture(w as u64, h, true);
        (rt, t.rtv(rt))
    };
    barriers(l1, &[barrier(s_rtv.0, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
    clear(l1, s_rtv.1, [1.0, 0.0, 0.0, 1.0]);
    barriers(l1, &[barrier(s_rtv.0, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE), barrier(shown, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
    copy(l1, shown, s_rtv.0);
    unsafe { method::<unsafe extern "system" fn(Raw, u64, u32, u64)>(l1, 0x250)(l1, label, 7, 1) };
    copy(l1, shown, d);
    barriers(l1, &[barrier(s_rtv.0, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON), barrier(shown, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
    close(l1);
    assert_eq!(rebuild_xlist::console::segments(l1).unwrap().len(), 2, "cut at the wait");
    // L2 (queue 2): clear d green, then the label
    let (l2, _a2) = t.list();
    barriers(l2, &[barrier(d, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_RENDER_TARGET)]);
    clear(l2, d_rtv, [0.0, 1.0, 0.0, 1.0]);
    barriers(l2, &[barrier(d, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COMMON)]);
    unsafe { method::<unsafe extern "system" fn(Raw, u64, u64, u32, u32)>(l2, 0x240)(l2, label, 1, 0, 0) };
    close(l2);
    let bridged = crate::submit::WAITS_BRIDGED.load(Ordering::Relaxed);
    let t0 = Instant::now();
    execute(q, l1);
    let submit = t0.elapsed();
    assert_eq!(crate::submit::WAITS_BRIDGED.load(Ordering::Relaxed), bridged + 1, "the wait went to the bridge");
    assert!(submit < Duration::from_millis(20), "the title's thread waited {submit:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(crate::submit::bridged_pending(), 1, "still waiting for the label");
    execute(q2, l2);
    let (_, _, px) = capture(q, shown);
    assert!(every_pixel_is(&px, [0, 255, 0]), "the copy after the wait ran after the other queue's clear: first pixel {:?}", &px[..4]);
    assert_eq!(unsafe { std::ptr::read_volatile(label as *const u64) }, 1);
    assert_eq!(crate::submit::bridged_pending(), 0);
    println!("present   the wait held queue 1 on the GPU {submit:?} on the title's thread; queue 2's clear came first");

    // 3. cost: submission of a one-segment list, and the present (paced like a 60 Hz title)
    let mut v = 1;
    finish(q, fence, v);
    let n = 2000;
    let t0 = Instant::now();
    for _ in 0..n {
        execute(q, l);
    }
    let per_submit = t0.elapsed() / n;
    // the same host list straight to the host queue: the driver's own share
    let host_q = crate::queue::find(q as usize).unwrap();
    let raw = [windows::core::Interface::as_raw(&rebuild_xlist::console::segments(l).unwrap()[0].list.list)];
    let t0 = Instant::now();
    for _ in 0..n {
        unsafe { (windows::core::Interface::vtable(&host_q.host).ExecuteCommandLists)(windows::core::Interface::as_raw(&host_q.host), 1, raw.as_ptr() as *const _) };
    }
    let per_host = t0.elapsed() / n;
    v += 1;
    finish(q, fence, v);
    let shown0 = crate::present::counts().0;
    let dxgi0 = crate::present::DXGI_NS.load(Ordering::Relaxed);
    let m = 120;
    let mut spent = Duration::ZERO;
    for _ in 0..m {
        let t0 = Instant::now();
        assert_eq!(present(q, back), 0);
        spent += t0.elapsed();
        std::thread::sleep(Duration::from_millis(16));
    }
    let shown = crate::present::counts().0 - shown0;
    let dxgi = (crate::present::DXGI_NS.load(Ordering::Relaxed) - dxgi0) as f64 / 1e3 / shown.max(1) as f64;
    println!(
        "cost      ExecuteCommandLists (1 list, 1 segment): {:.2} µs a call ({:.2} µs of it the host queue's own);          PresentX: {:.1} µs a call ({dxgi:.1} µs of it DXGI Present; {shown} of {m} shown, the rest late)",
        per_submit.as_secs_f64() * 1e6,
        per_host.as_secs_f64() * 1e6,
        spent.as_secs_f64() * 1e6 / m as f64
    );
    v += 1;
    finish(q, fence, v);
    let _ = std::fs::remove_dir_all(&dir);
}
