//! Tests on synthetic files only. The objects are called through their method tables, as the title
//! calls them.

use std::ffi::{c_char, c_void};
use std::mem::offset_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rebuild_vfs::{ModSet, VfsBuilder};

use crate::com::{method, missed};
use crate::engine::{Config, Engine, Pages};
use crate::zlib::{deflate, inflate, InflateError};
use crate::*;

/// A fresh folder under the system temp dir, removed on drop.
pub(crate) struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rebuild-dstorage-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("base")).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Deterministic bytes that compress about 1.3:1 (runs of a few values mixed with noise).
pub(crate) fn data(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let run = (x & 7) as usize + 1;
        let v = (x >> 8) as u8;
        for i in 0..run {
            out.push(if (x >> (16 + i)) & 1 == 1 { v } else { (x >> 24) as u8 & 0x0f });
        }
    }
    out.truncate(len);
    out
}

/// A package drive `G:` over `scratch/base`, an engine with `workers` workers, and its factory.
fn setup(scratch: &Scratch, workers: usize, pages: Option<Arc<dyn Pages>>) -> OwnedFactory {
    let vfs = VfsBuilder::new(scratch.0.join("overlay"))
        .package('G', scratch.0.join("base"), &ModSet::default())
        .build()
        .unwrap();
    OwnedFactory::new(Engine::new(Arc::new(vfs), Config { workers, pages, ..Config::default() }))
}

type OpenFile = extern "system" fn(*mut c_void, *const u16, *const Guid, *mut *mut c_void) -> Hresult;
type CreateStatus = extern "system" fn(*mut c_void, u32, *const c_char, *const Guid, *mut *mut c_void) -> Hresult;
type SetStaging = extern "system" fn(*mut c_void, u32) -> Hresult;
type CreateQueue = extern "system" fn(*mut c_void, *const QueueDesc, *const Guid, *mut *mut c_void) -> Hresult;
type Enqueue = extern "system" fn(*mut c_void, *const Request);
type EnqueueStatus = extern "system" fn(*mut c_void, *mut c_void, u32);
type Plain = extern "system" fn(*mut c_void);
type IsComplete = extern "system" fn(*mut c_void, u32) -> bool;
type GetResult = extern "system" fn(*mut c_void, u32) -> Hresult;
type Release = extern "system" fn(*mut c_void) -> u32;

/// The title's view of one factory.
struct Title {
    factory: *mut c_void,
}

impl Title {
    fn call<F: Copy>(object: *mut c_void, slot: usize) -> F {
        // SAFETY: every object in these tests is ours, and the types above match the slots.
        unsafe { method(object, slot) }
    }
    fn open(&self, path: &str) -> Result<*mut c_void, Hresult> {
        let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut out = std::ptr::null_mut();
        let hr = Self::call::<OpenFile>(self.factory, 4)(self.factory, wide.as_ptr(), &IID_FILE, &mut out);
        if hr == S_OK {
            Ok(out)
        } else {
            Err(hr)
        }
    }
    fn status(&self, n: u32) -> *mut c_void {
        let mut out = std::ptr::null_mut();
        let hr = Self::call::<CreateStatus>(self.factory, 5)(self.factory, n, c"t".as_ptr(), &IID_STATUS, &mut out);
        assert_eq!(hr, S_OK);
        out
    }
    fn queue(&self, capacity: u16, priority: i8) -> *mut c_void {
        let desc = QueueDesc { source_type: 0, capacity, priority, name: c"t".as_ptr(), unknown: 1 };
        let mut out = std::ptr::null_mut();
        let hr = Self::call::<CreateQueue>(self.factory, 10)(self.factory, &desc, &IID_QUEUE, &mut out);
        assert_eq!(hr, S_OK);
        out
    }
    fn staging(&self, bytes: u32) {
        assert_eq!(Self::call::<SetStaging>(self.factory, 8)(self.factory, bytes), S_OK);
    }
}

