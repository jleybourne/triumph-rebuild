//! The crate's tests: layouts and tables as title code sees them, every view kind through its
//! record and back, the shadow table, raw mirrors, copies and syncs, the oversized heap, on a
//! recording stand-in (no GPU). And, by hand while `agents\MEASURING` is absent, the console
//! path on the card: a buffer and views made through the console calls, read by a shader through
//! the heap and the shadow table; descriptor translation rates.
//!
//! ```text
//! cargo test -p rebuild-d3d12x --release -- --ignored --nocapture --test-threads=1
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{E_NOINTERFACE, E_NOTIMPL, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::format::{AllocationInfo, HeapProperties, KIB_64};
use crate::memory::PlainMemory;
use crate::object::{Header, HEADER_BYTES};
use crate::records::{Record, RAW};
use crate::*;

// ---- the recording stand-in ----

#[derive(Default)]
struct Fake {
    writes: Mutex<HashMap<usize, HostView>>,
    next: AtomicUsize,
}

impl Fake {
    fn at(&self, cpu: usize) -> Option<HostView> {
        self.writes.lock().unwrap().get(&cpu).copied()
    }
}

impl HostDevice for Fake {
    fn heap(&self, kind: i32, count: u32, visible: bool) -> Result<HostHeap, HRESULT> {
        let base = (self.next.fetch_add(1, Ordering::Relaxed) + 1) << 32;
        Ok(HostHeap { heap: None, kind, count, cpu: base, gpu: if visible { base as u64 | 1 << 48 } else { 0 }, step: 1 })
    }
    fn write(&self, at: usize, view: &HostView) {
        self.writes.lock().unwrap().insert(at, *view);
    }
    fn copy(&self, dst: usize, src: usize, _kind: i32) {
        let mut w = self.writes.lock().unwrap();
        if let Some(v) = w.get(&src).copied() {
            w.insert(dst, v);
        }
    }
    fn shadow(&self, entries: u32) -> Result<Shadow, HRESULT> {
        Ok(Shadow::in_memory(entries))
    }
    fn resource(&self, _: &D3D12_RESOURCE_DESC, _: D3D12_HEAP_TYPE, _: D3D12_HEAP_FLAGS, _: Option<&D3D12_CLEAR_VALUE>) -> Result<Option<ID3D12Resource>, HRESULT> {
        Ok(None)
    }
    fn footprints(&self, _: &D3D12_RESOURCE_DESC, _: u32, _: u32, _: u64, _: *mut D3D12_PLACED_SUBRESOURCE_FOOTPRINT, _: *mut u32, _: *mut u64, total: *mut u64) {
        if !total.is_null() {
            unsafe { total.write(0) };
        }
    }
}

#[derive(Default)]
struct Notes(Mutex<Vec<(&'static str, u64)>>);
impl Events for Notes {
    fn note(&self, what: &'static str, value: u64) {
        self.0.lock().unwrap().push((what, value));
    }
}

struct Rig {
    r: Resources,
    fake: Arc<Fake>,
    mem: Arc<PlainMemory>,
    notes: Arc<Notes>,
}

const SMALL: Limits = Limits { views: 4096, samplers: 64 };

fn rig_with(limits: Limits, bytes: u64) -> Rig {
    let fake = Arc::new(Fake::default());
    let mem = Arc::new(PlainMemory::new(bytes));
    let notes = Arc::new(Notes::default());
    let r = Resources::with_host(fake.clone(), mem.clone(), notes.clone(), limits);
    Rig { r, fake, mem, notes }
}

fn rig() -> Rig {
    rig_with(SMALL, 64 << 20)
}

/// The function in `offset` of `obj`'s table, as title code loads it.
fn method<F: Copy>(obj: Raw, offset: usize) -> F {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    unsafe {
        let table = *obj.cast::<*const *const ()>();
        std::mem::transmute_copy(&*table.add(offset / 8))
    }
}

type Release = unsafe extern "system" fn(Raw) -> u32;

fn release(obj: Raw) -> u32 {
    unsafe { method::<Release>(obj, 0x10)(obj) }
}

const UPLOAD: HeapProperties = HeapProperties { kind: 2, cpu_pages: 0, pool: 0, creation_nodes: 1, visible_nodes: 1 };
const DEFAULT: HeapProperties = HeapProperties { kind: 1, cpu_pages: 0, pool: 0, creation_nodes: 1, visible_nodes: 1 };

fn buffer(r: &Resources, bytes: u64) -> Raw {
    let d = crate::resource::buffer_desc(bytes, true);
    let mut out: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { r.create_committed(&UPLOAD, 0, &d, 0, std::ptr::null(), &ID3D12Resource::IID, &mut out) }, S_OK);
    out
}

fn texture(r: &Resources, format: i32, flags: i32) -> Raw {
    let d = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 64,
        Height: 64,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT(format),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Flags: D3D12_RESOURCE_FLAGS(flags),
        ..Default::default()
    };
    let mut out: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { r.create_committed(&DEFAULT, 0, &d, 0, std::ptr::null(), &ID3D12Resource::IID, &mut out) }, S_OK);
    out
}

#[repr(C)]
struct Ext([u32; 4], u64, u64);

/// A descriptor heap over `count` slots of title memory (placed), or ours.
fn heap(rig: &Rig, kind: u32, count: u32, visible: bool, placed: bool) -> (Raw, u64) {
    let stride = u64::from(rig.r.increment(kind));
    let bytes = stride * u64::from(count);
    let address = if placed { rig.mem.allocate(bytes, KIB_64, &UPLOAD).unwrap() } else { 0 };
    let e = Ext([kind, count, u32::from(visible), 0], address, if placed { bytes } else { 0 });
    let mut out: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { rig.r.create_descriptor_heap(&e.0, &ID3D12DescriptorHeap::IID, &mut out) }, S_OK);
    let start = rig.r.heap_state(out).unwrap().start;
    if placed {
        assert_eq!(start, address, "a placed heap's descriptors live in the title's memory");
    }
    (out, start)
}

fn record_at(handle: u64) -> Option<Record> {
    Record::decode(unsafe { &*(handle as *const [u32; 8]) })
}

// ---- layouts and tables ----

#[test]
fn every_object_starts_with_the_console_header() {
    assert_eq!(size_of::<Header>(), HEADER_BYTES);
    assert_eq!(std::mem::offset_of!(Header, bits), 0x08);
    assert_eq!(std::mem::offset_of!(Header, refs), 0x0c);
    let rig = rig();
    let b = buffer(&rig.r, 256);
    let (h, _) = heap(&rig, 0, 16, true, true);
    for (obj, kind) in [(b, Object::Resource), (h, Object::DescriptorHeap)] {
        let head = unsafe { &*obj.cast::<Header>() };
        assert_eq!(head.vtable, crate::object::table(kind).funcs.as_ptr());
        assert_eq!(head.bits, 0);
        assert_eq!(head.refs.load(Ordering::Relaxed), 1);
    }
    assert_eq!(release(b), 0);
    assert_eq!(release(h), 0);
}

#[test]
fn tables_name_the_implemented_slots_and_trap_the_rest() {
    let want: [(Object, &[(usize, &str)]); 3] = [
        (Object::Resource, &[(0x38, "GetDevice"), (0x40, "Map"), (0x48, "Unmap"), (0x50, "GetDesc"), (0x58, "GetGPUVirtualAddress"), (0x70, "GetHeapProperties")]),
        (Object::Heap, &[(0x38, "GetDevice"), (0x40, "GetDesc")]),
        (Object::DescriptorHeap, &[(0x40, "GetDesc"), (0x48, "GetCPUDescriptorHandleForHeapStart"), (0x50, "GetGPUDescriptorHandleForHeapStart")]),
    ];
    for (o, names) in want {
        let info = slots::slots(o);
        assert_eq!(info.len(), slots::SLOTS);
        for &(off, name) in names {
            assert_eq!(info[off / 8].kind, SlotKind::Implemented(name), "{} {off:#x}", o.name());
        }
        assert!(info.iter().any(|s| matches!(s.kind, SlotKind::Trap(_))));
    }
    // value and pointer slots never answer an HRESULT
    let res = slots::slots(Object::Resource);
    assert_eq!(res[0x60 / 8].kind, SlotKind::Trap(slots::Returns::Hresult));
    assert_eq!(res[0x80 / 8].kind, SlotKind::Trap(slots::Returns::Value));
    assert_eq!(res[0x28 / 8].kind, SlotKind::Trap(slots::Returns::Hresult));
}

#[test]
fn a_trap_counts_reports_once_and_fails_by_its_kind() {
    let rig = rig();
    let b = buffer(&rig.r, 256);
    type Hr = unsafe extern "system" fn(Raw) -> HRESULT;
    type V = unsafe extern "system" fn(Raw) -> u64;
    let count = || traps().into_iter().find(|t| t.object == "resource" && t.offset == 0x68).map_or(0, |t| t.count);
    let before = count();
    assert_eq!(unsafe { method::<Hr>(b, 0x68)(b) }, E_NOTIMPL);
    assert_eq!(unsafe { method::<Hr>(b, 0x68)(b) }, E_NOTIMPL);
    assert_eq!(unsafe { method::<V>(b, 0x98)(b) }, 0, "a value slot answers 0, not an HRESULT");
    assert_eq!(count(), before + 2);
    assert!(traps().into_iter().any(|t| t.object == "resource" && t.offset == 0x68 && t.first_ns > 0));
    release(b);
}

