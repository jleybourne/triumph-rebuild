//! Tests: the slot table against `agents/d3d12x-child-slots.csv`, the layout, every translated
//! path on a counting stand-in (no GPU), and, by hand while `agents\MEASURING` is absent, the same
//! objects recording on the card:
//!
//! ```text
//! cargo test -p rebuild-xlist --release -- --ignored --test-threads=1 card_records_a_clear_a_barrier
//! cargo test -p rebuild-xlist --release -- --ignored --nocapture card_cost_per_draw
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::{BOOL, E_NOINTERFACE, E_NOTIMPL, RECT, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::fake::{self, host, Fake};
use super::*;

// ---- helpers ----

/// The function in `offset` of `obj`'s table, as title code loads it.
fn method<F: Copy>(obj: Raw, offset: usize) -> F {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    unsafe {
        let table = *obj.cast::<*const *const ()>();
        std::mem::transmute_copy(&*table.add(offset / 8))
    }
}

fn read<T: Copy>(obj: Raw, offset: usize) -> T {
    unsafe { obj.cast::<u8>().add(offset).cast::<T>().read_unaligned() }
}

type Hr0 = unsafe extern "system" fn(Raw) -> HRESULT;
type U0 = unsafe extern "system" fn(Raw) -> u32;
type V0 = unsafe extern "system" fn(Raw) -> u64;

fn release(obj: Raw) -> u32 {
    unsafe { method::<U0>(obj, 0x10)(obj) }
}

#[derive(Default)]
struct TestHost {
    traps: Mutex<Vec<(&'static str, usize)>>,
    waits: Mutex<Vec<MemoryWait>>,
    labels: Mutex<Vec<(LabelWrite, Option<u64>)>>,
}

const HEAP: Raw = 0xD000 as Raw;
const BAD: Raw = 0xBAD as Raw;
const MEM: u64 = 0x2_0000_0000;
const MEM_HOST: u64 = 0x7_0000_0000;

impl PipelineSource for TestHost {
    fn pipeline(&self, console: Raw) -> Option<HostPipeline> {
        (console != BAD).then_some(HostPipeline { pso: (console as usize | 0x8000_0000) as Raw, root_signature: 0x5160 as Raw, compute: false })
    }
}

impl ListHost for TestHost {
    fn resource(&self, console: Raw) -> Option<Raw> {
        (console != BAD).then_some((console as usize ^ 0x1_0000) as Raw)
    }
    fn descriptor_heap(&self, console: Raw) -> Option<HeapBinding> {
        (console == HEAP).then_some(HeapBinding { heap: 0x4EA9 as Raw, console_base: 0x1000_0000, console_bytes: 1 << 20, console_stride: 32, host_base: 0x9_0000_0000, host_stride: 64 })
    }
    fn cpu_descriptor(&self, console: usize) -> Option<usize> {
        (console != 0xBAD).then_some(console + 0x100)
    }
    fn command_signature(&self, console: Raw) -> Option<Raw> {
        Some(console)
    }
    fn memory(&self, va: u64) -> Option<MemoryRange> {
        let r = MemoryRange { console: MEM, bytes: 1 << 30, host_va: MEM_HOST, resource: 0x77 as Raw, resource_offset: 0x100 };
        r.map(va).map(|_| r)
    }
    fn label(&self, list: &ID3D12GraphicsCommandList2, w: &LabelWrite, host_va: Option<u64>) -> bool {
        self.labels.lock().unwrap().push((*w, host_va));
        // and the default, so the host call is seen
        let (LabelValue::Value(v), Some(va)) = (w.value, host_va) else { return false };
        let p = D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: va, Value: v as u32 };
        unsafe { (list.vtable().WriteBufferImmediate)(list.as_raw(), 1, &p, std::ptr::null()) };
        true
    }
    fn memory_wait(&self, w: &MemoryWait) -> WaitAction {
        self.waits.lock().unwrap().push(*w);
        WaitAction::Split
    }
    fn first_trap(&self, object: &'static str, offset: usize) {
        self.traps.lock().unwrap().push((object, offset));
    }
}

fn fake_list(host: Arc<TestHost>) -> (Raw, Raw, Arc<fake::FakeBackend>) {
    let backend = fake::backend();
    let a = alloc::create_on(0, backend.clone(), host).expect("allocator");
    let l = create_list(0, a, std::ptr::null_mut()).expect("list");
    (l, a, backend)
}

// ---- the table ----

/// Offsets in a CSV cell: `0x040`, `0x018-0x030` (every slot between), `0x238/0x240`.
fn offsets(cell: &str) -> Vec<usize> {
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).unwrap_or_else(|_| panic!("offset {s}"));
    if let Some((a, b)) = cell.split_once('-') {
        (hex(a)..=hex(b)).step_by(8).collect()
    } else {
        cell.split('/').map(hex).collect()
    }
}

/// CSV rows split on commas outside quotes.
fn rows(csv: &str) -> Vec<Vec<String>> {
    csv.lines()
        .skip(1)
        .map(|line| {
            let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
            for ch in line.chars() {
                match ch {
                    '"' => quoted = !quoted,
                    ',' if !quoted => out.push(std::mem::take(&mut cur)),
                    c => cur.push(c),
                }
            }
            out.push(cur);
            out
        })
        .collect()
}