fn enqueue(queue: *mut c_void, r: &Request) {
    Title::call::<Enqueue>(queue, 3)(queue, r)
}
fn enqueue_status(queue: *mut c_void, array: *mut c_void, i: u32) {
    Title::call::<EnqueueStatus>(queue, 4)(queue, array, i)
}
fn submit(queue: *mut c_void) {
    Title::call::<Plain>(queue, 6)(queue)
}
fn close(queue: *mut c_void) {
    Title::call::<Plain>(queue, 8)(queue)
}
fn done(array: *mut c_void, i: u32) -> bool {
    Title::call::<IsComplete>(array, 3)(array, i)
}
fn result(array: *mut c_void, i: u32) -> Hresult {
    Title::call::<GetResult>(array, 4)(array, i)
}
fn release(object: *mut c_void) -> u32 {
    Title::call::<Release>(object, 2)(object)
}

fn read(options: u64, dst: *mut u8, dst_len: usize, src: *const c_void, offset: u64, src_len: usize) -> Request {
    Request {
        options,
        destination: dst.cast(),
        page_offset: 0,
        destination_size: dst_len as u32,
        source: src,
        offset,
        source_size: src_len as u32,
        intermediate_size: 0,
        tag: 0,
        name: std::ptr::null(),
    }
}

fn wait(array: *mut c_void, i: u32) -> Hresult {
    let start = Instant::now();
    while !done(array, i) {
        assert!(start.elapsed() < Duration::from_secs(20), "status {i} never completed");
        std::thread::yield_now();
    }
    result(array, i)
}

#[test]
fn d1_layouts_match_the_title() {
    assert_eq!(std::mem::size_of::<Request>(), 0x48);
    assert_eq!(offset_of!(Request, destination), 0x08);
    assert_eq!(offset_of!(Request, page_offset), 0x10);
    assert_eq!(offset_of!(Request, destination_size), 0x18);
    assert_eq!(offset_of!(Request, source), 0x20);
    assert_eq!(offset_of!(Request, offset), 0x28);
    assert_eq!(offset_of!(Request, source_size), 0x30);
    assert_eq!(offset_of!(Request, intermediate_size), 0x34);
    assert_eq!(offset_of!(Request, tag), 0x38);
    assert_eq!(offset_of!(Request, name), 0x40);
    assert_eq!(std::mem::size_of::<QueueDesc>(), 0x20);
    assert_eq!(offset_of!(QueueDesc, capacity), 0x08);
    assert_eq!(offset_of!(QueueDesc, priority), 0x0a);
    assert_eq!(offset_of!(QueueDesc, name), 0x10);
    assert_eq!(offset_of!(QueueDesc, unknown), 0x18);
    let r = read(ZLIB | (7 << 1) | PAGE_DESTINATION, std::ptr::null_mut(), 0, std::ptr::null(), 0, 0);
    assert_eq!(r.bcpack_format(), 7);
}

#[test]
fn d1_get_factory_and_interfaces() {
    let s = Scratch::new("factory");
    let vfs = VfsBuilder::new(s.0.join("overlay")).package('G', s.0.join("base"), &ModSet::default()).build().unwrap();
    // One install per process; this is the only test that installs.
    assert!(install(Engine::new(Arc::new(vfs), Config { workers: 2, ..Config::default() })).is_ok());
    let mut f = std::ptr::null_mut();
    // SAFETY: valid GUID and out pointer.
    assert_eq!(unsafe { DStorageGetFactory(&IID_FACTORY, &mut f) }, S_OK);
    assert!(!f.is_null());
    let mut other = std::ptr::null_mut();
    // SAFETY: as above.
    assert_eq!(unsafe { DStorageGetFactory(&IID_QUEUE, &mut other) }, E_NOINTERFACE);
    // QueryInterface on a status array: its own interface and IUnknown, nothing else.
    let t = Title { factory: f };
    let array = t.status(1);
    type Qi = extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Hresult;
    let qi = Title::call::<Qi>(array, 0);
    let mut out = std::ptr::null_mut();
    assert_eq!(qi(array, &IID_STATUS, &mut out), S_OK);
    assert_eq!(out, array);
    assert_eq!(qi(array, &IID_QUEUE, &mut out), E_NOINTERFACE);
    assert_eq!(release(array), 1);
    assert_eq!(release(array), 0);
    // A slot nobody implemented answers E_NOTIMPL and is counted.
    let unknown = Title::call::<extern "system" fn(*mut c_void) -> Hresult>(f, 3);
    assert_eq!(unknown(f), E_NOTIMPL);
    assert!(missed().iter().any(|&(o, slot, n)| o == "factory" && slot == 3 && n >= 1));
}