#[test]
fn a_resource_answers_as_title_code_calls_it() {
    let rig = rig();
    let b = buffer(&rig.r, 1000);
    let res = rig.r.console_resource(b).unwrap();
    // GPU address = CPU address = console memory from the model, 64 KiB aligned
    type Va = unsafe extern "system" fn(Raw) -> u64;
    let va = unsafe { method::<Va>(b, 0x58)(b) };
    assert_eq!(va, res.address);
    assert_eq!(va % KIB_64, 0);
    assert!(va >= rig.mem.base());
    type Map = unsafe extern "system" fn(Raw, u32, *const std::ffi::c_void, *mut u64) -> HRESULT;
    let mut p = 0u64;
    assert_eq!(unsafe { method::<Map>(b, 0x40)(b, 0, std::ptr::null(), &mut p) }, S_OK);
    assert_eq!(p, va);
    unsafe { (p as *mut u32).write(0x1234_5678) };
    // GetDesc in both register orders
    type Desc = unsafe extern "system" fn(Raw, Raw) -> Raw;
    let mut d = D3D12_RESOURCE_DESC::default();
    let dp: Raw = (&mut d as *mut D3D12_RESOURCE_DESC).cast();
    let got = unsafe { method::<Desc>(b, 0x50)(b, dp) };
    assert_eq!(got, dp);
    assert_eq!(d.Width, 1000);
    let mut d2 = D3D12_RESOURCE_DESC::default();
    unsafe { method::<Desc>(b, 0x50)((&mut d2 as *mut D3D12_RESOURCE_DESC).cast(), b) };
    assert_eq!(d2.Width, 1000, "(out, this) works too");
    // heap properties
    type Hp = unsafe extern "system" fn(Raw, *mut HeapProperties, *mut u32) -> HRESULT;
    let (mut hp, mut f) = (HeapProperties::default(), 7u32);
    assert_eq!(unsafe { method::<Hp>(b, 0x70)(b, &mut hp, &mut f) }, S_OK);
    assert_eq!((hp, f), (UPLOAD, 0));
    // QueryInterface
    type Qi = unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT;
    let mut o: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { method::<Qi>(b, 0)(b, &ID3D12Pageable::IID, &mut o) }, S_OK);
    assert_eq!(o, b);
    assert_eq!(unsafe { method::<Qi>(b, 0)(b, &ID3D12Heap::IID, &mut o) }, E_NOINTERFACE);
    assert!(o.is_null());
    assert_eq!(release(b), 1);
    assert_eq!(release(b), 0);
}

#[test]
fn placed_resources_sit_at_their_heap_offset_and_sizes_are_the_consoles() {
    let rig = rig();
    let hd = crate::format::HeapDesc { size: 4 * KIB_64, properties: DEFAULT, alignment: 0, flags: 0 };
    let mut h: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { rig.r.create_heap(&hd, &ID3D12Heap::IID, &mut h) }, S_OK);
    let base = crate::resource::heap_object(h).unwrap().address;
    let d = crate::resource::buffer_desc(KIB_64, false);
    let mut p: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { rig.r.create_placed(h, KIB_64, &d, 0, std::ptr::null(), &ID3D12Resource::IID, &mut p) }, S_OK);
    assert_eq!(rig.r.console_resource(p).unwrap().address, base + KIB_64);
    assert_eq!(release(h), 1, "the placed resource holds its heap");
    let mut q: Raw = std::ptr::null_mut();
    assert_ne!(unsafe { rig.r.create_placed(h, 4 * KIB_64, &d, 0, std::ptr::null(), &ID3D12Resource::IID, &mut q) }, S_OK, "past the heap");
    release(p);
    let mut info = AllocationInfo::default();
    let t = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 256,
        Height: 256,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    let got = unsafe { rig.r.allocation_info(&mut info, 0, 1, &t) };
    assert_eq!(got, &mut info as *mut AllocationInfo);
    assert_eq!(info, crate::format::footprint(&t).unwrap());
    // two descriptions through 0x360: 64-byte D3D12_RESOURCE_DESC1s, each placed
    let d1 = D3D12_RESOURCE_DESC1 { Dimension: t.Dimension, Width: 256, Height: 256, DepthOrArraySize: 1, MipLevels: 1, Format: t.Format, SampleDesc: t.SampleDesc, ..Default::default() };
    let b1 = D3D12_RESOURCE_DESC1 { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: 10, Height: 1, DepthOrArraySize: 1, MipLevels: 1, SampleDesc: t.SampleDesc, ..Default::default() };
    let mut each = [crate::format::AllocationInfo1::default(); 2];
    unsafe { rig.r.allocation_info2(&mut info, 0, 2, [d1, b1].as_ptr(), each.as_mut_ptr()) };
    assert_eq!(each[1].offset, info.size - KIB_64);
    assert_eq!(each[1].size, KIB_64, "the second description is read at +64");
    // a texture at a title address
    let mut a: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { rig.r.create_at_address(0x7_0000_0000, &d1, 0, std::ptr::null(), &ID3D12Resource::IID, &mut a) }, S_OK);
    assert_eq!(rig.r.console_resource(a).unwrap().address, 0x7_0000_0000);
    release(a);
    // page pools
    let mut handle = 0usize;
    assert_eq!(unsafe { rig.r.register_page_pool(0x10_0000, 4, &mut handle) }, S_OK);
    assert_eq!(rig.r.page_pool(handle), Some((0x10_0000, 4)));
    assert_eq!(rig.r.unregister_page_pool(handle), S_OK);
    assert_eq!(rig.r.page_pool(handle), None);
}

#[test]
fn a_descriptor_heap_hands_out_its_start_and_desc() {
    let rig = rig();
    let (h, start) = heap(&rig, 0, 64, true, true);
    type Start = unsafe extern "system" fn(Raw, *mut u64) -> u64;
    let mut out = 0u64;
    let got = unsafe { method::<Start>(h, 0x48)(h, &mut out) };
    assert_eq!((out, got), (start, &mut out as *mut u64 as u64), "C++ ABI: written to the caller's slot and returned");
    let got = unsafe { method::<Start>(h, 0x50)(h, std::ptr::null_mut()) };
    assert_eq!(got, start, "a C caller reads the value");
    type Desc = unsafe extern "system" fn(Raw, Raw) -> Raw;
    let mut d = [0u32; 4];
    unsafe { method::<Desc>(h, 0x40)(h, d.as_mut_ptr().cast()) };
    assert_eq!(d, [0, 64, 1, 0]);
    assert_eq!([0, 1, 2, 3].map(|k| rig.r.increment(k)), [32, 16, 128, 128]);
    release(h);
}

// ---- views: each kind through its record and back ----

fn srv_buffer(first: u64, n: u32, stride: u32, raw: bool, format: DXGI_FORMAT) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: crate::views::MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV { FirstElement: first, NumElements: n, StructureByteStride: stride, Flags: if raw { D3D12_BUFFER_SRV_FLAG_RAW } else { D3D12_BUFFER_SRV_FLAG_NONE } },
        },
    }
}

fn uav_buffer(first: u64, n: u32, stride: u32, raw: bool, format: DXGI_FORMAT) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV { FirstElement: first, NumElements: n, StructureByteStride: stride, CounterOffsetInBytes: 0, Flags: if raw { D3D12_BUFFER_UAV_FLAG_RAW } else { D3D12_BUFFER_UAV_FLAG_NONE } },
        },
    }
}

/// (first dword, dwords) of a raw SRV.
fn raw_range(v: &HostView) -> (u64, u32) {
    let HostView::Srv { desc, .. } = v else { panic!("not an SRV: {}", v.describe()) };
    assert!(v.is_raw_srv(), "{}", v.describe());
    let b = unsafe { desc.Anonymous.Buffer };
    (b.FirstElement, b.NumElements)
}

