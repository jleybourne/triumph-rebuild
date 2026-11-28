//! Benches of the console list objects (`listbench`, and the tests): Build 0 (b)'s synthetic frame recorded through
//! the console table against a host that knows its tokens ([`BenchHost`]), and a stand-in for the title's own work
//! between list calls ([`Pollute`]), so the cost of translating as each call arrives can be compared with
//! translating a whole list at once.

use std::ffi::c_void;

use windows::core::HRESULT;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{HeapBinding, HostPipeline, HostSignature, IndirectShape, ListHost, MemoryRange, PipelineSource, Raw, UserMap};

/// Console tokens the bench host knows.
pub const BENCH_HEAP: Raw = 0xD000 as Raw;
pub const BENCH_ROOT: Raw = 0x5160 as Raw;
pub const BENCH_RTV: usize = 0xC000;
pub const BENCH_MEMORY: Raw = 0xE000 as Raw;
pub const BENCH_SIGNATURE: Raw = 0xF000 as Raw;

/// The console's barrier record (D3D12's layout).
#[repr(C)]
#[derive(Clone, Copy)]
struct ConsoleBarrier {
    kind: u32,
    flags: u32,
    a: Raw,
    b: u64,
    c: u64,
}

/// The function in `offset` of `obj`'s table, as title code loads it.
fn method<F: Copy>(obj: Raw, offset: usize) -> F {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    unsafe {
        let table = *obj.cast::<*const *const ()>();
        std::mem::transmute_copy(&*table.add(offset / 8))
    }
}

type Hr0 = unsafe extern "system" fn(Raw) -> HRESULT;

/// The host side of the bench frame: every console token it uses maps to a host object (real ones from a
/// [`crate::d3d::Setup`], or made-up values for a stand-in backend).
pub struct BenchHost {
    pub pipelines: crate::PipelineTable,
    pub pso: Vec<usize>,
    pub root: usize,
    pub heap: HeapBinding,
    pub memory: MemoryRange,
    pub targets: Vec<u64>,
    pub target_raw: Vec<usize>,
    pub rtv: usize,
    pub indirect: usize,
    pub user: Option<&'static UserMap>,
}
unsafe impl Send for BenchHost {}
unsafe impl Sync for BenchHost {}

impl BenchHost {
    pub fn for_layout(lay: &crate::frame::Layout, setup: Option<&crate::d3d::Setup>) -> BenchHost {
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
                shadow_va: 0,
                pages: None,
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
            user: None,
        }
    }
}

impl PipelineSource for BenchHost {
    fn pipeline(&self, console: Raw) -> Option<HostPipeline> {
        let i = self.pipelines.get(console as u64)?;
        Some(HostPipeline { pso: self.pso[i as usize] as Raw, root_signature: self.root as Raw, compute: false, user: self.user, topology: 0, rects: false })
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
    fn command_signature(&self, console: Raw) -> Option<HostSignature> {
        (console == BENCH_SIGNATURE).then_some(HostSignature {
            signature: self.indirect as Raw,
            root: std::ptr::null_mut(),
            shape: IndirectShape { work: IndirectShape::WORK_DRAW_INDEXED, ..Default::default() },
        })
    }
    fn memory(&self, va: u64) -> Option<MemoryRange> {
        self.memory.map(va).map(|_| self.memory)
    }
}

/// Reset `l` on `a` and record `cmds` through the console table, as the title would, calling `between` after every
/// list call (the title's own work between its calls; [`Pollute`]).
///
/// # Safety
/// `l` and `a` are a console list and allocator made by this crate, on a host that knows the bench tokens.
pub unsafe fn record(l: Raw, a: Raw, cmds: &[crate::Cmd], lay: &crate::frame::Layout, between: &mut dyn FnMut()) {
    use crate::Cmd;
    unsafe {
        let _ = method::<Hr0>(a, 0x40)(a);
        let _ = method::<unsafe extern "system" fn(Raw, Raw, Raw) -> HRESULT>(l, 0x50)(l, a, std::ptr::null_mut());
        between();
        method::<unsafe extern "system" fn(Raw, u32, *const Raw)>(l, 0x100)(l, 1, &BENCH_HEAP);
        between();
        method::<unsafe extern "system" fn(Raw, Raw)>(l, 0x110)(l, BENCH_ROOT);
        between();
        let rt = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: BENCH_RTV };
        method::<unsafe extern "system" fn(Raw, u32, *const D3D12_CPU_DESCRIPTOR_HANDLE, BOOL, *const D3D12_CPU_DESCRIPTOR_HANDLE)>(l, 0x190)(l, 1, &rt, BOOL(0), std::ptr::null());
        between();
        let vp = D3D12_VIEWPORT { Width: 64.0, Height: 64.0, MaxDepth: 1.0, ..Default::default() };
        method::<unsafe extern "system" fn(Raw, u32, *const D3D12_VIEWPORT)>(l, 0xa8)(l, 1, &vp);
        between();
        let sc = RECT { left: 0, top: 0, right: 64, bottom: 64 };
        method::<unsafe extern "system" fn(Raw, u32, *const RECT)>(l, 0xb0)(l, 1, &sc);
        between();
        method::<unsafe extern "system" fn(Raw, i32)>(l, 0xa0)(l, 4);
        between();

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
            between();
        }
        let _ = method::<Hr0>(l, 0x48)(l);
    }
}

/// A stand-in for the title's own work between two list calls: it runs `code_kb` of straight-line machine code
/// (more than the 32 KiB first-level instruction cache and the decoded-micro-op cache hold) and reads and writes
/// one byte of every 64-byte line of `data_kb` of memory (the first- and second-level data caches). After it, the
/// translator's and the driver's code and data are no longer in the closest caches, as after the title's own
/// code in a real run.
pub struct Pollute {
    code: *mut c_void,
    data: Vec<u8>,
    turn: u8,
}