#[test]
fn d2_ordered_completion_over_files() {
    let s = Scratch::new("order");
    // 3 files: raw bytes, and zlib chunks of very different sizes so reads finish out of order.
    let raw = data(1, 3 << 20);
    std::fs::write(s.0.join("base/raw.bin"), &raw).unwrap();
    let mut packed = Vec::new();
    let mut chunks = Vec::new(); // (offset, packed len, plain bytes)
    for i in 0..120u64 {
        let len = if i % 7 == 0 { 1 << 20 } else { 512 + (i as usize * 997) % 40_000 };
        let plain = data(100 + i, len);
        let z = deflate(&plain, 6);
        chunks.push((packed.len() as u64, z.len(), plain));
        packed.extend_from_slice(&z);
    }
    std::fs::create_dir_all(s.0.join("base/Pack")).unwrap();
    std::fs::write(s.0.join("base/Pack/chunks.z"), &packed).unwrap();

    let f = setup(&s, 4, None);
    let t = Title { factory: f.as_ptr() };
    t.staging(32 << 20);
    let rawf = t.open("G:\\raw.bin").unwrap();
    let zf = t.open("g:\\pack\\CHUNKS.Z").unwrap(); // case-insensitive, as on the console
    let q = t.queue(4159, 2);
    let array = t.status(24);

    let mut dsts: Vec<Vec<u8>> = chunks.iter().map(|c| vec![0u8; c.2.len()]).collect();
    let mut raw_dsts: Vec<Vec<u8>> = (0..120).map(|_| vec![0u8; 16 << 10]).collect();
    // status k follows requests [10k, 10k + 10) of each kind
    for k in 0..12u32 {
        for i in (k as usize * 10)..(k as usize * 10 + 10) {
            let (off, zlen, _) = &chunks[i];
            let d = &mut dsts[i];
            enqueue(q, &read(ZLIB, d.as_mut_ptr(), d.len(), zf, *off, *zlen));
            let rd = &mut raw_dsts[i];
            enqueue(q, &read(0, rd.as_mut_ptr(), rd.len(), rawf, (i as u64) * 20_000, rd.len()));
        }
        enqueue_status(q, array, k);
    }
    assert!(!done(array, 11), "nothing is complete before Submit");
    submit(q);

    // Poll as the title does: entries complete in order, and a complete entry means every request
    // before it is in memory.
    let mut seen = 0u32;
    let start = Instant::now();
    while seen < 12 {
        assert!(start.elapsed() < Duration::from_secs(30));
        let complete: Vec<bool> = (0..12).map(|k| done(array, k)).collect();
        let first_pending = complete.iter().position(|c| !c).unwrap_or(12) as u32;
        assert!(complete[first_pending as usize..].iter().all(|c| !c), "out of order: {complete:?}");
        for k in seen..first_pending {
            assert_eq!(result(array, k), S_OK);
            for i in (k as usize * 10)..(k as usize * 10 + 10) {
                assert!(dsts[i] == chunks[i].2, "chunk {i} not there at status {k}");
                let o = i * 20_000;
                assert!(raw_dsts[i][..] == raw[o..o + (16 << 10)], "raw {i} not there at status {k}");
            }
        }
        seen = first_pending;
        std::thread::yield_now();
    }
    let st = f.engine().stats();
    assert_eq!(st.reads, 240);
    assert_eq!(st.zlib_reads, 120);
    assert_eq!(st.failed, 0);
    close(q);
    release(q);
    release(array);
    release(rawf);
    release(zf);
    assert_eq!(f.engine().vfs().open_handles(), 0, "file objects close their handles");
}

