//! Build 5: the D3D12.X command queue (docs/research/d3d12x-device.md, agents/d3d12x-child-slots.csv).
//!
//! The title-visible object: the method table first, the queue type at +0x10 (the title reads it right after
//! the header). Behind it, a host `ID3D12CommandQueue` of the same type. This title uses the newer table: from
//! 0x070 every slot is 0x10 later than the 2020 layout (Signal 0x080, PresentX 0x100, ExecuteCommandListsX 0x118).
//!
//! Written so far: IUnknown, private data, GetDevice, the profiler markers (no-ops), GetTimestampFrequency,
//! GetDesc. Submission, Signal, Wait and Present are traps until fences and the list glue land; a run names the
//! order the title needs them in ([`traps`]).

#![cfg(windows)]
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, D3D12_COMMAND_QUEUE_DESC};

pub const SLOTS: usize = 40;
const OBJECT_SIZE: usize = 0x40;
const E_NOTIMPL: u64 = 0x8000_4001;

/// One queue: the host queue and its creation description.
pub struct Queue {
    pub host: ID3D12CommandQueue,
    pub desc: D3D12_COMMAND_QUEUE_DESC,
}

// SAFETY: command queues are free-threaded.
unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

static QUEUES: Mutex<Option<HashMap<usize, &'static Queue>>> = Mutex::new(None);
static CALLS: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
static FIRST: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// The queue behind a title queue pointer.
pub fn find(object: usize) -> Option<&'static Queue> {
    QUEUES.lock().unwrap().as_ref()?.get(&object).copied()
}

fn note(slot: usize) {
    if CALLS[slot].fetch_add(1, Ordering::Relaxed) == 0 {
        let us = crate::device::get().map(|d| d.created.elapsed().as_micros() as u64).unwrap_or(0);
        FIRST[slot].store(us + 1, Ordering::Relaxed);
    }
}

fn slot_name(off: usize) -> &'static str {
    match off {
        0x040 => "UpdateTileMappings",
        0x048 => "CopyTileMappings",
        0x050 => "ExecuteCommandLists",
        0x070 | 0x078 => "console (unknown)",
        0x080 => "Signal",
        0x088 => "Wait",
        0x098 => "GetClockCalibration",
        0x0b8 | 0x0c0 => "suspend / resume",
        0x0f8 => "page mapping batch",
        0x100 => "PresentX",
        0x110 => "SignalX",
        0x118 => "ExecuteCommandListsX",
        _ => "?",
    }
}

fn implemented(off: usize) -> bool {
    matches!(off, 0x000..=0x038 | 0x058..=0x068 | 0x090 | 0x0e0)
}

/// Queue slots called but not written: (offset, name, calls, first call in ms after the device).
pub fn traps() -> Vec<(usize, &'static str, u32, f64)> {
    let mut v: Vec<_> = (0..SLOTS)
        .filter(|&s| !implemented(s * 8) && CALLS[s].load(Ordering::Relaxed) > 0)
        .map(|s| (s * 8, slot_name(s * 8), CALLS[s].load(Ordering::Relaxed), (FIRST[s].load(Ordering::Relaxed) - 1) as f64 / 1000.0))
        .collect();
    v.sort_by(|a, b| a.3.total_cmp(&b.3));
    v
}

type Trap = unsafe extern "system" fn(*mut c_void, usize, usize, usize, usize, usize, usize) -> u64;

unsafe extern "system" fn trap<const SLOT: usize>(_t: *mut c_void, _a: usize, _b: usize, _c: usize, _d: usize, _e: usize, _f: usize) -> u64 {
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
    // the title asks queues for their own interfaces only (the fork never saw another)
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

/// ID3D12DeviceChild::GetDevice: the one device.
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

unsafe extern "system" fn Marker(_t: *mut c_void, _a: usize, _b: usize, _c: usize) {}

unsafe extern "system" fn GetTimestampFrequency(this: *mut c_void, out: *mut u64) -> i32 {
    note(0x90 / 8);
    let Some(q) = find(this as usize) else { return 0x8000_4005u32 as i32 };
    match unsafe { q.host.GetTimestampFrequency() } {
        Ok(f) if !out.is_null() => {
            unsafe { *out = f };
            0
        }
        Ok(_) => 0x8000_4003u32 as i32,
        Err(e) => e.code().0,
    }
}

/// GetDesc returns the description through a hidden pointer (MSVC member-function struct return).
unsafe extern "system" fn GetDesc(this: *mut c_void, out: *mut D3D12_COMMAND_QUEUE_DESC) -> *mut D3D12_COMMAND_QUEUE_DESC {
    note(0xe0 / 8);
    if let Some(q) = find(this as usize) {
        unsafe { *out = q.desc };
    }
    out
}

static VTABLE: OnceLock<Box<[usize; SLOTS]>> = OnceLock::new();

fn vtable() -> &'static [usize; SLOTS] {
    VTABLE.get_or_init(|| {
        let traps: [Trap; SLOTS] =
            table!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39);
        let mut t = Box::new([0usize; SLOTS]);
        for (i, f) in traps.into_iter().enumerate() {
            t[i] = f as usize;
        }
        t[0] = QueryInterface as *const () as usize;
        t[1] = AddRef as *const () as usize;
        t[2] = Release as *const () as usize;
        t[3] = GetPrivateData as *const () as usize;
        t[4] = SetPrivateData as *const () as usize;
        t[5] = SetPrivateDataInterface as *const () as usize;
        t[6] = SetName as *const () as usize;
        t[7] = GetDevice as *const () as usize;
        for s in [0x58 / 8, 0x60 / 8, 0x68 / 8] {
            t[s] = Marker as *const () as usize;
        }
        t[0x90 / 8] = GetTimestampFrequency as *const () as usize;
        t[0xe0 / 8] = GetDesc as *const () as usize;
        t
    })
}

/// A new queue for the title: the host queue of the same description, the console object around it.
pub fn create(desc: &D3D12_COMMAND_QUEUE_DESC) -> Result<usize, i32> {
    let dev = crate::device::get().ok_or(0x8000_4005u32 as i32)?;
    let host: ID3D12CommandQueue = unsafe { dev.host.CreateCommandQueue(desc) }.map_err(|e| e.code().0)?;
    let object = Box::leak(Box::new([0u8; OBJECT_SIZE]));
    unsafe {
        (object.as_mut_ptr() as *mut usize).write(vtable().as_ptr() as usize);
        (object.as_mut_ptr().add(0x10) as *mut u32).write(desc.Type.0 as u32);
    }
    let q: &'static Queue = Box::leak(Box::new(Queue { host, desc: *desc }));
    let key = object.as_ptr() as usize;
    QUEUES.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, q);
    Ok(key)
}
