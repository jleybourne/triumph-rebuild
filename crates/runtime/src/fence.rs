//! Build 5: the D3D12.X fence (docs/research/d3d12x-device.md; agents/d3d12x-child-slots.csv "fence").
//!
//! The title reads a fence's value straight from memory, at the address `GetGpuAddress` (0x60) returns, as well as
//! through `GetCompletedValue`. So each fence has a value cell in its console object (+0x10), handed out by 0x60,
//! and a host `ID3D12Fence` for the real synchronisation. A watcher thread per fence sleeps on the host fence and
//! writes the completed value into the cell as the GPU passes each signal, so the title's direct reads see
//! progress. (A later step can let the GPU write the cell itself, the Build 2 label path, if the frame line shows
//! the watcher's latency matters.)

#![cfg(windows)]
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, D3D12_FENCE_FLAG_NONE};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

pub const SLOTS: usize = 24;
const OBJECT_SIZE: usize = 0x40;
const E_NOTIMPL: u64 = 0x8000_4001;

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

static FENCES: Mutex<Option<HashMap<usize, &'static Fence>>> = Mutex::new(None);
static CALLS: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

pub fn find(object: usize) -> Option<&'static Fence> {
    FENCES.lock().unwrap().as_ref()?.get(&object).copied()
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
    let r = match unsafe { f.host.SetEventOnCompletion(value, event) } {
        Ok(()) => 0,
        Err(e) => e.code().0,
    };
    if event.is_invalid() || event.0.is_null() {
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
    let object = Box::leak(Box::new([0u64; OBJECT_SIZE / 8]));
    let base = object.as_mut_ptr() as usize;
    unsafe {
        (base as *mut usize).write(vtable().as_ptr() as usize);
        ((base + 0x10) as *mut u64).write(initial);
    }
    let f: &'static Fence = Box::leak(Box::new(Fence { host, cell: base + 0x10, pending: Mutex::new(initial), wake: Condvar::new() }));
    FENCES.lock().unwrap().get_or_insert_with(HashMap::new).insert(base, f);
    std::thread::Builder::new().name("fence-watch".into()).stack_size(64 << 10).spawn(move || watch(f)).map_err(|_| 0x8000_4005u32 as i32)?;
    Ok(base)
}