#[test]
fn d2_failures_reach_the_next_status_only() {
    let s = Scratch::new("fail");
    let plain = data(7, 100_000);
    let z = deflate(&plain, 6);
    let mut file = z.clone();
    file.extend_from_slice(b"this is not a zlib stream at all, it is just text bytes");
    std::fs::write(s.0.join("base/a.z"), &file).unwrap();
    let f = setup(&s, 3, None);
    let t = Title { factory: f.as_ptr() };
    let a = t.open("G:\\a.z").unwrap();
    let q = t.queue(64, 0);
    let array = t.status(4);
    let (mut d0, mut d1, mut d2) = (vec![0u8; plain.len()], vec![0u8; 1000], vec![0u8; plain.len()]);
    let (mut small, mut d3) = (vec![0u8; 1000], vec![0u8; 10]);
    let (zl, tail) = (z.len(), file.len() - z.len());
    enqueue(q, &read(ZLIB, d0.as_mut_ptr(), d0.len(), a, 0, zl));
    enqueue_status(q, array, 0);
    enqueue(q, &read(ZLIB, d1.as_mut_ptr(), d1.len(), a, zl as u64, tail)); // corrupt
    enqueue(q, &read(ZLIB, small.as_mut_ptr(), small.len(), a, 0, zl)); // destination too small
    enqueue_status(q, array, 1);
    enqueue(q, &read(ZLIB, d2.as_mut_ptr(), d2.len(), a, 0, zl));
    enqueue_status(q, array, 2);
    enqueue(q, &read(0, d3.as_mut_ptr(), 10, a, file.len() as u64 - 4, 10)); // past the end
    enqueue_status(q, array, 3);
    submit(q);
    assert_eq!(wait(array, 0), S_OK);
    assert_eq!(wait(array, 1), INVALID_DATA, "the first failure in queue order");
    assert_eq!(wait(array, 2), S_OK, "a failure is reported once");
    assert_eq!(wait(array, 3), END_OF_FILE);
    assert!(d0 == plain && d2 == plain);
    assert_eq!(t.open("G:\\missing.bin").unwrap_err(), FILE_NOT_FOUND);
    assert_eq!(t.open("G:\\no\\such\\dir.bin").unwrap_err(), PATH_NOT_FOUND);
    close(q);
}

/// Pages as plain host buffers, numbered by index.
struct FakePages(Mutex<Vec<Vec<u8>>>);

impl Pages for FakePages {
    fn write(&self, pages: &[u64], offset: usize, data: &[u8]) -> bool {
        let mut mem = self.0.lock().unwrap();
        let (mut at, mut left) = (offset, data);
        for &p in pages {
            if left.is_empty() {
                break;
            }
            let Some(page) = mem.get_mut(p as usize) else { return false };
            let n = (PAGE - at).min(left.len());
            page[at..at + n].copy_from_slice(&left[..n]);
            left = &left[n..];
            at = 0;
        }
        left.is_empty()
    }
}