#[test]
fn every_list_and_allocator_slot_in_the_csv_resolves() {
    let csv = include_str!("../../../../agents/d3d12x-child-slots.csv");
    let (mut list_rows, mut alloc_rows) = (0, 0);
    for r in rows(csv) {
        let (object, offset, returns) = (r[0].as_str(), r[1].as_str(), r[6].as_str());
        let tables: Vec<(&[SlotInfo], &str)> = match object {
            "list" => vec![(list_slots(), "list")],
            "allocator" => vec![(allocator_slots(), "allocator")],
            "all" | "all children" => vec![(list_slots(), "list"), (allocator_slots(), "allocator")],
            _ => continue,
        };
        for (table, name) in tables {
            for o in offsets(offset) {
                let slot = table.get(o / 8).unwrap_or_else(|| panic!("{name} {o:#x} past the table"));
                assert_eq!(slot.offset, o);
                match slot.kind {
                    SlotKind::Implemented(_) => {}
                    SlotKind::Trap(kind) => {
                        if returns == "HRESULT" {
                            assert_eq!(kind, Returns::Hresult, "{name} {o:#x} returns HRESULT: its trap must too");
                        } else {
                            assert_eq!(kind, Returns::Value, "{name} {o:#x} returns {returns}: not an HRESULT");
                        }
                    }
                }
                if name == "list" {
                    list_rows += 1;
                } else {
                    alloc_rows += 1;
                }
            }
        }
    }
    assert!(list_rows >= 70 && alloc_rows >= 10, "{list_rows} list, {alloc_rows} allocator offsets checked");
    assert_eq!(list_slots().len(), 160);
    assert_eq!(allocator_slots().len(), 24);

    // The research's first set (§6 step 8) is implemented, not trapped. 0x188 (SOSetTargets) is
    // inside its 0x100-0x1a0 span but not in the CSV: the title is not seen to call it.
    let mut first: Vec<usize> = vec![0x040, 0x048, 0x050, 0x0c8, 0x0d0, 0x238, 0x240, 0x248, 0x250, 0x418, 0x420, 0x458, 0x490, 0x4a0];
    first.extend((0x100..=0x1a0).step_by(8).filter(|&o| o != 0x188));
    for o in first {
        assert!(matches!(list_slots()[o / 8].kind, SlotKind::Implemented(_)), "list {o:#x} is in the first set");
    }
    for o in [0x40, 0x48] {
        assert!(matches!(allocator_slots()[o / 8].kind, SlotKind::Implemented(_)), "allocator {o:#x}");
    }
    // 0x4a0 returns memory, not an HRESULT, and nothing value-returning traps with E_NOTIMPL.
    for s in list_slots() {
        if let SlotKind::Trap(Returns::Hresult) = s.kind {
            assert!([0x28].contains(&s.offset), "list {:#x} traps with an HRESULT", s.offset);
        }
    }
    let implemented = list_slots().iter().filter(|s| matches!(s.kind, SlotKind::Implemented(_))).count();
    println!("list: {implemented} of 160 slots implemented, {} trapped", 160 - implemented);
}

#[test]
fn traps_count_and_fail_with_their_return_kind() {
    let h = Arc::new(TestHost::default());
    let (l, a, _) = fake_list(h.clone());
    // 0x058 ClearState (void) and 0x4a8 (past the known methods): value traps.
    let before = traps().iter().find(|t| t.object == "list" && t.offset == 0x58).map_or(0, |t| t.count);
    assert_eq!(unsafe { method::<V0>(l, 0x58)(l) }, 0);
    assert_eq!(unsafe { method::<V0>(l, 0x58)(l) }, 0);
    assert_eq!(unsafe { method::<V0>(l, 0x4f8)(l) }, 0);
    // 0x028 SetPrivateDataInterface returns an HRESULT: E_NOTIMPL.
    assert_eq!(unsafe { method::<Hr0>(l, 0x28)(l) }, E_NOTIMPL);
    // allocator 0x50, past its methods.
    assert_eq!(unsafe { method::<V0>(a, 0x50)(a) }, 0);
    let t = traps();
    let clear = t.iter().find(|t| t.object == "list" && t.offset == 0x58).expect("counted");
    assert_eq!(clear.count, before + 2);
    assert!(clear.first_ns > 0);
    assert!(t.iter().any(|t| t.object == "allocator" && t.offset == 0x50));
    // The host is told on the first call process-wide: whichever test's host made the first call.
    let seen = h.traps.lock().unwrap().clone();
    assert!(seen.iter().all(|(o, off)| ["list", "allocator"].contains(o) && off % 8 == 0));
    assert_eq!(release(l), 0);
    assert_eq!(release(a), 0);
}

// ---- memory as the title reads it ----

#[test]
fn the_list_is_laid_out_as_the_title_reads_it() {
    let (l, a, _) = fake_list(Arc::new(TestHost::default()));
    assert_eq!(read::<*const ()>(l, 0), super::slots::list_table().funcs.as_ptr().cast());
    assert_eq!(read::<u32>(l, 0x0c), 1, "one reference");
    assert_eq!(read::<u32>(l, 0x10), 0, "type");
    for o in (0x18..0x40).step_by(8) {
        assert_eq!(read::<u64>(l, o), 0, "put / limit at {o:#x} is zero, so every command is a call");
    }
    for o in (0x40..TITLE_BYTES).step_by(8) {
        assert_eq!(read::<u64>(l, o), 0, "title field {o:#x} starts zero");
    }
    assert!(size_of::<list::ListObject>() >= TITLE_BYTES);
    // The title writes its own fields: the pipeline packet copy at +0x544 and the newer fields past
    // +0x560 do not disturb us.
    unsafe { l.cast::<u8>().add(0x544).write_bytes(0xEE, TITLE_BYTES - 0x544) };
    assert_eq!(unsafe { method::<U0>(l, 0x40)(l) }, 0, "GetType still answers");
    assert_eq!(unsafe { method::<Hr0>(l, 0x48)(l) }, S_OK, "Close");

    // A compute list reads type 2 at +0x10; the allocator's type is at +0x10 too.
    let backend = fake::backend();
    let ca = alloc::create_on(2, backend, Arc::new(TestHost::default())).unwrap();
    let cl = create_list(2, ca, std::ptr::null_mut()).unwrap();
    assert_eq!(read::<u32>(cl, 0x10), 2);
    assert_eq!(read::<u32>(ca, 0x10), 2);
    assert_eq!(unsafe { method::<U0>(cl, 0x40)(cl) }, 2);
    for p in [cl, ca, l, a] {
        assert_eq!(release(p), 0);
    }
}