impl Pollute {
    pub fn new(code_kb: usize, data_kb: usize) -> Pollute {
        use windows::Win32::System::Memory::{VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
        let code = if code_kb == 0 {
            std::ptr::null_mut()
        } else {
            let bytes = code_kb << 10;
            // SAFETY: a fresh allocation, written then made executable
            unsafe {
                let p = VirtualAlloc(None, bytes + 16, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
                assert!(!p.is_null(), "pollute: no memory");
                let b = std::slice::from_raw_parts_mut(p.cast::<u8>(), bytes + 16);
                // `add rax, imm32` (48 05 imm32), then `ret`
                let mut i = 0;
                let mut k = 1u32;
                while i + 6 <= bytes {
                    b[i..i + 2].copy_from_slice(&[0x48, 0x05]);
                    b[i + 2..i + 6].copy_from_slice(&k.to_le_bytes());
                    k = k.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    i += 6;
                }
                b[i] = 0xC3;
                let mut old = PAGE_PROTECTION_FLAGS(0);
                VirtualProtect(p, bytes + 16, PAGE_EXECUTE_READ, &mut old).expect("pollute: protect");
                p
            }
        };
        Pollute { code, data: vec![0u8; data_kb << 10], turn: 0 }
    }

    /// One stretch of the title's work.
    #[inline(never)]
    pub fn run(&mut self) {
        if !self.code.is_null() {
            // SAFETY: the code written in `new`: adds into rax, then returns (rax is volatile)
            let f: extern "system" fn() -> u64 = unsafe { std::mem::transmute(self.code) };
            std::hint::black_box(f());
        }
        self.turn = self.turn.wrapping_add(1);
        let t = self.turn;
        for i in (0..self.data.len()).step_by(64) {
            self.data[i] = self.data[i].wrapping_add(t);
        }
        std::hint::black_box(&self.data);
    }
}

impl Drop for Pollute {
    fn drop(&mut self) {
        if !self.code.is_null() {
            use windows::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
            // SAFETY: ours, from VirtualAlloc
            unsafe {
                let _ = VirtualFree(self.code, 0, MEM_RELEASE);
            }
        }
    }
}

/// Console lists on a real device for the bench frame: one (list, allocator) pair a list of the frame.
pub struct ConsoleLists {
    pub pairs: Vec<(Raw, Raw)>,
    pub host: std::sync::Arc<BenchHost>,
}

impl ConsoleLists {
    pub fn new(setup: &crate::d3d::Setup, lay: &crate::frame::Layout, n: usize) -> windows::core::Result<ConsoleLists> {
        let host = std::sync::Arc::new(BenchHost::for_layout(lay, Some(setup)));
        let mut pairs = Vec::with_capacity(n);
        for _ in 0..n {
            let a = super::create_allocator(0, &setup.device, host.clone())?;
            let l = super::create_list(0, a, std::ptr::null_mut())?;
            // SAFETY: ours, just made
            unsafe {
                let _ = method::<Hr0>(l, 0x48)(l);
            }
            pairs.push((l, a));
        }
        Ok(ConsoleLists { pairs, host })
    }

    /// Record `frame` through the console table (calling `between` after each call), submit it and wait.
    /// Returns the ticks spent inside the list functions ([`super::cost`]), the draws, the calls and the refused.
    pub fn frame(&self, setup: &mut crate::d3d::Setup, frame: &[Vec<crate::Cmd>], lay: &crate::frame::Layout, between: &mut dyn FnMut()) -> Result<(u64, u32, u32, u32), String> {
        let (mut ticks, mut draws, mut calls, mut refused) = (0u64, 0u32, 0u32, 0u32);
        let mut lists = Vec::with_capacity(frame.len());
        for (cmds, &(l, a)) in frame.iter().zip(&self.pairs) {
            // SAFETY: the pairs were made for this host
            unsafe { record(l, a, cmds, lay, between) };
            let st = super::stats(l).ok_or("not a console list")?;
            ticks += st.ticks;
            draws += st.draws;
            calls += st.calls;
            refused += st.refused;
            let h = super::host_list(l).ok_or("a list cut in parts")?;
            lists.push(Some(windows::core::Interface::cast::<ID3D12CommandList>(h).map_err(|e| e.to_string())?));
        }
        setup.submit(&lists)?;
        Ok((ticks, draws, calls, refused))
    }
}

impl Drop for ConsoleLists {
    fn drop(&mut self) {
        for &(l, a) in &self.pairs {
            // SAFETY: ours; Release at 0x10
            unsafe {
                method::<unsafe extern "system" fn(Raw) -> u32>(l, 0x10)(l);
                method::<unsafe extern "system" fn(Raw) -> u32>(a, 0x10)(a);
            }
        }
    }
}

/// What keeping a call as a record costs the recording thread (the first half of translating at Close): every
/// call of `frame` appended to a list's record buffer as a [`crate::Cmd`], with `between` after each, timed as
/// the list functions are (two `rdtsc` around each append). Returns ticks.
pub fn record_cost(frame: &[Vec<crate::Cmd>], buffers: &mut [Vec<crate::Cmd>], between: &mut dyn FnMut()) -> u64 {
    let mut ticks = 0u64;
    for (cmds, buf) in frame.iter().zip(buffers.iter_mut()) {
        buf.clear();
        for c in cmds {
            let t = super::cost::now();
            buf.push(std::hint::black_box(*c));
            ticks += super::cost::now().wrapping_sub(t);
            between();
        }
    }
    ticks
}
