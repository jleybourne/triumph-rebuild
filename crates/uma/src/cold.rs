//! Cold slices: watched memory nobody writes is not scanned, and stays exact.
//!
//! `GetWriteWatch` costs by the range it reads, not by what changed (~1.1-1.6 ms a GiB, reserved or touched;
//! `tests/wwbench.rs`), so a pass over the title's 5.2 GiB costs ~5 ms however little the title wrote: in menus
//! (~0.03 MB a frame) that scan was the frame. Here watched memory is cut into [`SLICE`]s; a slice no write
//! touched for a while goes **cold**: its committed writable pages are made read-only and the ring stops reading
//! its write-watch. The first write into it faults; the handler ([`install`]) makes that 64 KiB [`BLOCK`]
//! writable again and the slice **hot** (scanned) before the write runs, so the write is seen by write-watch as
//! any other. Nothing is missed:
//!
//! - **CPU writes** in user mode fault (the handler is first in the vectored line);
//! - **kernel writes** into a read-only page fail instead of faulting (a `ReadFile` into a cold buffer would
//!   return an error): every syscall that writes a caller's buffer for I/O calls [`kernel_write`] first
//!   (the runtime hooks `NtReadFile` and its kin), which pins the slice hot for the call and makes the range
//!   writable; a slice I/O wrote stays hot for [`IO_GRACE_MS`] after (asynchronous completions);
//! - **commits** into a cold slice ([`touch`], from the memory model): fresh pages are writable and unscanned,
//!   so the slice turns hot before the commit returns to the title;
//! - **going cold** ([`Slice::go_cold`], the ring's coordinator) marks the slice cold *before* protecting it,
//!   then reads its write-watch once more: a write that raced the protection is carried to the next pass.
//!
//! The handler, the kernel-write guard and the coordinator change a slice's protection under its own spin lock
//! only (never a lock of the model's: a title thread may fault while holding anything). Slices live for the
//! process ([`table`]): a region freed and made again at the same address finds its slice, reset.

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::*};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
use windows::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, EXCEPTION_POINTERS};
use windows::Win32::System::Memory::*;

/// The scan unit: write-watch of a hot slice is read every pass, a cold one's never.
pub const SLICE: usize = 32 << 20;
/// The protection unit: a fault makes this much writable again.
pub const BLOCK: usize = 64 << 10;
const BLOCKS: usize = SLICE / BLOCK;
const WORDS: usize = BLOCKS / 64;
/// A slice an I/O call wrote does not go cold for this long after the call (its completion may come later).
pub const IO_GRACE_MS: u64 = 2000;

pub const HOT: u8 = 0;
pub const COLD: u8 = 1;
/// never cold: memory we will not protect (a writable protection we do not restore, write-copy, ...)
pub const PINNED: u8 = 2;

/// One slice of watched memory (aligned to [`SLICE`]).
pub struct Slice {
    pub base: u64,
    state: AtomicU8,
    lock: AtomicBool,
    /// blocks we made read-only, and the protection each had
    ours: [AtomicU64; WORDS],
    orig: [AtomicU16; BLOCKS],
    /// kernel writes in flight into it
    pins: AtomicU32,
    /// ms (since [`epoch`]) before which it may not go cold
    hot_until: AtomicU64,
    /// coordinator only: when write-watch last found a page written in it (ms since [`epoch`])
    pub last_dirty: AtomicU64,
}

/// What the cold slices did over the run.
#[derive(Default)]
pub struct Counters {
    pub faults: AtomicU64,
    pub kernel_writes: AtomicU64,
    pub commits: AtomicU64,
    pub went_cold: AtomicU64,
    pub cold_us: AtomicU64,
    pub fault_us: AtomicU64,
    pub refused: AtomicU64,
}

pub fn counters() -> &'static Counters {
    static C: OnceLock<Counters> = OnceLock::new();
    C.get_or_init(Counters::default)
}

/// Milliseconds since the first call (the slices' clock).
pub fn now_ms() -> u64 {
    static T: OnceLock<Instant> = OnceLock::new();
    T.get_or_init(Instant::now).elapsed().as_millis() as u64
}