#[test]
fn d2_memory_sources_page_destinations_and_bcpack() {
    let s = Scratch::new("pages");
    let fake = Arc::new(FakePages(Mutex::new(vec![vec![0u8; PAGE]; 8])));
    let f = setup(&s, 2, Some(fake.clone()));
    let t = Title { factory: f.as_ptr() };
    let q = t.queue(64, 1);
    let array = t.status(3);
    // zlib from memory into pages 5, 2, 7 starting 1000 bytes into page 5.
    let plain = data(9, 2 * PAGE);
    let z = deflate(&plain, 1);
    let pages: [u64; 3] = [5, 2, 7];
    let mut r = read(ZLIB | MEMORY_SOURCE | PAGE_DESTINATION, pages.as_ptr() as *mut u8, plain.len(), z.as_ptr().cast(), 0, z.len());
    r.page_offset = 1000;
    enqueue(q, &r);
    enqueue_status(q, array, 0);
    // A BCPack request without a decoder: refused and counted.
    let mut out = vec![0u8; 64];
    enqueue(q, &read(7 << 1 | MEMORY_SOURCE, out.as_mut_ptr(), 64, z.as_ptr().cast(), 0, 16));
    enqueue_status(q, array, 1);
    // An option bit nobody knows.
    enqueue(q, &read(1 << 9 | MEMORY_SOURCE, out.as_mut_ptr(), 64, z.as_ptr().cast(), 0, 16));
    enqueue_status(q, array, 2);
    submit(q);
    assert_eq!(wait(array, 0), S_OK);
    assert_eq!(wait(array, 1), E_NOTIMPL);
    assert_eq!(wait(array, 2), E_NOTIMPL);
    let mem = fake.0.lock().unwrap();
    assert!(mem[5][1000..] == plain[..PAGE - 1000]);
    assert!(mem[2][..] == plain[PAGE - 1000..2 * PAGE - 1000]);
    assert!(mem[7][..1000] == plain[2 * PAGE - 1000..]);
    assert_eq!(f.engine().stats().bcpack_refused, 1);
    close(q);
}

#[test]
fn d2_auto_submit_holds_page_reads_for_the_titles_submit() {
    let s = Scratch::new("held");
    let fake = Arc::new(FakePages(Mutex::new(vec![vec![0u8; PAGE]; 4])));
    let f = setup(&s, 2, Some(fake.clone()));
    let t = Title { factory: f.as_ptr() };
    let q = t.queue(8, 0); // submits itself at 4 commands
    let array = t.status(2);
    let src = data(3, 4096);
    let mut plain_dst = vec![0u8; 4096];
    let page: [u64; 1] = [1];
    enqueue(q, &read(MEMORY_SOURCE, plain_dst.as_mut_ptr(), 4096, src.as_ptr().cast(), 0, 4096));
    enqueue_status(q, array, 0);
    enqueue(q, &read(MEMORY_SOURCE | PAGE_DESTINATION, page.as_ptr() as *mut u8, 4096, src.as_ptr().cast(), 0, 4096));
    enqueue_status(q, array, 1); // 4th command: the queue submits itself
    assert_eq!(wait(array, 0), S_OK, "the address read went at the automatic submit");
    std::thread::sleep(Duration::from_millis(100));
    assert!(!done(array, 1), "the page read and the status behind it wait for Submit");
    assert!(fake.0.lock().unwrap()[1][..4096].iter().all(|&b| b == 0));
    submit(q);
    assert_eq!(wait(array, 1), S_OK);
    assert!(fake.0.lock().unwrap()[1][..4096] == src[..]);
    assert!(plain_dst == src);
    close(q);
}