#[test]
fn buffer_views_round_trip_with_a_raw_mirror_and_a_shadow_entry() {
    let rig = rig();
    let b = buffer(&rig.r, 4 * KIB_64);
    let base = rig.r.console_resource(b).unwrap().address;
    let off = base - rig.mem.base(); // the buffer's offset in the plain model's one range
    let (h, start) = heap(&rig, 0, 64, true, true);
    let st = rig.r.heap_state(h).unwrap();
    let p = rig.r.view_index_limit();
    assert_eq!(p, SMALL.null_index());
    let slot = |i: u64| start + 32 * i;
    let shadow = |i: u32| st.shadow.as_ref().unwrap().read(i);
    let primary = |i: u32| rig.fake.at(st.host.cpu_at(i)).unwrap();
    let alt = |i: u32| rig.fake.at(st.host.cpu_at(shadow(i)[2])).unwrap();
    unsafe {
        // 0: CBV of 300 bytes at +512
        rig.r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: base + 512, SizeInBytes: 300 }, slot(0));
        // 1: raw SRV of 64 dwords from dword 8
        rig.r.create_srv(b, &srv_buffer(8, 64, 0, true, DXGI_FORMAT_R32_TYPELESS), slot(1));
        // 2: structured SRV, stride 24, 10 elements from element 3
        rig.r.create_srv(b, &srv_buffer(3, 10, 24, false, DXGI_FORMAT_UNKNOWN), slot(2));
        // 3: typed SRV R32G32B32A32_FLOAT, 5 elements from element 2
        rig.r.create_srv(b, &srv_buffer(2, 5, 0, false, DXGI_FORMAT_R32G32B32A32_FLOAT), slot(3));
        // 4: raw UAV of 32 dwords from dword 16
        rig.r.create_uav(b, std::ptr::null_mut(), &uav_buffer(16, 32, 0, true, DXGI_FORMAT_R32_TYPELESS), slot(4));
        // 5: typed UAV R32_UINT, 100 from element 4
        rig.r.create_uav(b, std::ptr::null_mut(), &uav_buffer(4, 100, 0, false, DXGI_FORMAT_R32_UINT), slot(5));
        // 6: structured UAV, stride 12, 8 from element 1
        rig.r.create_uav(b, std::ptr::null_mut(), &uav_buffer(1, 8, 12, false, DXGI_FORMAT_UNKNOWN), slot(6));
    }
    // records in the title's memory
    assert_eq!(record_at(slot(0)), Some(Record::Cbv { va: base + 512, size: 300 }));
    assert_eq!(record_at(slot(1)), Some(Record::Buffer { uav: false, va: base + 32, format: 39, elements: 64, stride: 0, flags: RAW }));
    assert_eq!(record_at(slot(2)), Some(Record::Buffer { uav: false, va: base + 72, format: 0, elements: 10, stride: 24, flags: 0 }));
    assert_eq!(record_at(slot(3)), Some(Record::Buffer { uav: false, va: base + 32, format: 2, elements: 5, stride: 0, flags: 0 }));
    assert_eq!(record_at(slot(4)), Some(Record::Buffer { uav: true, va: base + 64, format: 39, elements: 32, stride: 0, flags: RAW }));
    // every buffer view: a raw SRV at its index over exactly its bytes
    let want = [(off + 512, 300u64), (off + 32, 256), (off + 72, 240), (off + 32, 80), (off + 64, 128), (off + 16, 400), (off + 12, 96)];
    for (i, &(byte, len)) in want.iter().enumerate() {
        let (first, n) = raw_range(&primary(i as u32));
        let lead = byte % 16;
        assert_eq!(first * 4, byte - lead, "slot {i} starts 16-byte aligned");
        assert_eq!(u64::from(n) * 4, (lead + len).next_multiple_of(4), "slot {i} covers its bytes");
        assert_eq!(u64::from(shadow(i as u32)[3]), lead, "slot {i}: the lead is in the shadow table");
    }
    // the shadow table: (stride, records, alternate, lead)
    assert_eq!(&shadow(0)[..2], &[16, 300], "a CBV is 16-byte rows (records stay in bytes)");
    assert_eq!(&shadow(1)[..2], &[1, 256]);
    assert_eq!(&shadow(2)[..2], &[24, 10]);
    assert_eq!(&shadow(3)[..2], &[16, 5]);
    assert_eq!(&shadow(4)[..2], &[1, 128]);
    assert_eq!(&shadow(5)[..2], &[4, 100]);
    assert_eq!(&shadow(6)[..2], &[12, 8]);
    // alternates: the descriptor's own kind
    assert_eq!(shadow(0)[2], p, "a CBV is only its raw SRV in a shader-visible heap: no alternate");
    assert_eq!(shadow(1)[2], p, "a raw SRV needs no alternate: the null slot");
    assert_eq!(shadow(2)[2], p);
    assert_eq!(alt(3).describe(), format!("srv typed 0x0 fmt 2 first {} n 5", (off + 32) / 16));
    assert_eq!(alt(4).describe(), format!("uav raw 0x0 fmt 39 first {} n 32", (off + 64) / 4));
    assert_eq!(alt(5).describe(), format!("uav typed 0x0 fmt 42 first {} n 100", (off + 16) / 4));
    assert!(alt(6).describe().starts_with("uav raw"));
    for i in [3, 4, 5, 6] {
        assert!(shadow(i)[2] > p, "alternates live past the null slot");
    }
    assert_eq!(rig.r.stats().views_created, 7);
    release(b);
    release(h);
}

/// WARP run with the debug layer: a title view of 1,048,576 R32_UINT over 64 KiB the model knows was an invalid
/// CreateShaderResourceView (#31) and removed the device. A typed view keeps only the elements the memory
/// holds from its start; the shadow table's records follow.
#[test]
fn a_typed_view_longer_than_its_memory_is_cut_to_fit() {
    let rig = rig();
    let b = buffer(&rig.r, KIB_64);
    let base = rig.r.console_resource(b).unwrap().address;
    let room = rig.mem.base() + (64 << 20) - base; // the plain model's one range ends at 64 MiB
    let (h, start) = heap(&rig, 0, 4, true, true);
    let st = rig.r.heap_state(h).unwrap();
    unsafe {
        rig.r.create_srv(b, &srv_buffer(0, 1 << 25, 0, false, DXGI_FORMAT_R32_UINT), start);
        rig.r.create_uav(b, std::ptr::null_mut(), &uav_buffer(0, 1 << 25, 0, false, DXGI_FORMAT_R32_UINT), start + 32);
    }
    let fit = (room / 4) as u32;
    let alt = |i: u32| rig.fake.at(st.host.cpu_at(st.shadow.as_ref().unwrap().read(i)[2])).unwrap().describe();
    assert!(alt(0).ends_with(&format!(" n {fit}")), "{}", alt(0));
    assert!(alt(1).ends_with(&format!(" n {fit}")), "{}", alt(1));
    assert_eq!(st.shadow.as_ref().unwrap().read(0)[1], fit, "records");
    assert_eq!(st.stats.clamped.load(Ordering::Relaxed), 2);
    release(b);
    release(h);
}

#[test]
fn a_constant_buffer_off_256_keeps_its_raw_view() {
    let rig = rig();
    let b = buffer(&rig.r, KIB_64);
    let base = rig.r.console_resource(b).unwrap().address;
    let (h, start) = heap(&rig, 0, 4, true, true);
    let st = rig.r.heap_state(h).unwrap();
    unsafe { rig.r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: base + 100, SizeInBytes: 64 }, start) };
    let sh = st.shadow.as_ref().unwrap().read(0);
    assert_eq!(sh[2], st.null_index, "no alternate");
    assert!(rig.fake.at(st.host.cpu_at(0)).unwrap().is_raw_srv());
    assert_eq!(sh[3], 4, "100 = 96 + 4");
    assert_eq!(st.stats.misaligned.load(Ordering::Relaxed), 1);
    release(b);
    release(h);
}

/// GBV on WARP: "Descriptor Type in Heap: SRV, Register Type: CBV". Console descriptors are untyped, so a slot
/// the title fills with a CBV may be read as a buffer and the other way round; translated shaders read every
/// heap buffer raw, so a CBV into a shader-visible view heap is a raw SRV over the same bytes (R32_TYPELESS,
/// RAW, from the 16-byte aligned start below it with the lead in the shadow table) and no CBV is written
/// anywhere in that heap; a null CBV is a null raw SRV.
#[test]
fn a_cbv_into_a_view_heap_slot_is_a_raw_srv() {
    let rig = rig();
    let b = buffer(&rig.r, KIB_64);
    let base = rig.r.console_resource(b).unwrap().address;
    let off = base - rig.mem.base();
    let (h, start) = heap(&rig, 0, 4, true, true);
    let st = rig.r.heap_state(h).unwrap();
    unsafe {
        rig.r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: base + 256, SizeInBytes: 512 }, start);
        rig.r.create_cbv(std::ptr::null(), start + 32);
        rig.r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: 0, SizeInBytes: 256 }, start + 64);
    }
    let v = rig.fake.at(st.host.cpu_at(0)).unwrap();
    let HostView::Srv { desc, .. } = v else { panic!("not an SRV: {}", v.describe()) };
    let buf = unsafe { desc.Anonymous.Buffer };
    assert_eq!(desc.Format, DXGI_FORMAT_R32_TYPELESS);
    assert_ne!(buf.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0, 0);
    assert_eq!((buf.FirstElement, buf.NumElements), ((off + 256) / 4, 512 / 4), "offset / 4, size / 4");
    let sh = st.shadow.as_ref().unwrap().read(0);
    assert_eq!(sh, [16, 512, st.null_index, 0], "16-byte rows; records in bytes");
    for i in [1, 2] {
        assert_eq!(rig.fake.at(st.host.cpu_at(i)).unwrap().describe(), "null buffer", "slot {i}: a null raw SRV");
        assert_eq!(st.shadow.as_ref().unwrap().read(i)[2], st.null_index);
    }
    let cbvs = rig.fake.writes.lock().unwrap().values().filter(|v| matches!(v, HostView::Cbv { .. } | HostView::NullCbv)).count();
    assert_eq!(cbvs, 0, "no CBV anywhere in a shader-visible heap");
    release(b);
    release(h);
}