struct Spin<'a>(&'a AtomicBool);
impl Drop for Spin<'_> {
    fn drop(&mut self) {
        self.0.store(false, Release);
    }
}

impl Slice {
    fn new(base: u64) -> Slice {
        Slice {
            base,
            state: AtomicU8::new(HOT),
            lock: AtomicBool::new(false),
            ours: Default::default(),
            orig: std::array::from_fn(|_| AtomicU16::new(0)),
            pins: AtomicU32::new(0),
            hot_until: AtomicU64::new(0),
            last_dirty: AtomicU64::new(now_ms()),
        }
    }

    fn lock(&self) -> Spin<'_> {
        while self.lock.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
            std::hint::spin_loop();
        }
        Spin(&self.lock)
    }

    pub fn state(&self) -> u8 {
        self.state.load(SeqCst)
    }

    /// Whether write-watch must be read here this pass.
    pub fn scanned(&self) -> bool {
        self.state() != COLD
    }

    fn block_of(&self, va: u64) -> usize {
        ((va - self.base) / BLOCK as u64) as usize
    }

    fn is_ours(&self, b: usize) -> bool {
        self.ours[b / 64].load(SeqCst) & (1 << (b % 64)) != 0
    }

    /// Make our read-only blocks over `[from, to)` writable again (their own protection back). Lock held.
    fn restore(&self, from: u64, to: u64) -> usize {
        let (first, last) = (self.block_of(from.max(self.base)), self.block_of((to.min(self.base + SLICE as u64)) - 1));
        let mut n = 0;
        let mut b = first;
        while b <= last {
            if !self.is_ours(b) {
                b += 1;
                continue;
            }
            // a run of our blocks with one protection: one call
            let p = self.orig[b].load(Relaxed) as u32;
            let mut e = b + 1;
            while e <= last && self.is_ours(e) && self.orig[e].load(Relaxed) as u32 == p {
                e += 1;
            }
            let at = self.base + (b * BLOCK) as u64;
            let mut old = PAGE_PROTECTION_FLAGS(0);
            // a block decommitted meanwhile fails here: nothing to restore there
            let _ = unsafe { VirtualProtect(at as *const c_void, (e - b) * BLOCK, PAGE_PROTECTION_FLAGS(p), &mut old) };
            for k in b..e {
                self.ours[k / 64].fetch_and(!(1 << (k % 64)), SeqCst);
            }
            n += e - b;
            b = e;
        }
        n
    }

    /// Forget our protection over `[from, to)` without restoring it (the memory was decommitted or released).
    pub fn forget(&self, from: u64, to: u64) {
        let _g = self.lock();
        let (first, last) = (self.block_of(from.max(self.base)), self.block_of((to.min(self.base + SLICE as u64)) - 1));
        for k in first..=last {
            self.ours[k / 64].fetch_and(!(1 << (k % 64)), SeqCst);
        }
        if self.state.load(SeqCst) == COLD {
            self.state.store(HOT, SeqCst);
        }
    }

    /// Make it hot and `[from, to)` writable (a commit, or a kernel write about to happen).
    fn heat(&self, from: u64, to: u64, until: u64) {
        let _g = self.lock();
        self.restore(from, to);
        self.hot_until.fetch_max(until, SeqCst);
        if self.state.load(SeqCst) == COLD {
            self.state.store(HOT, SeqCst);
        }
    }

    /// Whether it may go cold now: hot, quiet since `quiet_ms` ago, no kernel write in flight or recent.
    pub fn may_go_cold(&self, now: u64, quiet_ms: u64) -> bool {
        self.state() == HOT && self.pins.load(SeqCst) == 0 && now >= self.hot_until.load(SeqCst) && now >= self.last_dirty.load(Relaxed) + quiet_ms
    }

    /// Go cold over the watched `parts` (address ranges inside this slice): mark it cold, then make every
    /// committed writable page of the parts read-only. `scan` is then called on each part to read (and reset)
    /// write-watch once more, for writes that raced the protection. Returns false (and stays hot, nothing
    /// protected) if a part holds memory this cannot protect and restore exactly, or a kernel write came first.
    pub fn go_cold(&self, parts: &[(u64, u64)], mut scan: impl FnMut(u64, u64)) -> bool {
        let t = Instant::now();
        let _g = self.lock();
        if self.state.load(SeqCst) != HOT {
            return false;
        }
        // cold first: from here a kernel write pins, takes the lock and finds the slice cold
        self.state.store(COLD, SeqCst);
        if self.pins.load(SeqCst) != 0 {
            self.state.store(HOT, SeqCst);
            return false;
        }
        // what is committed and how, part by part: a fully committed private allocation from its descriptor
        // (VirtualQuery's RegionSize walks to the end of a uniform run: 16 ms on 2 GiB), else VirtualQuery
        let mut runs: Vec<(u64, u64, u32)> = Vec::new();
        for &(from, to) in parts {
            if let Some(p) = whole_rw(from, to) {
                // our blocks are read-only already; every other one has the allocation's protection
                let mut b = self.block_of(from);
                let last = self.block_of(to - 1);
                while b <= last {
                    if self.is_ours(b) {
                        b += 1;
                        continue;
                    }
                    let mut e = b + 1;
                    while e <= last && !self.is_ours(e) {
                        e += 1;
                    }
                    runs.push((self.base + (b * BLOCK) as u64, self.base + (e * BLOCK) as u64, p));
                    b = e;
                }
                continue;
            }
            let mut at = from;
            while at < to {
                let mut m = MEMORY_BASIC_INFORMATION::default();
                if unsafe { VirtualQuery(Some(at as *const c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
                    break;
                }
                let end = (m.BaseAddress as u64 + m.RegionSize as u64).min(to);
                if m.State == MEM_COMMIT {
                    let p = m.Protect.0;
                    let writable = p & (PAGE_READWRITE.0 | PAGE_EXECUTE_READWRITE.0 | PAGE_WRITECOPY.0 | PAGE_EXECUTE_WRITECOPY.0) != 0;
                    if writable {
                        // only plain read-write memory, whole blocks: anything else stays hot for good
                        if (p != PAGE_READWRITE.0 && p != PAGE_EXECUTE_READWRITE.0) || at % BLOCK as u64 != 0 || end % BLOCK as u64 != 0 {
                            self.state.store(PINNED, SeqCst);
                            refused(format_args!("{at:#x}..{end:#x} protect {p:#x} (not plain read-write in whole blocks): hot for good"));
                            return false;
                        }
                        runs.push((at, end, p));
                    }
                }
                at = end;
            }
        }
        for &(from, to, p) in &runs {
            let ro = if p == PAGE_EXECUTE_READWRITE.0 { PAGE_EXECUTE_READ } else { PAGE_READONLY };
            let mut old = PAGE_PROTECTION_FLAGS(0);
            if let Err(e) = unsafe { VirtualProtect(from as *const c_void, (to - from) as usize, ro, &mut old) } {
                // undo what was done (the blocks marked ours so far), stay hot. "The parameter is incorrect" is
                // memory the graphics kernel secured (an allocation opened as a D3D12 heap in place: the
                // title's labels): it never can go cold. Anything else: not again soon
                self.restore(self.base, self.base + SLICE as u64);
                if e.code() == windows::core::HRESULT(0x8007_0057u32 as i32) {
                    self.state.store(PINNED, SeqCst);
                } else {
                    self.state.store(HOT, SeqCst);
                    self.hot_until.fetch_max(now_ms() + RETRY_MS, SeqCst);
                }
                refused(format_args!("{from:#x}..{to:#x} protect {p:#x}: VirtualProtect {e}"));
                return false;
            }
            for b in self.block_of(from)..=self.block_of(to - 1) {
                self.orig[b].store(p as u16, Relaxed);
                self.ours[b / 64].fetch_or(1 << (b % 64), SeqCst);
            }
        }
        for &(from, to) in parts {
            scan(from, to);
        }
        let c = counters();
        c.went_cold.fetch_add(1, Relaxed);
        c.cold_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
        true
    }

    /// A write fault at `va` (handler): our block is made writable and the slice hot. False: not ours.
    fn fault(&self, va: u64) -> bool {
        let t = Instant::now();
        let b = self.block_of(va);
        let _g = self.lock();
        if !self.is_ours(b) {
            return false;
        }
        let at = self.base + (b * BLOCK) as u64;
        self.restore(at, at + BLOCK as u64);
        self.state.store(HOT, SeqCst);
        let c = counters();
        c.faults.fetch_add(1, Relaxed);
        c.fault_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
        true
    }
}

/// The protection of `[from, to)` (whole blocks) if it lies in one fully committed private allocation made
/// read-write, from the allocation's descriptor (µs, not VirtualQuery's walk).
fn whole_rw(from: u64, to: u64) -> Option<u32> {
    if from % BLOCK as u64 != 0 || to % BLOCK as u64 != 0 {
        return None;
    }
    let mut r = WIN32_MEMORY_REGION_INFORMATION::default();
    let me = windows::Win32::Foundation::HANDLE(-1isize as *mut c_void);
    unsafe { QueryVirtualMemoryInformation(me, from as *const c_void, MemoryRegionInfo, &mut r as *mut _ as *mut c_void, std::mem::size_of::<WIN32_MEMORY_REGION_INFORMATION>(), None) }.ok()?;
    let (base, size) = (r.AllocationBase as u64, r.RegionSize as u64);
    let private = unsafe { r.Anonymous.Flags } & 1 != 0;
    let p = r.AllocationProtect;
    (private && base != 0 && r.CommitSize == r.RegionSize && base <= from && to <= base + size && (p == PAGE_READWRITE.0 || p == PAGE_EXECUTE_READWRITE.0)).then_some(p)
}

/// A slice that could not go cold tries again no sooner than this (ms).
const RETRY_MS: u64 = 5000;

fn refused(why: std::fmt::Arguments) {
    if counters().refused.fetch_add(1, Relaxed) < 8 {
        eprintln!("cold      a slice stays hot: {why}");
    }
}

// ------------------------------------------------------------------------------------------ the table

const CAP: usize = 1 << 14; // slices: 512 GiB of watched memory

struct Table {
    keys: Box<[AtomicU64]>,
    slots: Box<[AtomicPtr<Slice>]>,
    insert: Mutex<()>,
    /// any slice made: the kernel-write guard's fast way out
    any: AtomicBool,
}

fn table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| Table {
        keys: (0..CAP).map(|_| AtomicU64::new(0)).collect(),
        slots: (0..CAP).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
        insert: Mutex::new(()),
        any: AtomicBool::new(false),
    })
}

