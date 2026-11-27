//! Build 5: the D3D12.X fence (docs/research/d3d12x-device.md; agents/d3d12x-child-slots.csv "fence").
//!
//! The title reads a fence's value straight from memory, at the address `GetGpuAddress` (0x60) returns, as well as
//! through `GetCompletedValue`. So each fence has a value cell in its console object (+0x10), handed out by 0x60,
//! and a host `ID3D12Fence` for the real synchronisation. A watcher thread per fence sleeps on the host fence and
//! writes the completed value into the cell as the GPU passes each signal, so the title's direct reads see
//! progress. (A later step can let the GPU write the cell itself, the Build 2 label path, if the frame line shows
//! the watcher's latency matters.)
//!
//! The title's pointer leads to our [`Fence`] without a lock (+0x38 of the object, after the method table checks
//! out), and a list's memory wait on a fence's value cell is found by [`by_cell`] (arithmetic on the arena that
//! holds every fence object, no lock, no scan), so submission can make it a host queue wait on the host fence.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, D3D12_FENCE_FLAG_NONE};
use windows::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

pub const SLOTS: usize = 24;
const OBJECT_SIZE: usize = 0x40;
const E_NOTIMPL: u64 = 0x8000_4001;
const E_OUTOFMEMORY: i32 = 0x8007_000Eu32 as i32;

pub struct Fence {
    pub host: ID3D12Fence,
    /// The title-visible value (object + 0x10).
    cell: usize,
    /// The highest value the GPU has been asked to signal, for the watcher.
    pending: Mutex<u64>,
    wake: Condvar,
}

// SAFETY: fences are free-threaded; the cell is written by one watcher and read by the title.
unsafe impl Send for Fence {}
unsafe impl Sync for Fence {}

impl Fence {
    fn cell(&self) -> &AtomicU64 {
        // SAFETY: the cell lives in the leaked console object for the run, 8-byte aligned.
        unsafe { &*(self.cell as *const AtomicU64) }
    }

    /// The GPU will signal `value` on this fence (a queue Signal was just issued): let the watcher wait for it.
    pub fn expect(&self, value: u64) {
        let mut p = self.pending.lock().unwrap();
        if value > *p {
            *p = value;
            self.wake.notify_one();
        }
    }

    /// Refresh the cell from the host fence (never backwards).
    fn refresh(&self) -> u64 {
        let v = unsafe { self.host.GetCompletedValue() };
        self.cell().fetch_max(v, Ordering::AcqRel);
        v
    }
}

/// Where the console object keeps its [`Fence`].
const OURS: usize = 0x38;
/// Fences the arena holds. Each fence has a watcher thread, so a run cannot come near this; past it `CreateFence`
/// fails (E_OUTOFMEMORY) rather than hand out a fence whose cell [`by_cell`] cannot find.
const MAX_FENCES: usize = 1 << 20;
static CALLS: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

/// Every fence's console object, back to back in one reserved range (64 MiB of addresses at most, committed a page
/// at a time as fences are made). An object is published by the count's release store after it is written, so
/// [`Arena::object`] is a subtraction, two compares and one acquire load: no lock and no scan on the submit path.
struct Arena {
    base: usize,
    capacity: usize,
    count: AtomicUsize,
    adding: Mutex<()>,
}

impl Arena {
    fn reserve(capacity: usize) -> Option<Arena> {
        let base = unsafe { VirtualAlloc(None, capacity * OBJECT_SIZE, MEM_RESERVE, PAGE_READWRITE) } as usize;
        (base != 0).then(|| Arena { base, capacity, count: AtomicUsize::new(0), adding: Mutex::new(()) })
    }

    /// A new zeroed object, written by `init` before any lookup can find it; `None` when the arena is full.
    fn add(&self, init: impl FnOnce(usize)) -> Option<usize> {
        let _one = self.adding.lock().unwrap_or_else(|e| e.into_inner());
        let i = self.count.load(Ordering::Relaxed);
        if i >= self.capacity {
            return None;
        }
        let object = self.base + i * OBJECT_SIZE;
        // committing a page twice is allowed; fresh pages read as zero
        if unsafe { VirtualAlloc(Some(object as *const c_void), OBJECT_SIZE, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
            return None;
        }
        init(object);
        self.count.store(i + 1, Ordering::Release);
        Some(object)
    }

    /// `object`, if it is a published object of this arena.
    #[inline]
    fn object(&self, object: usize) -> Option<usize> {
        let off = object.checked_sub(self.base)?;
        (off % OBJECT_SIZE == 0 && off / OBJECT_SIZE < self.count.load(Ordering::Acquire)).then_some(object)
    }
}

static ARENA: OnceLock<Option<Arena>> = OnceLock::new();

fn arena() -> Option<&'static Arena> {
    ARENA.get_or_init(|| Arena::reserve(MAX_FENCES)).as_ref()
}