#[test]
fn texture_views_samplers_targets_and_nulls_round_trip() {
    let rig = rig();
    let t = texture(&rig.r, 29, 1 | 4); // an SRGB render target with unordered access
    let d = texture(&rig.r, 40, 2); // D32 depth
    let (h, start) = heap(&rig, 0, 16, true, true);
    let (s, sstart) = heap(&rig, 1, 16, true, true);
    let (rt, rstart) = heap(&rig, 2, 4, false, false);
    let (ds, dstart) = heap(&rig, 3, 4, false, false);
    let st = rig.r.heap_state(h).unwrap();
    let prim = |i: u32| rig.fake.at(st.host.cpu_at(i)).unwrap().describe();
    unsafe {
        rig.r.create_srv(t, std::ptr::null(), start); // default view: the resource's typed format
        rig.r.create_uav(t, std::ptr::null_mut(), std::ptr::null(), start + 32);
        rig.r.create_srv(d, std::ptr::null(), start + 64); // depth read as R32_FLOAT
        rig.r.create_srv(std::ptr::null_mut(), &D3D12_SHADER_RESOURCE_VIEW_DESC { ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D, ..Default::default() }, start + 96);
        rig.r.create_cbv(std::ptr::null(), start + 128);
    }
    assert_eq!(prim(0), "srv texture 0x0 fmt 29 dim 5");
    assert_eq!(prim(1), "uav texture 0x0 fmt 29 dim 5");
    assert_eq!(prim(2), "srv texture 0x0 fmt 41 dim 5");
    assert_eq!(prim(3), "null texture dim 4");
    assert_eq!(prim(4), "null buffer");
    assert!(matches!(record_at(start), Some(Record::Entry { .. })));
    assert_eq!(st.shadow.as_ref().unwrap().read(0), [0, 0, st.null_index, 0]);
    // the same view twice: one entry
    unsafe { rig.r.create_srv(t, std::ptr::null(), start + 160) };
    assert_eq!(record_at(start), record_at(start + 160));
    // samplers: PC and console descriptions, 16-byte records
    let sst = rig.r.heap_state(s).unwrap();
    let pc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        MaxLOD: 16.0,
        ..Default::default()
    };
    let mut cw = [0u32; 27];
    (cw[1], cw[2], cw[3], cw[6], cw[7], cw[8]) = (1, 1, 2, 3, 3, 3);
    unsafe {
        rig.r.create_sampler(&pc, sstart);
        rig.r.create_sampler_x(&cw, sstart + 16);
    }
    assert_eq!(rig.fake.at(sst.host.cpu_at(0)).unwrap().describe(), "sampler filter 21 addr 1");
    assert_eq!(rig.fake.at(sst.host.cpu_at(1)).unwrap().describe(), "sampler filter 21 addr 3");
    assert!(Record::decode16(unsafe { &*(sstart as *const [u32; 4]) }).is_some());
    // render target and depth views, and their CPU handles for the list translator
    unsafe {
        rig.r.create_rtv(t, std::ptr::null(), rstart + 128);
        rig.r.create_dsv(d, std::ptr::null(), dstart);
    }
    let rst = rig.r.heap_state(rt).unwrap();
    let dst = rig.r.heap_state(ds).unwrap();
    assert_eq!(rig.r.cpu_descriptor((rstart + 128) as usize), Some(rst.host.cpu_at(1)));
    assert_eq!(rig.fake.at(rst.host.cpu_at(1)).unwrap().describe(), "rtv 0x0 fmt 29");
    assert_eq!(rig.r.cpu_descriptor(dstart as usize), Some(dst.host.cpu_at(0)));
    assert_eq!(rig.fake.at(dst.host.cpu_at(0)).unwrap().describe(), "dsv 0x0 fmt 40");
    // a UAV clear's halves: a CPU-only copy of the slot's own view; the GPU handle of it
    let c = rig.r.cpu_descriptor((start + 32) as usize).unwrap();
    assert_eq!(rig.fake.at(c).unwrap().describe(), "uav texture 0x0 fmt 29 dim 5");
    assert_eq!(rig.r.own_kind_gpu(start + 32), Some(st.host.gpu_at(1)));
    // a released resource's records go stale: a copy of one mirrors as null
    release(t);
    unsafe { rig.r.copy_descriptors_simple(1, start + 7 * 32, start, 0) };
    assert_eq!(prim(7), "null buffer");
    for o in [d, h, s, rt, ds] {
        release(o);
    }
}

#[test]
fn placed_views_name_memory_by_address() {
    let rig = rig();
    let mem = rig.mem.allocate(KIB_64, KIB_64, &UPLOAD).unwrap();
    let (h, start) = heap(&rig, 0, 8, true, true);
    let st = rig.r.heap_state(h).unwrap();
    let rd = crate::resource::buffer_desc(4096, false);
    let rw: [u32; 14] = unsafe { std::mem::transmute(rd) };
    let mut vw = [0u32; 16];
    let pc: [u32; 10] = unsafe { std::mem::transmute(srv_buffer(4, 16, 16, false, DXGI_FORMAT_UNKNOWN)) };
    vw[..10].copy_from_slice(&pc);
    (vw[10], vw[11]) = (mem as u32, (mem >> 32) as u32);
    unsafe { rig.r.create_placed_view(false, &rw, &vw, start) };
    assert_eq!(record_at(start), Some(Record::Buffer { uav: false, va: mem + 64, format: 0, elements: 16, stride: 16, flags: 0 }));
    assert_eq!(raw_range(&rig.fake.at(st.host.cpu_at(0)).unwrap()).0 * 4, mem + 64 - rig.mem.base());
    assert_eq!(&st.shadow.as_ref().unwrap().read(0)[..2], &[16, 16]);
    // a placed texture view: an entry; with no host texture (the stand-in) a null of its dimension
    let td = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 64,
        Height: 64,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_BC1_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    let tw: [u32; 14] = unsafe { std::mem::transmute(td) };
    let mut tv = [0u32; 16];
    (tv[0], tv[1]) = (71, 4);
    (tv[10], tv[11]) = (mem as u32, (mem >> 32) as u32);
    unsafe { rig.r.create_placed_view(false, &tw, &tv, start + 32) };
    assert_eq!(record_at(start + 32), Some(Record::Null { uav: false, dimension: 4 }));
    release(h);
}

// ---- copies, syncs and the shadow table ----

#[test]
fn records_copied_as_bytes_are_mirrored_at_their_new_index() {
    let rig = rig();
    let b = buffer(&rig.r, KIB_64);
    let base = rig.r.console_resource(b).unwrap().address;
    let (cpu_heap, cstart) = heap(&rig, 0, 8, false, false);
    let (gpu_heap, gstart) = heap(&rig, 0, 8, true, true);
    let st = rig.r.heap_state(gpu_heap).unwrap();
    let cst = rig.r.heap_state(cpu_heap).unwrap();
    unsafe { rig.r.create_srv(b, &srv_buffer(0, 4, 0, false, DXGI_FORMAT_R32_FLOAT), cstart + 32) };
    // a CPU-only heap holds the view's own kind
    assert!(rig.fake.at(cst.host.cpu_at(1)).unwrap().describe().starts_with("srv typed"));
    // the API copy
    unsafe { rig.r.copy_descriptors_simple(1, gstart + 5 * 32, cstart + 32, 0) };
    assert!(rig.fake.at(st.host.cpu_at(5)).unwrap().is_raw_srv());
    assert_eq!(&st.shadow.as_ref().unwrap().read(5)[..2], &[4, 4]);
    // the title's own byte copy, then the lead's write-watch says which bytes changed
    unsafe { std::ptr::copy_nonoverlapping((cstart + 32) as *const u8, (gstart + 2 * 32) as *mut u8, 32) };
    assert!(rig.fake.at(st.host.cpu_at(2)).is_none(), "not mirrored before the sync");
    let before = rig.r.stats();
    assert_eq!(rig.r.sync(gstart, 8 * 32), 8);
    let after = rig.r.stats();
    assert!(rig.fake.at(st.host.cpu_at(2)).unwrap().is_raw_srv());
    assert_eq!(after.views_mirrored - before.views_mirrored, 1, "only the changed slot");
    assert_eq!(after.views_skipped - before.views_skipped, 7);
    // CopyDescriptors with ranges: two sources into one destination range
    unsafe { rig.r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: base, SizeInBytes: 256 }, cstart) };
    let (srcs, sizes) = ([cstart, cstart + 32], [1u32, 1]);
    let (dsts, dsizes) = ([gstart + 6 * 32], [2u32]);
    unsafe { rig.r.copy_descriptors(1, dsts.as_ptr(), dsizes.as_ptr(), 2, srcs.as_ptr(), sizes.as_ptr(), 0) };
    assert_eq!(record_at(gstart + 6 * 32), record_at(cstart));
    assert_eq!(record_at(gstart + 7 * 32), record_at(cstart + 32));
    // a CBV copied into a shader-visible heap is a raw SRV there, with no alternate (never a CBV)
    assert!(rig.fake.at(cst.host.cpu_at(0)).unwrap().describe().starts_with("cbv"), "a CPU-only heap keeps the CBV");
    let sh6 = st.shadow.as_ref().unwrap().read(6);
    assert!(rig.fake.at(st.host.cpu_at(6)).unwrap().is_raw_srv());
    assert_eq!(sh6[2], st.null_index);
    // and by the title's byte copy too
    unsafe { std::ptr::copy_nonoverlapping(cstart as *const u8, (gstart + 3 * 32) as *mut u8, 32) };
    rig.r.sync(gstart, 8 * 32);
    assert!(rig.fake.at(st.host.cpu_at(3)).unwrap().is_raw_srv());
    assert_eq!(st.shadow.as_ref().unwrap().read(3)[2], st.null_index);
    for o in [b, cpu_heap, gpu_heap] {
        release(o);
    }
}