#[test]
fn query_interface_answers_the_list_ids_and_refuses_others() {
    let (l, a, _) = fake_list(Arc::new(TestHost::default()));
    type Qi = unsafe extern "system" fn(Raw, *const GUID, *mut Raw) -> HRESULT;
    let mut out = std::ptr::null_mut();
    let console_list6 = GUID::from_u128(0xc3827890_4024_474c_98cf_645b51dc4874);
    for iid in [console_list6, ID3D12GraphicsCommandList::IID, ID3D12CommandList::IID, windows::core::IUnknown::IID] {
        assert_eq!(unsafe { method::<Qi>(l, 0)(l, &iid, &mut out) }, S_OK);
        assert_eq!(out, l);
        release(l);
    }
    assert_eq!(unsafe { method::<Qi>(l, 0)(l, &ID3D12Fence::IID, &mut out) }, E_NOINTERFACE);
    assert!(out.is_null());
    assert_eq!(unsafe { method::<Qi>(a, 0)(a, &ID3D12CommandAllocator::IID, &mut out) }, S_OK);
    release(a);
    // GetDevice without a device: E_NOINTERFACE, null.
    assert_eq!(unsafe { method::<Qi>(l, 0x38)(l, &ID3D12Device::IID, &mut out) }, E_NOINTERFACE);
    // Private data round trip.
    type Set = unsafe extern "system" fn(Raw, *const GUID, u32, *const u8) -> HRESULT;
    type Get = unsafe extern "system" fn(Raw, *const GUID, *mut u32, *mut u8) -> HRESULT;
    let g = GUID::from_u128(0x1234);
    assert_eq!(unsafe { method::<Set>(l, 0x20)(l, &g, 3, b"abc".as_ptr()) }, S_OK);
    let (mut n, mut buf) = (8u32, [0u8; 8]);
    assert_eq!(unsafe { method::<Get>(l, 0x18)(l, &g, &mut n, buf.as_mut_ptr()) }, S_OK);
    assert_eq!((n, &buf[..3]), (3, &b"abc"[..]));
    assert_eq!(release(l), 0);
    assert_eq!(release(a), 0);
}

// ---- translation, on the stand-in ----

#[repr(C)]
#[derive(Clone, Copy)]
struct ConsoleBarrier {
    kind: u32,
    flags: u32,
    a: Raw,
    b: u64,
    c: u64,
}