/// The fence behind a title fence pointer: read from the object, no lock.
#[inline]
pub fn find(object: usize) -> Option<&'static Fence> {
    if object == 0 || object % 8 != 0 {
        return None;
    }
    let table = VTABLE.get()?.as_ptr() as usize;
    // SAFETY: a pointer the title says is a fence; +0x38 is read only once the table shows it is ours.
    unsafe {
        if *(object as *const usize) != table {
            return None;
        }
        ((object + OURS) as *const *const Fence).read().as_ref()
    }
}

/// The fence whose value cell (the address `GetGpuAddress` handed out) is `address`.
#[inline]
pub fn by_cell(address: u64) -> Option<&'static Fence> {
    let object = (address as usize).checked_sub(0x10)?;
    find(ARENA.get()?.as_ref()?.object(object)?)
}

impl Fence {
    /// The title-visible value.
    pub fn value(&self) -> u64 {
        self.cell().load(Ordering::Acquire)
    }
}

fn note(slot: usize) {
    CALLS[slot].fetch_add(1, Ordering::Relaxed);
}

fn implemented(off: usize) -> bool {
    off <= 0x060
}

/// Fence slots called but not written: (offset, calls).
pub fn traps() -> Vec<(usize, u32)> {
    (0..SLOTS).filter(|&s| !implemented(s * 8) && CALLS[s].load(Ordering::Relaxed) > 0).map(|s| (s * 8, CALLS[s].load(Ordering::Relaxed))).collect()
}

type Trap = unsafe extern "system" fn(*mut c_void, usize, usize, usize, usize) -> u64;

unsafe extern "system" fn trap<const SLOT: usize>(_t: *mut c_void, _a: usize, _b: usize, _c: usize, _d: usize) -> u64 {
    note(SLOT);
    E_NOTIMPL
}

macro_rules! table {
    ($($n:literal)*) => { [$(trap::<$n> as Trap),*] };
}

unsafe extern "system" fn QueryInterface(this: *mut c_void, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    note(0);
    if out.is_null() {
        return 0x8000_4003u32 as i32;
    }
    unsafe { *out = this };
    0
}
unsafe extern "system" fn AddRef(_t: *mut c_void) -> u32 {
    note(1);
    2
}
unsafe extern "system" fn Release(_t: *mut c_void) -> u32 {
    note(2);
    1
}
unsafe extern "system" fn GetPrivateData(_t: *mut c_void, _g: *const c_void, size: *mut u32, _d: *mut c_void) -> i32 {
    note(3);
    if !size.is_null() {
        unsafe { *size = 0 };
    }
    0x887A_0002u32 as i32
}
unsafe extern "system" fn SetPrivateData(_t: *mut c_void, _g: *const c_void, _s: u32, _d: *const c_void) -> i32 {
    note(4);
    0
}
unsafe extern "system" fn SetPrivateDataInterface(_t: *mut c_void, _g: *const c_void, _d: *mut c_void) -> i32 {
    note(5);
    0
}
unsafe extern "system" fn SetName(_t: *mut c_void, _n: *const u16) -> i32 {
    note(6);
    0
}
unsafe extern "system" fn GetDevice(_t: *mut c_void, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    note(7);
    match crate::device::get() {
        Some(d) if !out.is_null() => {
            unsafe { *out = d.object as *mut c_void };
            0
        }
        _ => 0x8000_4005u32 as i32,
    }
}

unsafe extern "system" fn GetCompletedValue(this: *mut c_void) -> u64 {
    note(8);
    find(this as usize).map(|f| f.refresh()).unwrap_or(0)
}

/// A null event blocks until the value is reached (D3D12's rule, which the host fence follows).
unsafe extern "system" fn SetEventOnCompletion(this: *mut c_void, value: u64, event: HANDLE) -> i32 {
    note(9);
    let Some(f) = find(this as usize) else { return 0x8000_4005u32 as i32 };
    let blocking = event.is_invalid() || event.0.is_null();
    let started = blocking.then(std::time::Instant::now);
    let r = match unsafe { f.host.SetEventOnCompletion(value, event) } {
        Ok(()) => 0,
        Err(e) => e.code().0,
    };
    if let Some(t) = started {
        crate::framelog::waited(t.elapsed());
        f.refresh();
    }
    r
}

unsafe extern "system" fn Signal(this: *mut c_void, value: u64) -> i32 {
    note(10);
    let Some(f) = find(this as usize) else { return 0x8000_4005u32 as i32 };
    match unsafe { f.host.Signal(value) } {
        Ok(()) => {
            f.refresh();
            0
        }
        Err(e) => e.code().0,
    }
}

unsafe extern "system" fn GetCreationFlags(_this: *mut c_void) -> u32 {
    note(11);
    0
}