#[test]
fn a_heap_over_records_mirrors_them_when_made() {
    let rig = rig();
    let b = buffer(&rig.r, KIB_64);
    let base = rig.r.console_resource(b).unwrap().address;
    let bytes = 4 * 32;
    let mem = rig.mem.allocate(bytes, KIB_64, &UPLOAD).unwrap();
    unsafe { (mem as *mut [u32; 8]).add(3).write(Record::Cbv { va: base, size: 256 }.encode()) };
    let e = Ext([0, 4, 1, 0], mem, bytes);
    let mut h: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { rig.r.create_descriptor_heap(&e.0, &ID3D12DescriptorHeap::IID, &mut h) }, S_OK);
    let st = rig.r.heap_state(h).unwrap();
    assert!(rig.fake.at(st.host.cpu_at(3)).unwrap().is_raw_srv());
    assert!(rig.fake.at(st.host.cpu_at(0)).is_none(), "zeroed slots need no work: the host heap starts null");
    release(b);
    release(h);
}

// ---- the oversized heap ----

/// A console index past the mirrored range (the title's heap is 1,245,184 views, the host's 1,000,000) must
/// never reach an alternate or a window view: translated shaders send it to the null bank, which holds a null
/// of every declared view type, and alternates live only after the bank and below the windows (or past
/// 1,000,000 on a larger heap).
#[test]
fn console_indices_and_alternates_never_share_host_slots() {
    use crate::heap::WINDOW_UAV_BASE;
    use rebuild_sir::lift::{NullKind, CONSOLE_VIEWS, NULL_BANK, WINDOW_COUNT, WINDOW_HEAP_BASE};
    assert_eq!(Limits::TIER2.null_index(), CONSOLE_VIEWS, "the index translated shaders compare with");
    assert_eq!(WINDOW_UAV_BASE + WINDOW_COUNT, WINDOW_HEAP_BASE, "window UAVs right below the window SRVs");
    assert_eq!(Limits::TIER2.alternate_ranges(), [CONSOLE_VIEWS + NULL_BANK..WINDOW_UAV_BASE, 1_000_000..1_000_000]);
    let big = Limits { views: 1 << 21, samplers: 2048 };
    assert_eq!(big.null_index(), CONSOLE_VIEWS);
    assert_eq!(big.alternate_ranges(), [CONSOLE_VIEWS + NULL_BANK..WINDOW_UAV_BASE, 1_000_000..1 << 21]);

    let rig = rig_with(Limits::TIER2, 64 << 20);
    let r = &rig.r;
    let b = buffer(r, KIB_64);
    let ok = |a: u32| (CONSOLE_VIEWS + NULL_BANK..WINDOW_UAV_BASE).contains(&a);
    // the title's oversized heap (pooled alternates) and a small one (fixed alternates)
    for count in [1_245_184u32, 64] {
        let (h, start) = heap(&rig, 0, count, true, true);
        let st = r.heap_state(h).unwrap();
        assert_eq!(st.null_index, CONSOLE_VIEWS);
        // the null bank: one typed null per kind
        for k in NullKind::all() {
            let v = rig.fake.at(st.host.cpu_at(CONSOLE_VIEWS + k.slot())).unwrap();
            assert_eq!(v.describe(), crate::heap::null_view(k).describe(), "{k:?}");
        }
        assert!(rig.fake.at(st.host.cpu_at(CONSOLE_VIEWS + 2)).unwrap().describe().starts_with("srv typed 0x0 fmt 2"));
        assert!(rig.fake.at(st.host.cpu_at(CONSOLE_VIEWS + 17)).unwrap().describe().starts_with("uav texture 0x0 fmt 42 dim 4"));
        // what a shader past the range reads from the shadow table: no stride, no records, the bank
        assert_eq!(st.shadow.as_ref().unwrap().read(CONSOLE_VIEWS), [0, 0, CONSOLE_VIEWS, 0]);
        // typed SRVs and UAVs at the first and last mirrored indices: their alternates are past the bank
        let last = count.min(CONSOLE_VIEWS) - 1;
        for i in [0, 1, last] {
            unsafe {
                if i % 2 == 0 {
                    r.create_srv(b, &srv_buffer(0, 16, 0, false, DXGI_FORMAT_R32G32B32A32_FLOAT), start + 32 * u64::from(i));
                } else {
                    r.create_uav(b, std::ptr::null_mut(), &uav_buffer(0, 16, 0, true, DXGI_FORMAT_R32_TYPELESS), start + 32 * u64::from(i));
                }
            }
            let a = st.shadow.as_ref().unwrap().read(i)[2];
            assert!(ok(a), "count {count}, index {i}: alternate {a} is where a console index or a window can reach");
        }
        release(h);
    }
    release(b);
}

#[test]
fn the_titles_heap_of_1245184_views_keeps_indices_and_counts_what_does_not_fit() {
    let rig = rig_with(Limits::TIER2, 64 << 20);
    let r = &rig.r;
    let p = r.view_index_limit();
    assert_eq!(p, 750_000);
    let b = buffer(r, KIB_64);
    let (h, start) = heap(&rig, 0, 1_245_184, true, true);
    let st = r.heap_state(h).unwrap();
    assert_eq!((st.mirrored, st.host.count, st.null_index), (750_000, 1_000_000, 750_000));
    let binding = r.binding(h).unwrap();
    assert_eq!(binding.console_bytes, 750_000 * 32, "the list translator maps only mirrored indices");
    assert_eq!(binding.console_stride, 32);
    // a typed SRV has an alternate (a CBV has none: it is only its raw SRV)
    let typed = |i: u64| unsafe { r.create_srv(b, &srv_buffer(0, 16, 0, false, DXGI_FORMAT_R32G32B32A32_FLOAT), start + 32 * i) };
    typed(0);
    typed(720_000);
    let a0 = st.shadow.as_ref().unwrap().read(0)[2];
    let a1 = st.shadow.as_ref().unwrap().read(720_000)[2];
    assert!(a0 > p && a1 > p && a0 != a1, "pooled alternates");
    assert!(rig.fake.at(st.host.cpu_at(720_000)).unwrap().is_raw_srv(), "index kept: console 720,000 is host 720,000");
    // an index past the mirror: recorded in the title's memory, counted and noted, never mirrored
    typed(800_000);
    assert!(record_at(start + 32 * 800_000).is_some());
    assert_eq!(st.stats.beyond.load(Ordering::Relaxed), 1);
    assert_eq!(st.stats.highest.load(Ordering::Relaxed), 800_000);
    let notes = rig.notes.0.lock().unwrap().clone();
    assert!(notes.contains(&("descriptor index past the host heap", 800_000)));
    assert!(notes.iter().any(|n| n.0 == "console heap larger than the host mirrors"));
    // overwriting a slot with a view that needs no alternate gives its alternate back
    unsafe { r.create_srv(b, &srv_buffer(0, 4, 0, true, DXGI_FORMAT_R32_TYPELESS), start) };
    assert_eq!(st.shadow.as_ref().unwrap().read(0)[2], p);
    typed(1);
    assert_eq!(st.shadow.as_ref().unwrap().read(1)[2], a0, "the freed alternate is reused");
    release(b);
    release(h);
}