#[test]
fn calls_are_translated_into_the_host_list() {
    let h = Arc::new(TestHost::default());
    let (l, a, backend) = fake_list(h.clone());
    unsafe {
        // heaps, pipeline (brings its root signature), a table, a root CBV, a draw
        method::<unsafe extern "system" fn(Raw, u32, *const Raw)>(l, 0x100)(l, 1, &HEAP);
        method::<unsafe extern "system" fn(Raw, Raw)>(l, 0xc8)(l, 0xAAA0 as Raw);
        method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x120)(l, 0, 0x1000_0000 + 3 * 32);
        method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x150)(l, 2, MEM + 0x40);
        method::<unsafe extern "system" fn(Raw, u32, u32, u32, i32, u32)>(l, 0x68)(l, 3, 1, 0, 0, 0);
        // the same again: dropped
        method::<unsafe extern "system" fn(Raw, Raw)>(l, 0xc8)(l, 0xAAA0 as Raw);
        method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x120)(l, 0, 0x1000_0000 + 3 * 32);
        method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x150)(l, 2, MEM + 0x40);
        // an unknown pipeline and an address outside every range: refused, not guessed
        method::<unsafe extern "system" fn(Raw, Raw)>(l, 0xc8)(l, BAD);
        method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x150)(l, 2, 0x5);
    }
    let f = fake::host_of(l);
    assert_eq!(f.count(host::SET_DESCRIPTOR_HEAPS), 1);
    assert_eq!(f.count(host::SET_PIPELINE_STATE), 1);
    assert_eq!(f.arg(host::SET_PIPELINE_STATE, 0), 0x8000_AAA0);
    assert_eq!(f.count(host::SET_GRAPHICS_ROOT_SIGNATURE), 1);
    assert_eq!(f.arg(host::SET_GRAPHICS_ROOT_SIGNATURE, 0), 0x5160);
    assert_eq!(f.count(host::SET_GRAPHICS_ROOT_TABLE), 1);
    assert_eq!(f.arg(host::SET_GRAPHICS_ROOT_TABLE, 1), 0x9_0000_0000 + 3 * 64, "descriptor 3 at the host's stride");
    assert_eq!(f.count(host::SET_GRAPHICS_ROOT_CBV), 1);
    assert_eq!(f.arg(host::SET_GRAPHICS_ROOT_CBV, 1), MEM_HOST + 0x40);
    assert_eq!(f.count(host::DRAW_INDEXED), 1);
    let st = stats(l).unwrap();
    assert_eq!((st.draws, st.elided, st.refused), (1, 3, 2));

    unsafe {
        // render target + clear: console CPU handles to host ones
        let rt = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: 0x3000 };
        method::<unsafe extern "system" fn(Raw, u32, *const D3D12_CPU_DESCRIPTOR_HANDLE, BOOL, *const D3D12_CPU_DESCRIPTOR_HANDLE)>(l, 0x190)(l, 1, &rt, BOOL(0), std::ptr::null());
        let red = [1.0f32, 0.0, 0.0, 1.0];
        method::<unsafe extern "system" fn(Raw, D3D12_CPU_DESCRIPTOR_HANDLE, *const f32, u32, *const RECT)>(l, 0x1a0)(l, rt, red.as_ptr(), 0, std::ptr::null());
        // a transition barrier and one on an unknown resource (becomes a global barrier)
        let b = [
            ConsoleBarrier { kind: 0, flags: 0, a: 0x1000 as Raw, b: 0xffff_ffff | (4 << 32), c: 0x800 },
            ConsoleBarrier { kind: 0, flags: 0, a: BAD, b: 0, c: 0 },
        ];
        method::<unsafe extern "system" fn(Raw, u32, *const ConsoleBarrier) -> u32>(l, 0xd0)(l, 2, b.as_ptr());
        // ExecuteIndirect: signature and argument buffer mapped
        method::<unsafe extern "system" fn(Raw, Raw, u32, Raw, u64, Raw, u64)>(l, 0x1e8)(l, 0x516 as Raw, 4, 0xA000 as Raw, 0x20, std::ptr::null_mut(), 0);
        // copy memory to memory: both addresses become (resource, offset)
        method::<unsafe extern "system" fn(Raw, u64, u64, u64, u32)>(l, 0x1f0)(l, MEM + 0x1000, MEM + 0x2000, 256, 0);
    }
    assert_eq!(f.count(host::OM_SET_RENDER_TARGETS), 1);
    assert_eq!(f.count(host::CLEAR_RTV), 1);
    assert_eq!(f.arg(host::CLEAR_RTV, 0), 0x3100);
    assert_eq!(f.count(host::RESOURCE_BARRIER), 1);
    assert_eq!(f.arg(host::RESOURCE_BARRIER, 0), 2, "the mapped transition and one global barrier");
    assert_eq!(f.count(host::EXECUTE_INDIRECT), 1);
    assert_eq!((f.arg(host::EXECUTE_INDIRECT, 0), f.arg(host::EXECUTE_INDIRECT, 2)), (0x516, 0x1A000));
    assert_eq!(f.count(host::COPY_BUFFER_REGION), 1);
    assert_eq!((f.arg(host::COPY_BUFFER_REGION, 0), f.arg(host::COPY_BUFFER_REGION, 1), f.arg(host::COPY_BUFFER_REGION, 2)), (0x77, 0x1100, 0x77));

    unsafe {
        // enhanced barriers: a buffer barrier on a known resource and a texture barrier on an
        // unknown one (kept as a global barrier with its sync and access)
        let mut buffer = [0u8; 40];
        buffer[16..24].copy_from_slice(&0x2222u64.to_le_bytes());
        let mut texture = [0u8; 64];
        texture[24..32].copy_from_slice(&(BAD as u64).to_le_bytes());
        #[repr(C)]
        struct Group(u32, u32, *const u8);
        let groups = [Group(2, 1, buffer.as_ptr()), Group(1, 1, texture.as_ptr())];
        method::<unsafe extern "system" fn(Raw, u32, *const Group)>(l, 0x490)(l, 2, groups.as_ptr());
        // pipe syncs: a barrier after work, none after none
        method::<unsafe extern "system" fn(Raw, u32, u32, u32, i32, u32)>(l, 0x68)(l, 3, 1, 0, 0, 0);
        method::<unsafe extern "system" fn(Raw, u32, u32, u64, u64)>(l, 0x420)(l, 0, 0, 0, 0);
        method::<unsafe extern "system" fn(Raw, u32, u32, u64, u64)>(l, 0x418)(l, 0, 0, 0, 0);
        // a label: the host's label callback with the host address
        method::<unsafe extern "system" fn(Raw, u64, u32, u32, u32)>(l, 0x238)(l, MEM + 0x80, 7, 0, 0);
    }
    assert_eq!(f.count(host::BARRIER), 1);
    assert_eq!(f.arg(host::BARRIER, 0), 2, "two groups: global + buffer");
    assert_eq!(f.count(host::RESOURCE_BARRIER), 2, "one sync barrier after the draw, none after the second sync");
    assert_eq!(f.count(host::WRITE_BUFFER_IMMEDIATE), 1);
    let labels = h.labels.lock().unwrap().clone();
    assert_eq!(labels.len(), 1);
    assert_eq!((labels[0].0.value, labels[0].1), (LabelValue::Value(7), Some(MEM_HOST + 0x80)));

    // 0x4a0: real memory, aligned, writable, owned by the allocator
    let p = unsafe { method::<unsafe extern "system" fn(Raw, u64, u64) -> *mut u8>(l, 0x4a0)(l, 100, 256) };
    assert!(!p.is_null() && p as usize % 256 == 0);
    unsafe { p.write_bytes(0xAB, 100) };
    let q = unsafe { method::<unsafe extern "system" fn(Raw, u64, u64) -> *mut u8>(l, 0x4a0)(l, 8, 16) };
    assert!(q as usize >= p as usize + 100, "carved after the first");
    assert!(upload_range(a, p as u64).is_some(), "the address resolves through the allocator");
    // a root view at that address resolves too
    unsafe { method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x160)(l, 1, p as u64) };
    assert_eq!(fake::host_of(l).count(40), 1, "SetGraphicsRootShaderResourceView");

    // Close, Reset with the allocator: the same host list, open again, counts restarted
    assert_eq!(unsafe { method::<Hr0>(l, 0x48)(l) }, S_OK);
    assert_ne!(unsafe { method::<Hr0>(l, 0x48)(l) }, S_OK, "closing twice fails, as D3D12");
    assert_eq!(f.count(host::CLOSE), 1);
    assert_eq!(unsafe { method::<Hr0>(a, 0x40)(a) }, S_OK, "allocator Reset");
    let r = unsafe { method::<unsafe extern "system" fn(Raw, Raw, Raw, u32) -> HRESULT>(l, 0x458)(l, a, 0xBBB0 as Raw, 0) };
    assert_eq!(r, S_OK, "ResetX");
    assert_eq!(f.count(host::RESET), 1);
    assert_eq!(stats(l).unwrap().calls, 0, "counts restart at reset");
    assert_eq!(f.count(host::SET_PIPELINE_STATE), 2, "the reset's pipeline is set");
    assert_eq!(backend.lists.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(host_list(l).is_some());
    assert_eq!(release(l), 0);
    assert_eq!(release(a), 0);
}