fn hash(k: u64) -> usize {
    (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 50) as usize & (CAP - 1)
}

/// The slice holding `va`, if one was made. Lock-free (the fault handler's way in).
pub fn find(va: u64) -> Option<&'static Slice> {
    let t = table();
    if !t.any.load(Acquire) {
        return None;
    }
    let key = va / SLICE as u64 + 1;
    let mut i = hash(key);
    for _ in 0..CAP {
        match t.keys[i].load(Acquire) {
            0 => return None,
            k if k == key => {
                let p = t.slots[i].load(Acquire);
                // SAFETY: slices are leaked: they live for the process
                return (!p.is_null()).then(|| unsafe { &*p });
            }
            _ => i = (i + 1) & (CAP - 1),
        }
    }
    None
}

/// The slice holding `va`, made (hot) if there is none. A slice found again is reset to hot with nothing of
/// ours in it when `fresh` (a region made again at the same address).
pub fn slice(va: u64, fresh: bool) -> Option<&'static Slice> {
    let t = table();
    let _g = t.insert.lock().unwrap_or_else(|e| e.into_inner());
    let key = va / SLICE as u64 + 1;
    let mut i = hash(key);
    for _ in 0..CAP {
        match t.keys[i].load(Acquire) {
            0 => {
                let s: &'static Slice = Box::leak(Box::new(Slice::new((key - 1) * SLICE as u64)));
                t.slots[i].store(s as *const _ as *mut _, Release);
                t.keys[i].store(key, Release);
                t.any.store(true, Release);
                return Some(s);
            }
            k if k == key => {
                // SAFETY: as in `find`
                let s = unsafe { &*t.slots[i].load(Acquire) };
                if fresh && s.state() == PINNED {
                    s.state.store(HOT, SeqCst);
                }
                return Some(s);
            }
            _ => i = (i + 1) & (CAP - 1),
        }
    }
    None
}