#[test]
fn the_list_host_answers_through_resources() {
    use rebuild_xlist::console::{HostPipeline, ListHost, PipelineSource};
    struct NoPipelines;
    impl PipelineSource for NoPipelines {
        fn pipeline(&self, _: Raw) -> Option<HostPipeline> {
            None
        }
    }
    let rig = rig();
    let b = buffer(&rig.r, KIB_64);
    let (h, start) = heap(&rig, 0, 8, true, true);
    let host = Host { resources: rig.r.clone(), pipelines: NoPipelines };
    assert_eq!(host.resource(b), None, "no host buffer in the stand-in");
    let hb = host.descriptor_heap(h).unwrap();
    assert_eq!((hb.console_base, hb.console_stride, hb.host_stride), (start, 32, 1));
    let base = rig.r.console_resource(b).unwrap().address;
    let m = host.memory(base + 8).unwrap();
    assert_eq!(m.map(base + 8), Some(base + 8));
    assert!(host.descriptor_heap(b).is_none(), "a resource is no heap");
    release(b);
    release(h);
}

// ---- rates on the stand-in (CPU work only) ----

#[test]
fn translation_rate_on_the_stand_in() {
    let rig = rig_with(Limits { views: 1 << 18, samplers: 64 }, 64 << 20);
    let b = buffer(&rig.r, KIB_64);
    let base = rig.r.console_resource(b).unwrap().address;
    let n = 20_000u64;
    let (h, start) = heap(&rig, 0, n as u32, true, true);
    let t = Instant::now();
    for i in 0..n {
        unsafe { rig.r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: base + (i % 200) * 256, SizeInBytes: 256 }, start + 32 * i) };
    }
    let us = t.elapsed().as_secs_f64() * 1e6;
    println!("stand-in: {n} CBVs created and mirrored in {us:.0} us = {:.2} a us (record, plan, two views into a locked map, shadow)", n as f64 / us);
    let t = Instant::now();
    rig.r.sync(start, n * 32);
    let us2 = t.elapsed().as_secs_f64() * 1e6;
    println!("stand-in: sync of {n} unchanged slots in {us2:.0} us = {:.1} a us", n as f64 / us2);
    release(b);
    release(h);
}

// ---- on the card (by hand, while agents\MEASURING is absent) ----

fn measuring() -> bool {
    let lock = std::path::Path::new(r"D:\Developer\triumph-rebuild\agents\MEASURING");
    if lock.exists() {
        println!("skipped: agents\\MEASURING is present (the lead is timing)");
        return true;
    }
    false
}

fn device() -> ID3D12Device {
    let mut d: Option<ID3D12Device> = None;
    unsafe { D3D12CreateDevice(None::<&windows::core::IUnknown>, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0, &mut d) }.expect("device");
    d.unwrap()
}

fn dxc() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("REBUILD_DXC") {
        return Some(p.into());
    }
    let root = std::path::PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut v: Vec<_> = std::fs::read_dir(root).ok()?.filter_map(|e| e.ok().map(|e| e.path().join("x64").join("dxc.exe"))).filter(|p| p.exists()).collect();
    v.sort();
    v.pop()
}

/// The slots the kernel reads, through the heap and the shadow table, as translated programs
/// do: raw loads at the console index (from the lead the shadow table gives), the typed view
/// and the constant buffer at their alternates.
const KERNEL: &str = r#"
ByteAddressBuffer shadow : register(t0, space101);
RWByteAddressBuffer result : register(u0);
cbuffer Root : register(b0) { uint rawIndex; uint typedIndex; uint cbvIndex; uint pad; };
struct Cb { uint4 v[4]; };
[numthreads(1, 1, 1)]
void main() {
    ByteAddressBuffer raw = ResourceDescriptorHeap[rawIndex];
    uint4 sh = shadow.Load4(rawIndex * 16u);
    result.Store4(0, raw.Load4(sh.w));
    ByteAddressBuffer typedRaw = ResourceDescriptorHeap[typedIndex];
    uint4 st = shadow.Load4(typedIndex * 16u);
    result.Store(16, typedRaw.Load(st.w + 4u));
    Buffer<float4> typed = ResourceDescriptorHeap[st.z];
    result.Store(20, asuint(typed[1].x));
    result.Store4(24, st);
    uint4 sc = shadow.Load4(cbvIndex * 16u);
    ConstantBuffer<Cb> cb = ResourceDescriptorHeap[sc.z];
    result.Store(40, cb.v[1].y);
    ByteAddressBuffer cbRaw = ResourceDescriptorHeap[cbvIndex];
    result.Store(44, cbRaw.Load(sc.w + 20u));
}
"#;

#[test]
#[ignore = "GPU: by hand, while agents\\MEASURING is absent"]
fn card_reads_a_console_buffer_through_its_views_in_the_heap() {
    if measuring() {
        return;
    }
    let Some(dxc) = dxc() else {
        println!("skipped: no dxc.exe");
        return;
    };
    let device = device();
    let limits = Limits::probe(&device);
    println!("host limits: {limits:?} (null index {})", limits.null_index());
    let mem = Arc::new(HostMemory::new(&device, 16 << 20).unwrap());
    let r = Resources::new(&device, mem.clone(), Arc::new(Quiet)).unwrap();
    unsafe {
        // the console path: a buffer, written by the title through Map
        let mut buf: Raw = std::ptr::null_mut();
        let d = crate::resource::buffer_desc(KIB_64, false);
        assert_eq!(r.create_committed(&UPLOAD, 0, &d, 0, std::ptr::null(), &ID3D12Resource::IID, &mut buf), S_OK);
        assert!(r.host_resource(buf).is_some(), "a buffer has the model's host buffer at its address");
        type Map = unsafe extern "system" fn(Raw, u32, *const std::ffi::c_void, *mut u64) -> HRESULT;
        let mut p = 0u64;
        assert_eq!(method::<Map>(buf, 0x40)(buf, 0, std::ptr::null(), &mut p), S_OK);
        let words = p as *mut u32;
        for i in 0..1024u32 {
            words.add(i as usize).write(0xA000_0000 + i);
        }
        let floats = (p + 4096) as *mut f32;
        for i in 0..16 {
            floats.add(i).write(i as f32 * 1.5);
        }
        let cb = (p + 8192) as *mut u32;
        for i in 0..64 {
            cb.add(i).write(0xC000 + i as u32);
        }
        // a shader-visible view heap, and views made through the console calls
        let e = Ext([0, 64, 1, 0], 0, 0);
        let mut heap: Raw = std::ptr::null_mut();
        assert_eq!(r.create_descriptor_heap(&e.0, &ID3D12DescriptorHeap::IID, &mut heap), S_OK);
        let start = r.heap_state(heap).unwrap().start;
        // slot 3: raw SRV from byte 36 (not 16-aligned: the lead is 4)
        r.create_srv(buf, &srv_buffer(9, 32, 0, true, DXGI_FORMAT_R32_TYPELESS), start + 3 * 32);
        // slot 7: typed float4 SRV from byte 4096
        r.create_srv(buf, &srv_buffer(256, 4, 0, false, DXGI_FORMAT_R32G32B32A32_FLOAT), start + 7 * 32);
        // slot 9: a CBV at byte 8192; the title copies it as bytes to slot 10, and syncs
        r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: p + 8192, SizeInBytes: 256 }, start + 9 * 32);
        std::ptr::copy_nonoverlapping((start + 9 * 32) as *const u8, (start + 10 * 32) as *mut u8, 32);
        r.sync(start + 10 * 32, 32);

        // the host side of the test: root signature, pipeline, output
        let dir = std::env::temp_dir().join(format!("d3d12x-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("k.hlsl"), KERNEL).unwrap();
        let out = std::process::Command::new(&dxc).args(["-T", "cs_6_6", "-E", "main", "-O3", "-Fo"]).arg(dir.join("k.dxil")).arg(dir.join("k.hlsl")).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let code = std::fs::read(dir.join("k.dxil")).unwrap();
        let root_view = |kind, register, space| D3D12_ROOT_PARAMETER1 {
            ParameterType: kind,
            Anonymous: D3D12_ROOT_PARAMETER1_0 { Descriptor: D3D12_ROOT_DESCRIPTOR1 { ShaderRegister: register, RegisterSpace: space, Flags: D3D12_ROOT_DESCRIPTOR_FLAG_NONE } },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let params = [
            root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0, 101),
            root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0, 0),
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 { Constants: D3D12_ROOT_CONSTANTS { ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 4 } },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];
        let rs = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 { NumParameters: 3, pParameters: params.as_ptr(), Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED, ..Default::default() },
            },
        };
        let mut blob = None;
        D3D12SerializeVersionedRootSignature(&rs, &mut blob, None).unwrap();
        let blob = blob.unwrap();
        let root: ID3D12RootSignature = device.CreateRootSignature(0, std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())).unwrap();
        let pso: ID3D12PipelineState = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            })
            .unwrap();
        let mut result: Option<ID3D12Resource> = None;
        let rd = crate::resource::buffer_desc(256, true);
        device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &rd, D3D12_RESOURCE_STATE_COMMON, None, &mut result).unwrap();
        let result = result.unwrap();
        let mut back: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &crate::resource::buffer_desc(256, false), D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)
            .unwrap();
        let back = back.unwrap();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
        let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso).unwrap();
        // what the list translator binds for the console heap
        let binding = r.binding(heap).unwrap();
        let host_heap = ID3D12DescriptorHeap::from_raw_borrowed(&binding.heap).unwrap().clone();
        list.SetDescriptorHeaps(&[Some(host_heap)]);
        list.SetComputeRootSignature(&root);
        list.SetComputeRootShaderResourceView(0, r.shadow_va(heap).unwrap());
        list.SetComputeRootUnorderedAccessView(1, result.GetGPUVirtualAddress());
        let consts = [3u32, 7, 10, 0];
        list.SetComputeRoot32BitConstants(2, 4, consts.as_ptr().cast(), 0);
        list.Dispatch(1, 1, 1);
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::transmute_copy(&result),
                    Subresource: 0,
                    StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                }),
            },
            ..Default::default()
        };
        list.ResourceBarrier(&[barrier]);
        list.CopyResource(&back, &result);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        let t = Instant::now();
        while fence.GetCompletedValue() < 1 {
            assert!(t.elapsed().as_secs() < 10, "GPU did not finish");
            std::thread::yield_now();
        }
        if let Err(e) = device.GetDeviceRemovedReason() {
            panic!("device removed: {e}");
        }
        let mut rp: *mut std::ffi::c_void = std::ptr::null_mut();
        back.Map(0, None, Some(&mut rp)).unwrap();
        let got = std::slice::from_raw_parts(rp.cast::<u32>(), 12).to_vec();
        println!("read back: {got:x?}");
        assert_eq!(&got[0..4], &[0xA000_0009, 0xA000_000A, 0xA000_000B, 0xA000_000C], "raw SRV at the console index, from byte 36");
        assert_eq!(got[4], 1.5f32.to_bits(), "the typed view's bytes, read raw at its console index");
        assert_eq!(got[5], 6.0f32.to_bits(), "the typed float4 view at its alternate: element 1.x");
        assert_eq!(&got[6..8], &[16, 4], "shadow (stride, records)");
        assert_eq!(got[9], 0, "lead 0 at byte 4096");
        assert_eq!(got[10], 0xC005, "the CBV copied as bytes, at its alternate: v[1].y");
        assert_eq!(got[11], 0xC005, "and read raw at its console index");
        let _ = std::fs::remove_dir_all(&dir);
        release(buf);
        release(heap);
    }
}