#[test]
fn a_memory_wait_cuts_the_list_and_the_state_follows() {
    let h = Arc::new(TestHost::default());
    let (l, a, backend) = fake_list(h.clone());
    type Wait = unsafe extern "system" fn(Raw, u64, u32, u64);
    unsafe {
        // a wait before any work needs no cut
        method::<Wait>(l, 0x250)(l, MEM, 7, 1);
        method::<unsafe extern "system" fn(Raw, u32, *const Raw)>(l, 0x100)(l, 1, &HEAP);
        method::<unsafe extern "system" fn(Raw, Raw)>(l, 0xc8)(l, 0xAAA0 as Raw);
        method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x120)(l, 1, 0x1000_0000);
        let c = [1u32, 2, 3];
        method::<unsafe extern "system" fn(Raw, u32, u32, *const u32, u32)>(l, 0x140)(l, 3, 3, c.as_ptr(), 1);
        method::<unsafe extern "system" fn(Raw, u32, u32, u32, u32)>(l, 0x60)(l, 3, 1, 0, 0);
        method::<Wait>(l, 0x248)(l, MEM + 8, 3, 0x1_0000_0005);
        method::<unsafe extern "system" fn(Raw, u32, u32, u32, u32)>(l, 0x60)(l, 3, 1, 0, 0);
        assert_eq!(method::<Hr0>(l, 0x48)(l), S_OK);
    }
    let segs = segments(l).unwrap();
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].waits, vec![MemoryWait { address: MEM, comparison: 7, reference: 1, wide: true }]);
    assert_eq!(segs[1].waits, vec![MemoryWait { address: MEM + 8, comparison: 3, reference: 5, wide: false }]);
    assert!(host_list(l).is_none(), "several segments: the queue uses segments()");
    assert_eq!(backend.lists.load(std::sync::atomic::Ordering::Relaxed), 2);
    let (first, second) = (Fake::of(segs[0].list.list.as_raw()), Fake::of(segs[1].list.list.as_raw()));
    assert_eq!(first.count(host::CLOSE), 1);
    assert_eq!(second.count(host::CLOSE), 1);
    // the second part starts with what was bound: heaps, root signature, pipeline, table, constants
    assert_eq!(second.count(host::SET_DESCRIPTOR_HEAPS), 1);
    assert_eq!(second.count(host::SET_GRAPHICS_ROOT_SIGNATURE), 1);
    assert_eq!(second.count(host::SET_PIPELINE_STATE), 1);
    assert_eq!(second.count(host::SET_GRAPHICS_ROOT_TABLE), 1);
    assert_eq!(second.count(34), 3, "three root constants, one by one");
    assert_eq!(second.count(12), 1, "the draw after the wait");
    assert_eq!(stats(l).unwrap().splits, 1);
    assert_eq!(h.waits.lock().unwrap().len(), 2);
    // MemoryWait compares as D3D12's comparison functions
    assert!(segs[0].waits[0].holds_for(1) && !segs[0].waits[0].holds_for(0));
    assert!(segs[1].waits[0].holds_for(0xFFFF_0000_0000_0005));

    // Reset folds the parts back into one list; the spare is reused at the next cut
    unsafe {
        assert_eq!(method::<unsafe extern "system" fn(Raw, Raw, Raw) -> HRESULT>(l, 0x50)(l, a, std::ptr::null_mut()), S_OK);
        method::<unsafe extern "system" fn(Raw, u32, u32, u32, u32)>(l, 0x60)(l, 3, 1, 0, 0);
        method::<Wait>(l, 0x248)(l, MEM, 7, 2);
    }
    assert_eq!(segments(l).unwrap().len(), 2);
    assert_eq!(backend.lists.load(std::sync::atomic::Ordering::Relaxed), 2, "no new host list");
    assert_eq!(release(l), 0);
    assert_eq!(release(a), 0);
}

/// Our side alone: every call translated into a stand-in list that only counts. The frame is
/// Build 0 (b)'s (4,500 draws, ~20,300 calls), each call through the console table.
#[test]
fn our_cost_per_draw_on_the_stand_in() {
    let shape = crate::frame::Shape::default();
    let lay = crate::frame::layout(&shape);
    let frame = crate::frame::frame(&shape, &lay, 1);
    let h = Arc::new(BenchHost::for_layout(&lay, None));
    let backend = fake::backend();
    let a = alloc::create_on(0, backend, h).unwrap();
    let l = create_list(0, a, std::ptr::null_mut()).unwrap();
    let mut best = f64::MAX;
    let mut calls = 0;
    for _ in 0..20 {
        let t = Instant::now();
        for cmds in &frame {
            unsafe { record(l, a, cmds, &lay) };
        }
        best = best.min(t.elapsed().as_secs_f64());
        calls = frame.iter().map(|c| c.len()).sum::<usize>();
    }
    let per_draw = best * 1e6 / shape.draws as f64;
    println!("stand-in: {calls} calls, {} draws: {:.3} ms a frame, {per_draw:.4} µs a draw, {:.4} µs a call", shape.draws, best * 1e3, best * 1e6 / calls as f64);
    assert!(per_draw < 0.43, "ours alone {per_draw:.3} µs a draw: over the 0.43 µs class");
    assert_eq!(release(l), 0);
    assert_eq!(release(a), 0);
}

// ---- the frame of Build 0 (b) as console calls ----

/// Console tokens the bench host knows.
const BENCH_HEAP: Raw = 0xD000 as Raw;
const BENCH_ROOT: Raw = 0x5160 as Raw;
const BENCH_RTV: usize = 0xC000;
const BENCH_MEMORY: Raw = 0xE000 as Raw;
const BENCH_SIGNATURE: Raw = 0xF000 as Raw;

struct BenchHost {
    pipelines: crate::PipelineTable,
    pso: Vec<usize>,
    root: usize,
    heap: HeapBinding,
    memory: MemoryRange,
    targets: Vec<u64>,
    target_raw: Vec<usize>,
    rtv: usize,
    indirect: usize,
}
unsafe impl Send for BenchHost {}
unsafe impl Sync for BenchHost {}

impl BenchHost {
    fn for_layout(lay: &crate::frame::Layout, setup: Option<&crate::d3d::Setup>) -> BenchHost {
        let n = lay.pipeline_handles.len();
        let slots = crate::frame::Shape::default().descriptor_slots;
        BenchHost {
            pipelines: crate::PipelineTable::new(&lay.pipeline_handles),
            pso: (0..n).map(|i| setup.map_or(0x10_0000 + i, |s| s.pipeline_raw(i) as usize)).collect(),
            root: setup.map_or(0x5161, |s| s.signature_raw() as usize),
            heap: HeapBinding {
                heap: setup.map_or(0x4EA9 as Raw, |s| s.heap_raw()),
                console_base: lay.descriptor_base,
                console_bytes: slots * crate::CONSOLE_DESCRIPTOR,
                console_stride: 32,
                host_base: setup.map_or(0x9_0000_0000, |s| s.heap_gpu_start()),
                host_stride: setup.map_or(32, |s| s.view_increment()),
            },
            memory: MemoryRange {
                console: lay.memory_base,
                bytes: 64 << 20,
                host_va: setup.map_or(0x7_0000_0000, |s| s.memory_va()),
                resource: setup.map_or(0x77 as Raw, |s| s.memory_raw()),
                resource_offset: 0,
            },
            targets: lay.barrier_targets.clone(),
            target_raw: (0..lay.barrier_targets.len()).map(|i| setup.map_or(0x4000 + i, |s| s.target_raw(i) as usize)).collect(),
            rtv: setup.map_or(0xC100, |s| s.rtv_cpu()),
            indirect: setup.map_or(0x5160, |s| s.indirect_raw() as usize),
        }
    }
}