/// The address the title reads the value at.
unsafe extern "system" fn GetGpuAddress(this: *mut c_void) -> u64 {
    note(12);
    find(this as usize).map(|f| f.cell as u64).unwrap_or(0)
}

static VTABLE: OnceLock<Box<[usize; SLOTS]>> = OnceLock::new();

fn vtable() -> &'static [usize; SLOTS] {
    VTABLE.get_or_init(|| {
        let traps: [Trap; SLOTS] = table!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23);
        let mut t = Box::new([0usize; SLOTS]);
        for (i, f) in traps.into_iter().enumerate() {
            t[i] = f as usize;
        }
        let impls: [*const (); 13] = [
            QueryInterface as *const (),
            AddRef as *const (),
            Release as *const (),
            GetPrivateData as *const (),
            SetPrivateData as *const (),
            SetPrivateDataInterface as *const (),
            SetName as *const (),
            GetDevice as *const (),
            GetCompletedValue as *const (),
            SetEventOnCompletion as *const (),
            Signal as *const (),
            GetCreationFlags as *const (),
            GetGpuAddress as *const (),
        ];
        for (i, f) in impls.into_iter().enumerate() {
            t[i] = f as usize;
        }
        t
    })
}

/// The watcher: waits on the host fence for the highest expected value and writes the cell as it completes.
fn watch(f: &'static Fence) {
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else { return };
    loop {
        let target = {
            let mut p = f.pending.lock().unwrap();
            while *p <= f.cell().load(Ordering::Acquire) {
                p = f.wake.wait(p).unwrap();
            }
            *p
        };
        if unsafe { f.host.SetEventOnCompletion(target, event) }.is_err() {
            break;
        }
        unsafe { WaitForSingleObject(event, INFINITE) };
        f.refresh();
    }
    unsafe {
        let _ = CloseHandle(event);
    }
}

/// `CreateFence(initial, flags, iid, out)`: the console object around a host fence, and its watcher.
pub fn create(initial: u64) -> Result<usize, i32> {
    let dev = crate::device::get().ok_or(0x8000_4005u32 as i32)?;
    let host: ID3D12Fence = unsafe { dev.host.CreateFence(initial, D3D12_FENCE_FLAG_NONE) }.map_err(|e| e.code().0)?;
    let mut made = None;
    let base = arena()
        .and_then(|a| {
            a.add(|base| {
                let f: &'static Fence = Box::leak(Box::new(Fence { host, cell: base + 0x10, pending: Mutex::new(initial), wake: Condvar::new() }));
                unsafe {
                    (base as *mut usize).write(vtable().as_ptr() as usize);
                    ((base + 0x10) as *mut u64).write(initial);
                    ((base + OURS) as *mut *const Fence).write(f);
                }
                made = Some(f);
            })
        })
        .ok_or(E_OUTOFMEMORY)?;
    let f = made.ok_or(E_OUTOFMEMORY)?;
    std::thread::Builder::new().name("fence-watch".into()).stack_size(64 << 10).spawn(move || watch(f)).map_err(|_| 0x8000_4005u32 as i32)?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_object_is_found_by_arithmetic_past_the_old_4096() {
        let a = Arena::reserve(5000).expect("reserve");
        let mut made = Vec::new();
        for i in 0..5000u64 {
            made.push(a.add(|o| unsafe { ((o + 0x10) as *mut u64).write(i) }).expect("room"));
        }
        assert!(a.add(|_| panic!("written past the end")).is_none(), "a full arena refuses");
        for (i, &o) in made.iter().enumerate() {
            assert_eq!(a.object(o), Some(o));
            assert_eq!(unsafe { ((o + 0x10) as *const u64).read() }, i as u64);
            assert_eq!(a.object(o + 8), None, "inside an object");
        }
        assert_eq!(a.object(a.base - OBJECT_SIZE), None);
        assert_eq!(a.object(a.base + 5000 * OBJECT_SIZE), None);
    }

    #[test]
    fn an_object_not_yet_made_is_not_found() {
        let a = Arena::reserve(8).expect("reserve");
        assert_eq!(a.object(a.base), None, "nothing published yet (and the page is not committed)");
        let o = a.add(|_| {}).unwrap();
        assert_eq!(o, a.base);
        assert_eq!(a.object(a.base + OBJECT_SIZE), None);
        // fresh objects read as zero
        assert!((0..OBJECT_SIZE).step_by(8).all(|k| unsafe { ((o + k) as *const u64).read() } == 0));
    }

    #[test]
    fn a_cell_outside_every_fence_is_not_found() {
        assert!(by_cell(0).is_none());
        assert!(by_cell(0x10).is_none());
        let local = 0u64;
        assert!(by_cell(&local as *const u64 as u64).is_none());
    }
}