#[test]
fn d2_full_queue_blocks_then_drains_and_close_drops_unsubmitted() {
    let s = Scratch::new("full");
    let body = data(5, 1 << 20);
    std::fs::write(s.0.join("base/b.bin"), &body).unwrap();
    let f = setup(&s, 2, None);
    let t = Title { factory: f.as_ptr() };
    t.staging(64 << 10); // small staging budget: zlib reads queue up behind it
    let file = t.open("G:\\b.bin").unwrap();
    let q = t.queue(6, -1);
    let array = t.status(1);
    let mut dsts: Vec<Vec<u8>> = (0..200).map(|_| vec![0u8; 4096]).collect();
    // 200 reads through a 6-entry queue: enqueue must block and resume, never deadlock.
    for (i, d) in dsts.iter_mut().enumerate() {
        enqueue(q, &read(0, d.as_mut_ptr(), 4096, file, i as u64 * 4096, 4096));
    }
    enqueue_status(q, array, 0);
    submit(q);
    assert_eq!(wait(array, 0), S_OK);
    for (i, d) in dsts.iter().enumerate() {
        assert!(d[..] == body[i * 4096..(i + 1) * 4096]);
    }
    // Enqueued, never submitted: Close drops it and the entry never completes.
    let q2 = t.queue(64, 0);
    let array2 = t.status(1);
    enqueue(q2, &read(0, dsts[0].as_mut_ptr(), 4096, file, 0, 4096));
    enqueue_status(q2, array2, 0);
    close(q2);
    assert_eq!(result(array2, 0), E_PENDING);
    close(q);
}

#[test]
fn d2_priorities_share_the_workers() {
    // The order is recorded by the worker itself (each read writes through `Pages`, page 0 for the low queue,
    // page 1 for real-time), so the check does not depend on when this thread gets to look: it held once under
    // a loaded machine only by luck of scheduling.
    struct Order(std::sync::Mutex<Vec<u64>>);
    impl Pages for Order {
        fn write(&self, pages: &[u64], _offset: usize, _data: &[u8]) -> bool {
            self.0.lock().unwrap().push(pages[0]);
            true
        }
    }
    let s = Scratch::new("prio");
    let order = Arc::new(Order(std::sync::Mutex::new(Vec::new())));
    let f = setup(&s, 1, Some(order.clone()));
    let engine = f.engine();
    let low = engine.create_queue(4000, -1);
    let rt = engine.create_queue(4000, 2);
    let src = data(11, 64 << 10);
    let (page_low, page_rt) = ([0u64], [1u64]);
    let (al, ah) = (StatusArray::new(400), StatusArray::new(400));
    for i in 0..400 {
        // SAFETY: the page lists and the source outlive the queues (closed below).
        unsafe {
            engine.enqueue_request(low, read(MEMORY_SOURCE | PAGE_DESTINATION, page_low.as_ptr() as *mut u8, 1 << 16, src.as_ptr().cast(), 0, 1 << 16));
            engine.enqueue_request(rt, read(MEMORY_SOURCE | PAGE_DESTINATION, page_rt.as_ptr() as *mut u8, 1 << 16, src.as_ptr().cast(), 0, 1 << 16));
        }
        engine.enqueue_status(low, &al, i as u32);
        engine.enqueue_status(rt, &ah, i as u32);
    }
    // Real-time first: submitting low first would let the worker start on it alone, which is not priority.
    engine.submit(rt);
    engine.submit(low);
    // Generous: only a hung engine takes this long.
    let start = Instant::now();
    while !(ah.is_complete(399) && al.is_complete(399)) {
        assert!(start.elapsed() < Duration::from_secs(120), "the reads did not finish");
        std::thread::yield_now();
    }
    engine.close(low);
    engine.close(rt);
    let order = order.0.lock().unwrap().clone();
    assert_eq!(order.len(), 800);
    let last_rt = order.iter().rposition(|&p| p == 1).unwrap();
    let low_before = order[..last_rt].iter().filter(|&&p| p == 0).count();
    // With one worker and weights 1000:1, real-time finishes while low has done few.
    assert!(low_before < 100, "low priority finished {low_before} of 400 before real-time finished");
    assert_eq!(ah.result(399), S_OK);
    assert_eq!(al.result(399), S_OK);
}