impl PipelineSource for BenchHost {
    fn pipeline(&self, console: Raw) -> Option<HostPipeline> {
        let i = self.pipelines.get(console as u64)?;
        Some(HostPipeline { pso: self.pso[i as usize] as Raw, root_signature: self.root as Raw, compute: false })
    }
}

impl ListHost for BenchHost {
    fn root_signature(&self, console: Raw) -> Option<Raw> {
        (console == BENCH_ROOT).then_some(self.root as Raw)
    }
    fn resource(&self, console: Raw) -> Option<Raw> {
        if console == BENCH_MEMORY {
            return Some(self.memory.resource);
        }
        self.targets.binary_search(&(console as u64)).ok().map(|i| self.target_raw[i] as Raw)
    }
    fn descriptor_heap(&self, console: Raw) -> Option<HeapBinding> {
        (console == BENCH_HEAP).then_some(self.heap)
    }
    fn cpu_descriptor(&self, console: usize) -> Option<usize> {
        (console == BENCH_RTV).then_some(self.rtv)
    }
    fn command_signature(&self, console: Raw) -> Option<Raw> {
        (console == BENCH_SIGNATURE).then_some(self.indirect as Raw)
    }
    fn memory(&self, va: u64) -> Option<MemoryRange> {
        self.memory.map(va).map(|_| self.memory)
    }
}

/// Reset `l` on `a` and record `cmds` through the console table, as the title would.
unsafe fn record(l: Raw, a: Raw, cmds: &[crate::Cmd], lay: &crate::frame::Layout) {
    use crate::Cmd;
    unsafe {
        let _ = method::<Hr0>(a, 0x40)(a);
        let _ = method::<unsafe extern "system" fn(Raw, Raw, Raw) -> HRESULT>(l, 0x50)(l, a, std::ptr::null_mut());
        method::<unsafe extern "system" fn(Raw, u32, *const Raw)>(l, 0x100)(l, 1, &BENCH_HEAP);
        method::<unsafe extern "system" fn(Raw, Raw)>(l, 0x110)(l, BENCH_ROOT);
        let rt = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: BENCH_RTV };
        method::<unsafe extern "system" fn(Raw, u32, *const D3D12_CPU_DESCRIPTOR_HANDLE, BOOL, *const D3D12_CPU_DESCRIPTOR_HANDLE)>(l, 0x190)(l, 1, &rt, BOOL(0), std::ptr::null());
        let vp = D3D12_VIEWPORT { Width: 64.0, Height: 64.0, MaxDepth: 1.0, ..Default::default() };
        method::<unsafe extern "system" fn(Raw, u32, *const D3D12_VIEWPORT)>(l, 0xa8)(l, 1, &vp);
        let sc = RECT { left: 0, top: 0, right: 64, bottom: 64 };
        method::<unsafe extern "system" fn(Raw, u32, *const RECT)>(l, 0xb0)(l, 1, &sc);
        method::<unsafe extern "system" fn(Raw, i32)>(l, 0xa0)(l, 4);

        let set_pipeline = method::<unsafe extern "system" fn(Raw, Raw)>(l, 0xc8);
        let table = method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x120);
        let cbv = method::<unsafe extern "system" fn(Raw, u32, u64)>(l, 0x150);
        let consts = method::<unsafe extern "system" fn(Raw, u32, u32, *const u32, u32)>(l, 0x140);
        let vb = method::<unsafe extern "system" fn(Raw, u32, u32, *const D3D12_VERTEX_BUFFER_VIEW)>(l, 0x180);
        let ib = method::<unsafe extern "system" fn(Raw, *const D3D12_INDEX_BUFFER_VIEW)>(l, 0x178);
        let barrier = method::<unsafe extern "system" fn(Raw, u32, *const ConsoleBarrier) -> u32>(l, 0xd0);
        let draw = method::<unsafe extern "system" fn(Raw, u32, u32, u32, i32, u32)>(l, 0x68);
        let indirect = method::<unsafe extern "system" fn(Raw, Raw, u32, Raw, u64, Raw, u64)>(l, 0x1e8);
        for c in cmds {
            match *c {
                Cmd::SetPipeline(h) => set_pipeline(l, h as Raw),
                Cmd::SetTable(slot, va) => table(l, slot as u32, va),
                Cmd::SetCbv(_, va) => cbv(l, 2, va),
                Cmd::SetConstants(v) => consts(l, 3, 4, v.as_ptr(), 0),
                Cmd::SetVertexBuffer(va, bytes, stride) => {
                    vb(l, 0, 1, &D3D12_VERTEX_BUFFER_VIEW { BufferLocation: va, SizeInBytes: bytes, StrideInBytes: stride })
                }
                Cmd::SetIndexBuffer(va, bytes) => ib(l, &D3D12_INDEX_BUFFER_VIEW { BufferLocation: va, SizeInBytes: bytes, Format: DXGI_FORMAT_R16_UINT }),
                Cmd::Barrier(va) => {
                    let _ = barrier(l, 1, &ConsoleBarrier { kind: 2, flags: 0, a: va as Raw, b: 0, c: 0 });
                }
                Cmd::Draw(n, i, f) => draw(l, n, i, f, 0, 0),
                Cmd::DrawIndirect(va, count) => indirect(l, BENCH_SIGNATURE, count, BENCH_MEMORY, va - lay.memory_base, std::ptr::null_mut(), 0),
            }
        }
        let _ = method::<Hr0>(l, 0x48)(l);
    }
}

// ---- on the card (by hand, while agents\MEASURING is absent) ----

fn measuring() -> bool {
    let lock = std::path::Path::new(r"D:\Developer\triumph-rebuild\agents\MEASURING");
    if lock.exists() {
        println!("skipped: agents\\MEASURING is present (the lead is timing)");
    }
    lock.exists()
}