#[test]
#[ignore = "GPU: by hand, while agents\\MEASURING is absent"]
fn card_descriptor_translation_rate() {
    if measuring() {
        return;
    }
    let device = device();
    let mem = Arc::new(HostMemory::new(&device, 64 << 20).unwrap());
    let r = Resources::new(&device, mem.clone(), Arc::new(Quiet)).unwrap();
    let limits = r.limits();
    unsafe {
        let mut buf: Raw = std::ptr::null_mut();
        assert_eq!(r.create_committed(&UPLOAD, 0, &crate::resource::buffer_desc(16 << 20, false), 0, std::ptr::null(), &ID3D12Resource::IID, &mut buf), S_OK);
        let base = r.console_resource(buf).unwrap().address;
        // the title's heap: 1,245,184 views
        let e = Ext([0, 1_245_184, 1, 0], 0, 0);
        let mut heap: Raw = std::ptr::null_mut();
        let t = Instant::now();
        assert_eq!(r.create_descriptor_heap(&e.0, &ID3D12DescriptorHeap::IID, &mut heap), S_OK);
        let st = r.heap_state(heap).unwrap();
        println!(
            "title view heap of 1,245,184: host heap of {} made and null-filled, with its shadow table, in {:.1} ms; {} indices mirrored, null at {}",
            limits.views,
            t.elapsed().as_secs_f64() * 1e3,
            st.mirrored,
            limits.null_index()
        );
        let start = st.start;
        let n = 100_000u64;
        for (name, kind) in [("CBV", 0), ("raw SRV", 1), ("typed SRV", 2), ("raw UAV", 3)] {
            let t = Instant::now();
            for i in 0..n {
                let h = start + 32 * i;
                let at = (i % 4096) * 256;
                match kind {
                    0 => r.create_cbv(&D3D12_CONSTANT_BUFFER_VIEW_DESC { BufferLocation: base + at, SizeInBytes: 256 }, h),
                    1 => r.create_srv(buf, &srv_buffer(at / 4, 64, 0, true, DXGI_FORMAT_R32_TYPELESS), h),
                    2 => r.create_srv(buf, &srv_buffer(at / 16, 16, 0, false, DXGI_FORMAT_R32G32B32A32_FLOAT), h),
                    _ => r.create_uav(buf, std::ptr::null_mut(), &uav_buffer(at / 4, 64, 0, true, DXGI_FORMAT_R32_TYPELESS), h),
                }
            }
            let us = t.elapsed().as_secs_f64() * 1e6;
            println!("{name:>9}: {n} created and mirrored in {:.1} ms = {:.2} a us ({:.0} ns each)", us / 1e3, n as f64 / us, us * 1e3 / n as f64);
        }
        // the title's byte copies: 100,000 records moved, then synced
        std::ptr::copy(start as *const u8, (start + 32 * n) as *mut u8, (32 * n) as usize);
        let t = Instant::now();
        r.sync(start + 32 * n, 32 * n);
        let us = t.elapsed().as_secs_f64() * 1e6;
        println!("     sync: {n} copied records mirrored in {:.1} ms = {:.2} a us", us / 1e3, n as f64 / us);
        let t = Instant::now();
        r.sync(start, 32 * 2 * n);
        let us = t.elapsed().as_secs_f64() * 1e6;
        println!("     sync: {} unchanged slots looked at in {:.2} ms = {:.1} a us", 2 * n, us / 1e3, 2.0 * n as f64 / us);
        // what the host's own calls cost, for comparison: one view written straight into a
        // shader-visible heap, n times
        let host = crate::views::DeviceHost::new(&device).unwrap();
        let hh = host.heap(0, n as u32 * 2, true).unwrap();
        let whole = r.memory(base).unwrap();
        let t = Instant::now();
        for i in 0..n as u32 {
            host.write(hh.cpu_at(i), &HostView::Cbv { va: whole.host_va + u64::from(i % 4096) * 256, size: 256 });
        }
        let cbv_ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        let t = Instant::now();
        for i in 0..n as u32 {
            host.write(hh.cpu_at(i), &HostView::raw_srv(whole.resource, u64::from(i % 4096) * 64, 64));
        }
        let srv_ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        let shadow = host.shadow(n as u32).unwrap();
        let t = Instant::now();
        for i in 0..n as u32 {
            shadow.write(i, [1, 256, i, 0]);
        }
        let sh_ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        println!("     host: CreateConstantBufferView {cbv_ns:.0} ns, raw CreateShaderResourceView {srv_ns:.0} ns, a shadow entry {sh_ns:.1} ns (shader-visible heap; {})", if host.gpu_upload { "shadow in GPU upload memory" } else { "shadow in upload memory" });
        if let Err(e) = device.GetDeviceRemovedReason() {
            panic!("device removed: {e}");
        }
        release(buf);
        release(heap);
    }
}