#[test]
fn d3_zlib_round_trip() {
    for (seed, len) in [(1u64, 0usize), (2, 1), (3, 100), (4, 65_536), (5, 1 << 20), (6, 3_000_001)] {
        let plain = data(seed, len);
        for level in [1, 6, 9] {
            let z = deflate(&plain, level);
            let mut out = vec![0xAAu8; len];
            assert_eq!(inflate(&z, &mut out), Ok(len), "len {len} level {level}");
            assert!(out == plain);
            // A larger destination: the stream ends early (bytes past it are not promised).
            let mut big = vec![0xAAu8; len + 10];
            assert_eq!(inflate(&z, &mut big), Ok(len));
            assert!(big[..len] == plain[..]);
            if len > 0 {
                let mut small = vec![0u8; len - 1];
                assert_eq!(inflate(&z, &mut small), Err(InflateError::Short));
                assert_eq!(inflate(&z[..z.len() - 1], &mut out), Err(InflateError::Short));
            }
        }
    }
    // Incompressible data still round-trips.
    let noise: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let mut out = vec![0u8; noise.len()];
    assert_eq!(inflate(&deflate(&noise, 6), &mut out), Ok(noise.len()));
    assert!(out == noise);
    let mut bad = deflate(&noise, 6);
    bad[100] ^= 0xff;
    assert!(inflate(&bad, &mut out).is_err());
    assert_eq!(inflate(b"\x00\x01garbage", &mut out), Err(InflateError::Corrupt));
}

#[cfg(windows)]
mod write_watch {
    use super::*;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const PAGE_READWRITE: u32 = 4;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
        fn GetWriteWatch(flags: u32, base: *mut c_void, size: usize, addrs: *mut *mut c_void, count: *mut usize, gran: *mut u32) -> u32;
    }

    /// Pages of `[base, base + size)` written since the last call (flag 1 resets the watch).
    fn written(base: *mut c_void, size: usize) -> usize {
        let mut addrs = vec![std::ptr::null_mut(); size / 4096];
        let (mut count, mut gran) = (addrs.len(), 0u32);
        // SAFETY: a write-watched region we allocated; the array holds `count` entries.
        let r = unsafe { GetWriteWatch(1, base, size, addrs.as_mut_ptr(), &mut count, &mut gran) };
        assert_eq!(r, 0);
        count
    }

    /// For the lead (agents/dstorage.md, item 3): does write-watch see what each kind of read writes?
    #[test]
    fn worker_writes_are_seen_by_write_watch() {
        let s = Scratch::new("watch");
        let body = data(21, 256 << 10);
        let z = deflate(&body, 6);
        std::fs::write(s.0.join("base/w.bin"), &body).unwrap();
        std::fs::write(s.0.join("base/w.z"), &z).unwrap();
        let f = setup(&s, 2, None);
        let t = Title { factory: f.as_ptr() };
        let (raw, packed) = (t.open("G:\\w.bin").unwrap(), t.open("G:\\w.z").unwrap());
        let size = 1 << 20;
        // SAFETY: a fresh reservation, freed below.
        let base = unsafe { VirtualAlloc(std::ptr::null_mut(), size, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) };
        assert!(!base.is_null());
        let q = t.queue(64, 0);
        let array = t.status(2);
        let mut seen = Vec::new();
        for (i, (file, options, len)) in [(raw, 0, body.len()), (packed, ZLIB, z.len())].into_iter().enumerate() {
            written(base, size);
            enqueue(q, &read(options, base.cast(), body.len(), file, 0, len));
            enqueue_status(q, array, i as u32);
            submit(q);
            assert_eq!(wait(array, i as u32), S_OK);
            // SAFETY: the read above filled this many bytes.
            assert!(unsafe { std::slice::from_raw_parts(base.cast::<u8>(), body.len()) } == &body[..]);
            seen.push(written(base, size));
        }
        println!("write-watch pages seen of 64: plain read (ReadFile) {}, zlib {}", seen[0], seen[1]);
        assert_eq!(seen, [64, 64], "4 KiB pages recorded for a 256 KiB destination");
        close(q);
        // SAFETY: allocated above.
        unsafe { VirtualFree(base, 0, MEM_RELEASE) };
    }
}