fn device() -> ID3D12Device {
    let mut d: Option<ID3D12Device> = None;
    unsafe { D3D12CreateDevice(None::<&windows::core::IUnknown>, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut d) }.expect("device");
    d.expect("device")
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r) }.expect("buffer");
    r.expect("buffer")
}

struct CardHost {
    texture: usize,
    readback: usize,
    readback_va: u64,
    rtv: usize,
}
unsafe impl Send for CardHost {}
unsafe impl Sync for CardHost {}

const CARD_TEXTURE: Raw = 0x1000 as Raw;
const CARD_RTV: usize = 0x3000;
const CARD_MEM: u64 = 0x10_0000_0000;

impl PipelineSource for CardHost {
    fn pipeline(&self, _: Raw) -> Option<HostPipeline> {
        None
    }
}

impl ListHost for CardHost {
    fn resource(&self, console: Raw) -> Option<Raw> {
        (console == CARD_TEXTURE).then_some(self.texture as Raw)
    }
    fn descriptor_heap(&self, _: Raw) -> Option<HeapBinding> {
        None
    }
    fn cpu_descriptor(&self, console: usize) -> Option<usize> {
        (console == CARD_RTV).then_some(self.rtv)
    }
    fn memory(&self, va: u64) -> Option<MemoryRange> {
        let r = MemoryRange { console: CARD_MEM, bytes: 64 << 10, host_va: self.readback_va, resource: self.readback as Raw, resource_offset: 0 };
        r.map(va).map(|_| r)
    }
}

/// Record a clear, a barrier, a 64-bit label, a copy out of 0x4a0 memory, a memory wait (the
/// cut) and a pipe sync through the console table into real host lists; submit the segments in
/// order; read back the pixel, the label and the copied bytes.
#[test]
#[ignore = "GPU: by hand, while agents\\MEASURING is absent"]
fn card_records_a_clear_a_barrier_a_label_and_a_cut() {
    if measuring() {
        return;
    }
    // The debug layer, when the Graphics Tools are installed: every D3D12 error is counted below.
    let debug = unsafe {
        let mut d: Option<ID3D12Debug> = None;
        D3D12GetDebugInterface(&mut d).ok().and(d).map(|d| d.EnableDebugLayer()).is_some()
    };
    let device = device();
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let tex_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &tex_desc, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut texture)
            .unwrap();
        let texture = texture.unwrap();
        let rtv_heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() }).unwrap();
        let rtv = rtv_heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&texture, None, rtv);
        let readback = buffer(&device, D3D12_HEAP_TYPE_READBACK, 64 << 10, D3D12_RESOURCE_STATE_COPY_DEST);

        let host = Arc::new(CardHost { texture: texture.as_raw() as usize, readback: readback.as_raw() as usize, readback_va: readback.GetGPUVirtualAddress(), rtv: rtv.ptr });
        let a = create_allocator(0, &device, host).unwrap();
        let l = create_list(0, a, std::ptr::null_mut()).unwrap();

        let red = [1.0f32, 0.0, 0.0, 1.0];
        method::<unsafe extern "system" fn(Raw, D3D12_CPU_DESCRIPTOR_HANDLE, *const f32, u32, *const RECT)>(l, 0x1a0)(l, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: CARD_RTV }, red.as_ptr(), 0, std::ptr::null());
        let to_copy = ConsoleBarrier { kind: 0, flags: 0, a: CARD_TEXTURE, b: 0xffff_ffff | ((D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u64) << 32), c: D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u64 };
        method::<unsafe extern "system" fn(Raw, u32, *const ConsoleBarrier) -> u32>(l, 0xd0)(l, 1, &to_copy);
        method::<unsafe extern "system" fn(Raw, u64, u64, u32, u32)>(l, 0x240)(l, CARD_MEM, 0x1_2345_6789, 0, 0);
        let p = method::<unsafe extern "system" fn(Raw, u64, u64) -> *mut u8>(l, 0x4a0)(l, 64, 256);
        assert!(!p.is_null());
        for i in 0..64 {
            p.add(i).write(i as u8 ^ 0x5A);
        }
        method::<unsafe extern "system" fn(Raw, u64, u64, u64, u32)>(l, 0x1f0)(l, CARD_MEM + 0x100, p as u64, 64, 0);
        method::<unsafe extern "system" fn(Raw, u64, u32, u64)>(l, 0x250)(l, CARD_MEM, 7, 1);
        method::<unsafe extern "system" fn(Raw, u32, u32, u64, u64)>(l, 0x420)(l, 0, 0, 0, 0);
        // an enhanced global barrier group (0x490): sync all, copy writes before copy reads
        let global: [u32; 4] = [D3D12_BARRIER_SYNC_ALL.0 as u32, D3D12_BARRIER_SYNC_ALL.0 as u32, D3D12_BARRIER_ACCESS_COPY_DEST.0 as u32, D3D12_BARRIER_ACCESS_COPY_SOURCE.0 as u32];
        #[repr(C)]
        struct Group(u32, u32, *const u32);
        method::<unsafe extern "system" fn(Raw, u32, *const Group)>(l, 0x490)(l, 1, &Group(0, 1, global.as_ptr()));
        // back to render target for the next frame, then the copy below needs COPY_SOURCE again:
        // keep it in COPY_SOURCE (no barrier) so the test's own list can read it.
        assert_eq!(method::<Hr0>(l, 0x48)(l), S_OK);
        let st = stats(l).unwrap();
        assert_eq!((st.refused, st.splits), (0, 1), "{st:?}");

        // as the lead's queue will: each segment after its waits
        let segs = segments(l).unwrap();
        assert_eq!(segs.len(), 2);
        let mut value = 0;
        for (i, seg) in segs.iter().enumerate() {
            for w in &seg.waits {
                // the label the first segment wrote, read where the GPU put it
                let mut m: *mut std::ffi::c_void = std::ptr::null_mut();
                readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 8 }), Some(&mut m)).unwrap();
                let v = m.cast::<u64>().read_volatile();
                readback.Unmap(0, None);
                assert!(w.holds_for(v), "segment {i} waits for {w:?}; memory holds {v:#x}");
            }
            queue.ExecuteCommandLists(&[Some(seg.list.list.cast::<ID3D12CommandList>().unwrap())]);
            value += 1;
            queue.Signal(&fence, value).unwrap();
            fence.SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE::default()).unwrap();
        }

        // the test's own list: the cleared texture into the readback buffer at 4 KiB
        let ca: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let cl: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &ca, None).unwrap();
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&readback),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 4096,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Width: 4, Height: 4, Depth: 1, RowPitch: 256 },
                },
            },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&texture), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
        cl.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        cl.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(cl.cast::<ID3D12CommandList>().unwrap())]);
        value += 1;
        queue.Signal(&fence, value).unwrap();
        fence.SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE::default()).unwrap();
        device.GetDeviceRemovedReason().expect("device alive");

        let mut m: *mut std::ffi::c_void = std::ptr::null_mut();
        readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 8192 }), Some(&mut m)).unwrap();
        let bytes = std::slice::from_raw_parts(m.cast::<u8>(), 8192);
        let label = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let copied: Vec<u8> = bytes[0x100..0x140].to_vec();
        let pixel = &bytes[4096..4100];
        println!("label {label:#x}, pixel {pixel:?}, copied[0..4] {:?}", &copied[..4]);
        assert_eq!(label, 0x1_2345_6789, "the 64-bit label landed");
        assert_eq!(copied, (0..64).map(|i| i as u8 ^ 0x5A).collect::<Vec<_>>(), "0x4a0 memory read by the GPU");
        assert_eq!(pixel, &[255, 0, 0, 255], "the clear, after the barrier");
        if debug {
            let q: ID3D12InfoQueue = device.cast().unwrap();
            let mut errors = 0;
            for i in 0..q.GetNumStoredMessages() {
                let mut len = 0;
                let _ = q.GetMessage(i, None, &mut len);
                let mut buf = vec![0u64; len.div_ceil(8)];
                let m = buf.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if q.GetMessage(i, Some(m), &mut len).is_ok() && (*m).Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 {
                    errors += 1;
                    println!("D3D12: {}", std::ffi::CStr::from_ptr((*m).pDescription.cast()).to_string_lossy());
                }
            }
            println!("debug layer: {} messages, {errors} errors", q.GetNumStoredMessages());
            assert_eq!(errors, 0, "the debug layer found errors");
        } else {
            println!("debug layer not installed: D3D12 validation skipped");
        }
        readback.Unmap(0, None);
        std::mem::forget(dst);
        std::mem::forget(src);
        assert_eq!(release(l), 0);
        assert_eq!(release(a), 0);
    }
}