#[test]
#[ignore = "GPU: by hand, while agents\\MEASURING is absent"]
fn card_profile_pieces() {
    if measuring() {
        return;
    }
    let device = device();
    let mem = Arc::new(HostMemory::new(&device, 64 << 20).unwrap());
    let r = Resources::new(&device, mem.clone(), Arc::new(Quiet)).unwrap();
    unsafe {
        let mut buf: Raw = std::ptr::null_mut();
        assert_eq!(r.create_committed(&UPLOAD, 0, &crate::resource::buffer_desc(16 << 20, false), 0, std::ptr::null(), &ID3D12Resource::IID, &mut buf), S_OK);
        let e = Ext([0, 200_000, 1, 0], 0, 0);
        let mut heap: Raw = std::ptr::null_mut();
        assert_eq!(r.create_descriptor_heap(&e.0, &ID3D12DescriptorHeap::IID, &mut heap), S_OK);
        let st = r.heap_state(heap).unwrap();
        let n = 100_000u64;
        let t = Instant::now();
        for i in 0..n {
            std::hint::black_box(r.inner.heap_at(st.start + 32 * i));
        }
        println!("heap_at {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let w = Record::Buffer { uav: false, va: r.console_resource(buf).unwrap().address, format: 39, elements: 64, stride: 0, flags: RAW }.encode();
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(r.inner.plan(std::hint::black_box(&w), &st));
        }
        println!("plan {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let plan = r.inner.plan(&w, &st);
        let t = Instant::now();
        for i in 0..n as u32 {
            r.inner.host.write(st.host.cpu_at(i), &plan.primary);
        }
        println!("write primary {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let t = Instant::now();
        for i in 0..n as u32 {
            r.inner.host.write(st.host.cpu_at(i), &plan.primary);
        }
        println!("write primary again {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let t = Instant::now();
        for i in 0..n as u32 {
            r.inner.write_view(st.host.cpu_at(i), &plan.primary);
        }
        println!("write_view (cached) {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let sh = st.shadow.as_ref().unwrap();
        let t = Instant::now();
        for i in 0..n as u32 {
            sh.write(i, [1, 2, 3, 4]);
        }
        println!("shadow write {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let t = Instant::now();
        for i in 0..n {
            std::hint::black_box(((st.start + 32 * i) as *const [u32; 8]).read_volatile());
        }
        println!("record read {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        for i in 0..n {
            ((st.start + 32 * i) as *mut [u32; 8]).write(w);
        }
        let t = Instant::now();
        for i in 0..n as u32 {
            r.inner.mirror(&st, i, true);
        }
        println!("mirror {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let d = srv_buffer(0, 64, 0, true, DXGI_FORMAT_R32_TYPELESS);
        let t = Instant::now();
        for i in 0..n {
            r.create_srv(buf, &d, st.start + 32 * i);
        }
        println!("create_srv {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let t = Instant::now();
        for i in 0..n {
            r.create_srv(buf, &d, st.start + 32 * i);
        }
        println!("create_srv again {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let stage: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 4096, ..Default::default() }).unwrap();
        let sbase = stage.GetCPUDescriptorHandleForHeapStart().ptr;
        let inc = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        let t = Instant::now();
        for i in 0..n as u32 {
            device.CopyDescriptorsSimple(1, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: st.host.cpu_at(i) }, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: sbase + (i as usize % 4096) * inc }, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        }
        println!("CopyDescriptorsSimple(1) {:.1} ns", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        release(buf);
        release(heap);
    }
}

#[test]
fn a_root_signature_keeps_its_blob_and_answers_as_a_device_child() {
    // the stand-in host: an object with no host root signature
    let rig = rig();
    let blob = [0u8; 24];
    let mut out: Raw = std::ptr::null_mut();
    unsafe {
        assert_eq!(rig.r.create_root_signature(blob.as_ptr(), blob.len(), &ID3D12RootSignature::IID, &mut out), S_OK);
        assert_eq!(rig.r.create_root_signature(std::ptr::null(), 0, &ID3D12RootSignature::IID, &mut out.clone()), windows::Win32::Foundation::E_INVALIDARG);
    }
    assert_eq!(crate::root::of(out).map(|r| r.blob.len()), Some(24));
    assert!(rig.r.host_root_signature(out).is_none());
    type Qi = unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT;
    let mut again: Raw = std::ptr::null_mut();
    assert_eq!(unsafe { method::<Qi>(out, 0)(out, &ID3D12DeviceChild::IID, &mut again) }, S_OK);
    assert_eq!(again, out);
    assert_eq!(release(again), 1);
    assert_eq!(release(out), 0);

    // a real host: the blob becomes an ID3D12RootSignature the lists can bind
    let mut d: Option<ID3D12Device> = None;
    if unsafe { D3D12CreateDevice(None::<&windows::core::IUnknown>, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0, &mut d) }.is_err() {
        println!("skipped: no D3D12 device");
        return;
    }
    let device = d.unwrap();
    let r = Resources::new(&device, Arc::new(PlainMemory::new(1 << 20)), Arc::new(Quiet)).unwrap();
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC { Version: D3D_ROOT_SIGNATURE_VERSION_1_1, ..Default::default() };
    let mut sb: Option<windows::Win32::Graphics::Direct3D::ID3DBlob> = None;
    unsafe { D3D12SerializeVersionedRootSignature(&desc, &mut sb, None) }.unwrap();
    let sb = sb.unwrap();
    let mut rs: Raw = std::ptr::null_mut();
    unsafe {
        let bytes = std::slice::from_raw_parts(sb.GetBufferPointer() as *const u8, sb.GetBufferSize());
        assert_eq!(r.create_root_signature(bytes.as_ptr(), bytes.len(), &ID3D12RootSignature::IID, &mut rs), S_OK);
        // a blob the host refuses fails as the host does
        let mut bad: Raw = std::ptr::null_mut();
        assert!(r.create_root_signature(blob.as_ptr(), blob.len(), &ID3D12RootSignature::IID, &mut bad).is_err());
        assert!(bad.is_null());
    }
    assert!(r.host_root_signature(rs).is_some());
    assert_eq!(release(rs), 0);
}

#[test]
fn query_heaps_and_command_signatures_are_device_children_with_host_objects() {
    let qd = D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 4, NodeMask: 0 };
    let arg = D3D12_INDIRECT_ARGUMENT_DESC { Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH, ..Default::default() };
    let cd = D3D12_COMMAND_SIGNATURE_DESC { ByteStride: 12, NumArgumentDescs: 1, pArgumentDescs: &arg, NodeMask: 0 };
    // the stand-in host: valid objects, no host side
    let rig = rig();
    let (mut q, mut c): (Raw, Raw) = (std::ptr::null_mut(), std::ptr::null_mut());
    unsafe {
        assert_eq!(rig.r.create_query_heap(&qd, &ID3D12QueryHeap::IID, &mut q), S_OK);
        assert_eq!(rig.r.create_command_signature(&cd, std::ptr::null_mut(), &ID3D12CommandSignature::IID, &mut c), S_OK);
    }
    assert_eq!(crate::signature::query_heap_of(q).map(|h| h.desc.Count), Some(4));
    assert_eq!(crate::signature::of(c).map(|s| (s.stride, s.arguments.len())), Some((12, 1)));
    assert_eq!(crate::signature::of(c).map(|s| (s.shape.side(), s.shape.changes_root(), s.host_root.is_none())), Some((Some(1), false, true)), "a dispatch: no root named");
    assert!(rig.r.host_command_signature(c).is_none());
    assert_eq!((release(q), release(c)), (0, 0));

    let mut d: Option<ID3D12Device> = None;
    if unsafe { D3D12CreateDevice(None::<&windows::core::IUnknown>, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0, &mut d) }.is_err() {
        println!("skipped: no D3D12 device");
        return;
    }
    let r = Resources::new(&d.unwrap(), Arc::new(PlainMemory::new(1 << 20)), Arc::new(Quiet)).unwrap();
    unsafe {
        assert_eq!(r.create_query_heap(&qd, &ID3D12QueryHeap::IID, &mut q), S_OK);
        assert_eq!(r.create_command_signature(&cd, std::ptr::null_mut(), &ID3D12CommandSignature::IID, &mut c), S_OK);
    }
    assert!(r.host_query_heap(q).is_some());
    assert!(r.host_command_signature(c).is_some());
    assert_eq!((release(q), release(c)), (0, 0));
}

/// The window contract (`docs/design/memory-run.md`): sub-window k (console addresses k << 29, 512 MiB) is a raw
/// SRV at `WINDOW_HEAP_BASE + k` and a raw UAV at `WINDOW_UAV_BASE + k` over 2^27 elements from byte 0, in every
/// shader-visible view heap, the ones made before it was placed and after.
#[test]
fn window_views_are_placed_in_every_visible_heap() {
    use crate::heap::{WINDOW_ELEMENTS, WINDOW_UAV_BASE};
    use rebuild_sir::lift::WINDOW_HEAP_BASE;
    let Ok(factory) = (unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1::<windows::Win32::Graphics::Dxgi::IDXGIFactory4>() }) else { return };
    let Ok(adapter) = (unsafe { factory.EnumWarpAdapter::<windows::core::IUnknown>() }) else { return };
    let mut device: Option<ID3D12Device> = None;
    if unsafe { D3D12CreateDevice(&adapter, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut device) }.is_err() {
        return;
    }
    let device = device.unwrap();
    let mut buf: Option<ID3D12Resource> = None;
    unsafe {
        device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &crate::resource::buffer_desc(KIB_64, true), D3D12_RESOURCE_STATE_COMMON, None, &mut buf)
            .unwrap()
    };
    let buf = buf.unwrap();
    let rig = rig_with(Limits::TIER2, 64 << 20);
    let r = &rig.r;
    let (before, _) = heap(&rig, 0, 1_245_184, true, true);
    r.place_window(9, &buf);
    let (after, _) = heap(&rig, 0, 64, true, true);
    let (cpu_only, _) = heap(&rig, 0, 64, false, false);
    let raw = buf.as_raw();
    for h in [before, after] {
        let st = r.heap_state(h).unwrap();
        let srv = rig.fake.at(st.host.cpu_at(WINDOW_HEAP_BASE + 9)).unwrap().describe();
        let uav = rig.fake.at(st.host.cpu_at(WINDOW_UAV_BASE + 9)).unwrap().describe();
        assert_eq!(srv, format!("srv raw {raw:?} fmt {} first 0 n {WINDOW_ELEMENTS}", DXGI_FORMAT_R32_TYPELESS.0));
        assert_eq!(uav, format!("uav raw {raw:?} fmt {} first 0 n {WINDOW_ELEMENTS}", DXGI_FORMAT_R32_TYPELESS.0));
        assert!(rig.fake.at(st.host.cpu_at(WINDOW_HEAP_BASE + 10)).is_none_or(|v| !v.describe().contains("raw")), "an unplaced sub-window stays null");
    }
    let st = r.heap_state(cpu_only).unwrap();
    assert!(st.host.count < 1_000_000, "a CPU-only heap has no window range");
    assert_eq!(r.windows_placed(), 1);
}