fn each(va: u64, len: u64, mut f: impl FnMut(&'static Slice, u64, u64)) {
    if len == 0 {
        return;
    }
    let end = va.saturating_add(len);
    let mut k = va / SLICE as u64;
    while k * (SLICE as u64) < end {
        let base = k * SLICE as u64;
        if let Some(s) = find(base) {
            f(s, va.max(base), end.min(base + SLICE as u64));
        }
        k += 1;
    }
}

/// `len` bytes at `va` were committed (or re-committed with a new protection): the slices turn hot before the
/// title can write there.
pub fn touch(va: u64, len: u64) {
    each(va, len, |s, from, to| {
        counters().commits.fetch_add(1, Relaxed);
        // the commit set the pages' protection itself: ours there is gone, nothing to restore
        s.forget(from, to);
    });
}

/// `len` bytes at `va` were decommitted or released: our protection there is forgotten.
pub fn forget(va: u64, len: u64) {
    each(va, len, |s, from, to| s.forget(from, to));
}

/// A kernel write into `[va, va + len)` is about to happen (a syscall's output buffer): the range becomes
/// writable and its slices hot, pinned so until the guard drops, and kept hot [`IO_GRACE_MS`] after.
pub fn kernel_write(va: u64, len: u64) -> KernelWrite {
    let mut g = KernelWrite { pinned: [None; 4], n: 0 };
    if va == 0 || len == 0 || !table().any.load(Acquire) {
        return g;
    }
    let until = now_ms() + IO_GRACE_MS;
    each(va, len, |s, from, to| {
        // pin first: the coordinator, under the lock, then sees the pin or this sees the slice cold
        s.pins.fetch_add(1, SeqCst);
        if g.n < g.pinned.len() {
            g.pinned[g.n] = Some(s);
            g.n += 1;
        } else {
            // more slices than the guard holds: unpinned at once, the grace keeps them hot
            s.pins.fetch_sub(1, SeqCst);
        }
        counters().kernel_writes.fetch_add(1, Relaxed);
        s.heat(from, to, until);
    });
    g
}

/// Pins slices hot while a kernel write runs.
pub struct KernelWrite {
    pinned: [Option<&'static Slice>; 4],
    n: usize,
}

impl Drop for KernelWrite {
    fn drop(&mut self) {
        for s in self.pinned[..self.n].iter().flatten() {
            s.pins.fetch_sub(1, SeqCst);
        }
    }
}

/// The original protection of `va` if it is ours (read-only for write tracking only): what callers that ask the
/// protection of the title's memory must see.
pub fn original(va: u64) -> Option<u32> {
    let s = find(va)?;
    let b = s.block_of(va);
    s.is_ours(b).then(|| s.orig[b].load(Relaxed) as u32)
}

// ------------------------------------------------------------------------------------------ the handler

unsafe extern "system" fn handler(p: *mut EXCEPTION_POINTERS) -> i32 {
    const CONTINUE_EXECUTION: i32 = -1;
    const CONTINUE_SEARCH: i32 = 0;
    let r = unsafe { &*(*p).ExceptionRecord };
    if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION || r.NumberParameters < 2 || r.ExceptionInformation[0] != 1 {
        return CONTINUE_SEARCH;
    }
    let va = r.ExceptionInformation[1] as u64;
    let Some(s) = find(va) else { return CONTINUE_SEARCH };
    if s.fault(va) {
        return CONTINUE_EXECUTION;
    }
    // another thread made it writable first (a race on one block), or the page is not ours at all
    let mut m = MEMORY_BASIC_INFORMATION::default();
    if unsafe { VirtualQuery(Some(va as *const c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } != 0
        && m.State == MEM_COMMIT
        && m.Protect.0 & (PAGE_READWRITE.0 | PAGE_EXECUTE_READWRITE.0) != 0
    {
        return CONTINUE_EXECUTION;
    }
    CONTINUE_SEARCH
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Put the write-fault handler first in the vectored line (once). Slices go cold only once it is in, and only
/// when the caller also guards kernel writes ([`kernel_write`]); see [`enable`].
pub fn install() {
    if !INSTALLED.swap(true, SeqCst) {
        unsafe { AddVectoredExceptionHandler(1, Some(handler)) };
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Let slices go cold: the handler is installed and kernel writes are guarded. Without it they all stay hot
/// (scanned every pass), exactly as before.
pub fn enable() {
    install();
    ENABLED.store(true, SeqCst);
}

pub fn enabled() -> bool {
    ENABLED.load(SeqCst)
}

/// The slice table outlives the memory it covers (a slice freed and made again at one address keeps its
/// `hot_until` and `last_dirty`), and its tests pick free addresses to allocate at: the tests that make slices
/// cold (here and in `ring`) run one at a time.
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

/// Tests: `VirtualAlloc(kind, protect)` of `len` bytes at a [`SLICE`]-aligned address, so the memory owns its
/// slices (nothing another test allocates shares them). The free address found may be taken by another thread
/// before the allocation lands: then it looks again.
#[cfg(test)]
pub(crate) fn alloc_aligned(len: usize, kind: VIRTUAL_ALLOCATION_TYPE, protect: PAGE_PROTECTION_FLAGS) -> u64 {
    for _ in 0..1000 {
        let r = unsafe { VirtualAlloc(None, len + SLICE, MEM_RESERVE, PAGE_NOACCESS) } as u64;
        assert_ne!(r, 0, "reserve {} MiB", (len + SLICE) >> 20);
        unsafe { VirtualFree(r as *mut c_void, 0, MEM_RELEASE).unwrap() };
        let at = r.div_ceil(SLICE as u64) * SLICE as u64;
        let p = unsafe { VirtualAlloc(Some(at as *const c_void), len, kind, protect) } as u64;
        if p == at {
            return p;
        }
        if p != 0 {
            unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
        }
    }
    panic!("no {SLICE:#x}-aligned room for {len:#x} bytes in 1000 tries");
}

#[cfg(test)]
mod tests {
    use super::*;

    const WW: VIRTUAL_ALLOCATION_TYPE = VIRTUAL_ALLOCATION_TYPE(0x0020_0000);

    fn dirty(p: u64, len: usize) -> usize {
        let mut a = vec![std::ptr::null_mut(); len / 4096];
        let mut n = a.len();
        let mut g = 0u32;
        unsafe { GetWriteWatch(1, p as *const c_void, len, Some(a.as_mut_ptr()), Some(&mut n), Some(&mut g)) };
        n
    }

    /// Aligned watched memory of two slices.
    fn region() -> u64 {
        alloc_aligned(2 * SLICE, MEM_RESERVE | MEM_COMMIT | WW, PAGE_READWRITE)
    }

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_write_into_a_cold_slice_faults_once_and_is_seen_by_write_watch() {
        let _g = serial();
        enable();
        let p = region();
        let s = slice(p, true).unwrap();
        unsafe { ((p + 100) as *mut u8).write_volatile(1) };
        let mut late = 0;
        assert!(s.go_cold(&[(p, p + SLICE as u64)], |f, t| late += dirty(f, (t - f) as usize)));
        assert_eq!(late, 1, "the write before going cold is read by the last scan");
        assert_eq!(s.state(), COLD);
        assert_eq!(original(p + 5), Some(PAGE_READWRITE.0));
        let before = counters().faults.load(Relaxed);
        unsafe { ((p + 3 * BLOCK as u64 + 8) as *mut u64).write_volatile(7) };
        unsafe { ((p + 3 * BLOCK as u64 + 16) as *mut u64).write_volatile(8) };
        assert_eq!(counters().faults.load(Relaxed) - before, 1, "one fault a block");
        assert_eq!(s.state(), HOT);
        assert_eq!(dirty(p, SLICE), 1);
        assert_eq!(unsafe { ((p + 3 * BLOCK as u64 + 8) as *const u64).read_volatile() }, 7);
        assert_eq!(original(p + 3 * BLOCK as u64), None, "made writable again");
        assert_eq!(original(p), Some(PAGE_READWRITE.0), "the rest stays read-only");
        forget(p, 2 * SLICE as u64);
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
    }

    #[test]
    fn a_kernel_write_into_a_cold_slice_succeeds_after_the_guard() {
        let _g = serial();
        enable();
        let p = region();
        let s = slice(p + SLICE as u64, true).unwrap();
        let q = p + SLICE as u64;
        assert!(s.go_cold(&[(q, q + SLICE as u64)], |_, _| {}));
        // ReadProcessMemory into our own read-only page: the kernel's write fails without the guard
        let src = [5u8; 64];
        let me = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        let mut n = 0usize;
        let bad = unsafe { windows::Win32::System::Diagnostics::Debug::ReadProcessMemory(me, src.as_ptr() as *const c_void, (q + 4096) as *mut c_void, 64, Some(&mut n)) };
        assert!(bad.is_err(), "a kernel write into a cold page fails: the reason for the guard");
        {
            let _g = kernel_write(q + 4096, 64);
            assert_eq!(s.state(), HOT);
            unsafe { windows::Win32::System::Diagnostics::Debug::ReadProcessMemory(me, src.as_ptr() as *const c_void, (q + 4096) as *mut c_void, 64, Some(&mut n)).unwrap() };
            assert!(!s.may_go_cold(u64::MAX, 0), "pinned while the write runs");
        }
        assert_eq!(unsafe { ((q + 4096) as *const u8).read_volatile() }, 5);
        assert_eq!(dirty(q, SLICE), 1, "the kernel's write is in write-watch");
        assert!(!s.may_go_cold(now_ms(), 0), "kept hot for the grace after");
        forget(p, 2 * SLICE as u64);
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
    }

    #[test]
    fn a_commit_into_a_cold_slice_turns_it_hot() {
        let _g = serial();
        enable();
        let p = alloc_aligned(SLICE, MEM_RESERVE | WW, PAGE_READWRITE);
        unsafe { VirtualAlloc(Some(p as *const c_void), BLOCK, MEM_COMMIT, PAGE_READWRITE) };
        let s = slice(p, true).unwrap();
        assert!(s.go_cold(&[(p, p + SLICE as u64)], |_, _| {}), "a partly committed slice goes cold");
        unsafe { VirtualAlloc(Some((p + 4 * BLOCK as u64) as *const c_void), BLOCK, MEM_COMMIT, PAGE_READWRITE) };
        touch(p + 4 * BLOCK as u64, BLOCK as u64);
        assert_eq!(s.state(), HOT);
        forget(p, SLICE as u64);
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
    }

    #[test]
    fn write_copy_or_odd_protection_is_never_made_cold() {
        let _g = serial();
        enable();
        // reserved no-access, committed read-write: not answered from the descriptor (VirtualQuery's way)
        let p = alloc_aligned(BLOCK, MEM_RESERVE, PAGE_NOACCESS);
        unsafe { VirtualAlloc(Some(p as *const c_void), BLOCK, MEM_COMMIT, PAGE_READWRITE) };
        let mut old = PAGE_PROTECTION_FLAGS(0);
        // a 4 KiB writable page in a read-only block: not whole blocks
        unsafe { VirtualProtect(p as *const c_void, BLOCK, PAGE_READONLY, &mut old).unwrap() };
        unsafe { VirtualProtect(p as *const c_void, 4096, PAGE_READWRITE, &mut old).unwrap() };
        let s = slice(p, true).unwrap();
        assert!(!s.go_cold(&[(p, p + BLOCK as u64)], |_, _| {}));
        assert_eq!(s.state(), PINNED);
        unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE).unwrap() };
        assert!(slice(p, true).unwrap().state() == HOT, "fresh again for a new region");
    }
}