/// Build 0 (b)'s frame through the console table into real D3D12 lists on one thread, against
/// `listbench`'s 0.426 µs a draw for the same frame through `translate`.
#[test]
#[ignore = "GPU: by hand, while agents\\MEASURING is absent"]
fn card_cost_per_draw() {
    if measuring() {
        return;
    }
    let shape = crate::frame::Shape::default();
    let lay = crate::frame::layout(&shape);
    let frames: Vec<_> = (0..4).map(|s| crate::frame::frame(&shape, &lay, s + 1)).collect();
    let (mut setup, ctx) = crate::d3d::Setup::new(&lay, shape.descriptor_slots as u32).expect("setup");
    let host = Arc::new(BenchHost::for_layout(&lay, Some(&setup)));
    let pairs: Vec<(Raw, Raw)> = (0..shape.lists)
        .map(|_| {
            let a = create_allocator(0, &setup.device, host.clone()).unwrap();
            let l = create_list(0, a, std::ptr::null_mut()).unwrap();
            unsafe {
                let _ = method::<Hr0>(l, 0x48)(l);
            }
            (l, a)
        })
        .collect();
    // Build 0 (b)'s path on the same frames, same device, interleaved round by round so both see
    // the same machine: `translate` into `D3dSink`, one worker thread.
    let base_lists = crate::d3d::Lists::new(&setup, shape.lists).expect("lists");
    let (mut ours, mut base) = (Vec::new(), Vec::new());
    let mut refused = 0;
    let mut calls = 0;
    const ROUNDS: usize = 40;
    for f in 0..ROUNDS {
        let frame = &frames[f % frames.len()];
        let t = Instant::now();
        for (cmds, &(l, a)) in frame.iter().zip(&pairs) {
            unsafe { record(l, a, cmds, &lay) };
        }
        ours.push(t.elapsed().as_secs_f64());
        let mut lists = Vec::new();
        for &(l, _) in &pairs[..frame.len()] {
            let st = stats(l).unwrap();
            refused += st.refused;
            calls += st.calls as usize;
            lists.push(Some(host_list(l).expect("one part").cast::<ID3D12CommandList>().unwrap()));
        }
        setup.submit(&lists).expect("frame ran");
        let b = crate::d3d::run_frames(&mut setup, &base_lists, &ctx, std::slice::from_ref(frame), 1, 1, true).expect("baseline frame");
        base.push(b[0].thread_ms / 1e3);
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let (m_ours, m_base) = (median(&mut ours), median(&mut base));
    let per_draw = m_ours * 1e6 / shape.draws as f64;
    let base_draw = m_base * 1e6 / shape.draws as f64;
    println!(
        "card: {} lists, {} draws, {} calls a frame (incl. per-list setup), {ROUNDS} rounds, medians, one thread
  console table: {:.3} ms a frame, {per_draw:.3} µs a draw, {:.4} µs a call (best {:.3} ms)
  translate():   {:.3} ms a frame, {base_draw:.3} µs a draw (best {:.3} ms)  [Build 0 (b) recorded 0.426]
  ratio {:.2}; refused {refused}",
        shape.lists,
        shape.draws,
        calls / ROUNDS,
        m_ours * 1e3,
        m_ours * 1e6 / (calls / ROUNDS) as f64,
        ours[0] * 1e3,
        m_base * 1e3,
        base[0] * 1e3,
        m_ours / m_base
    );
    assert_eq!(refused, 0);
    for (l, a) in pairs {
        assert_eq!(release(l), 0);
        assert_eq!(release(a), 0);
    }
}
