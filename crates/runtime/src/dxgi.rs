//! Build 5: the DXGI objects the title reaches from the device (docs/research/d3d12x-device.md, "DXGI"):
//! device `QueryInterface` → IDXGIDevice (0x38 GetAdapter) → adapter (0x38 EnumOutputs, 0x40 GetDesc) → output
//! (0xa8: the console's display-mode list). The console's output table differs from the PC's at 0xa8, so these
//! are our own console-shaped objects, not the host's DXGI. One adapter, one output, the host display's mode.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

const SLOTS: usize = 32;
const E_NOTIMPL: u64 = 0x8000_4001;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const NOT_FOUND: i32 = 0x887A_0002u32 as i32;
const MORE_DATA: i32 = 0x887A_0003u32 as i32;

const DEVICE: usize = 0;
const ADAPTER: usize = 1;
const FACTORY: usize = 2;
const OUTPUT: usize = 3;
const NAMES: [&str; 4] = ["DXGI device", "adapter", "factory", "output"];

static CALLS: [[AtomicU32; SLOTS]; 4] = [const { [const { AtomicU32::new(0) }; SLOTS] }; 4];

/// The four objects: each is one word, its method table.
static OBJECTS: OnceLock<[Box<usize>; 4]> = OnceLock::new();

fn object(kind: usize) -> *mut c_void {
    let o = OBJECTS.get_or_init(|| {
        [Box::new(table(DEVICE).as_ptr() as usize), Box::new(table(ADAPTER).as_ptr() as usize), Box::new(table(FACTORY).as_ptr() as usize), Box::new(table(OUTPUT).as_ptr() as usize)]
    });
    &*o[kind] as *const usize as *mut c_void
}

/// The DXGI device, for the D3D12 device's `QueryInterface`.
pub fn device() -> *mut c_void {
    object(DEVICE)
}

/// IDXGIDevice, IDXGIDevice1, IDXGIDevice2 (by their first word: the title asks newest first and takes what works).
pub fn is_dxgi_device(iid: &[u8; 16]) -> bool {
    matches!(u32::from_le_bytes([iid[0], iid[1], iid[2], iid[3]]), 0x54ec_77fa | 0x77db_970f | 0x0500_8617)
}

fn implemented(kind: usize, slot: usize) -> bool {
    matches!((kind, slot * 8), (_, 0x00..=0x10) | (_, 0x30) | (DEVICE, 0x38) | (ADAPTER, 0x38 | 0x40) | (FACTORY, 0x38 | 0x60) | (OUTPUT, 0xa8))
}

/// Slots called but not written: (object, offset, calls).
pub fn traps() -> Vec<(&'static str, usize, u32)> {
    let mut v = Vec::new();
    for k in 0..4 {
        for s in 0..SLOTS {
            let n = CALLS[k][s].load(Ordering::Relaxed);
            if n > 0 && !implemented(k, s) {
                v.push((NAMES[k], s * 8, n));
            }
        }
    }
    v
}

type Trap = unsafe extern "system" fn(*mut c_void, usize, usize, usize, usize) -> u64;

unsafe extern "system" fn trap<const K: usize, const S: usize>(_t: *mut c_void, _a: usize, _b: usize, _c: usize, _d: usize) -> u64 {
    CALLS[K][S].fetch_add(1, Ordering::Relaxed);
    E_NOTIMPL
}

macro_rules! traps_of {
    ($k:literal; $($n:literal)*) => { [$(trap::<$k, $n> as Trap),*] };
}

fn trap_table(kind: usize) -> [Trap; SLOTS] {
    match kind {
        0 => traps_of!(0; 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31),
        1 => traps_of!(1; 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31),
        2 => traps_of!(2; 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31),
        _ => traps_of!(3; 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31),
    }
}

fn answer(found: *mut c_void, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return 0x8000_4003u32 as i32;
    }
    unsafe { *out = found };
    if found.is_null() { E_NOINTERFACE } else { 0 }
}

unsafe extern "system" fn AddRef(_t: *mut c_void) -> u32 {
    2
}
unsafe extern "system" fn Release(_t: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn device_query(this: *mut c_void, iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    CALLS[DEVICE][0].fetch_add(1, Ordering::Relaxed);
    if iid.is_null() {
        return 0x8000_4003u32 as i32;
    }
    let g = unsafe { &*iid };
    if is_dxgi_device(g) {
        return answer(this, out);
    }
    // the D3D12 device, asked for from the DXGI device
    match crate::device::get() {
        Some(d) if crate::device::is_device_iid_pub(g) => answer(d.object as *mut c_void, out),
        _ => answer(std::ptr::null_mut(), out),
    }
}
unsafe extern "system" fn device_parent(_t: *mut c_void, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    answer(object(ADAPTER), out)
}
unsafe extern "system" fn device_adapter(_t: *mut c_void, out: *mut *mut c_void) -> i32 {
    answer(object(ADAPTER), out)
}

/// Adapter, factory and output answer for themselves whatever version is asked.
unsafe extern "system" fn self_query(this: *mut c_void, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    answer(this, out)
}
unsafe extern "system" fn adapter_parent(_t: *mut c_void, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    answer(object(FACTORY), out)
}
unsafe extern "system" fn adapter_outputs(_t: *mut c_void, index: u32, out: *mut *mut c_void) -> i32 {
    if index != 0 {
        if !out.is_null() {
            unsafe { *out = std::ptr::null_mut() };
        }
        return NOT_FOUND;
    }
    answer(object(OUTPUT), out)
}

/// `GetDesc`: the host adapter's own description (DXGI_ADAPTER_DESC, 304 bytes).
unsafe extern "system" fn adapter_desc(_t: *mut c_void, out: *mut windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_DESC) -> i32 {
    if out.is_null() {
        return 0x8007_0057u32 as i32;
    }
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let desc = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.and_then(|f| unsafe { f.EnumAdapters1(0) }).and_then(|a| unsafe { a.GetDesc() });
    match desc {
        Ok(d) => {
            unsafe { *out = d };
            0
        }
        Err(e) => e.code().0,
    }
}
unsafe extern "system" fn factory_adapters(_t: *mut c_void, index: u32, out: *mut *mut c_void) -> i32 {
    if index != 0 {
        if !out.is_null() {
            unsafe { *out = std::ptr::null_mut() };
        }
        return NOT_FOUND;
    }
    answer(object(ADAPTER), out)
}
unsafe extern "system" fn output_parent(_t: *mut c_void, _iid: *const [u8; 16], out: *mut *mut c_void) -> i32 {
    answer(object(ADAPTER), out)
}

/// A display mode as the console lists it (32 bytes): size, refresh (numerator, denominator), format, scanline
/// order, scaling, flags.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh: [u32; 2],
    pub format: u32,
    pub scanlines: u32,
    pub scaling: u32,
    pub flags: u32,
}
const _: () = assert!(std::mem::size_of::<Mode>() == 32);

/// The host display's current mode: what the console reports for the television it is plugged into.
pub fn host_mode() -> Mode {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool();
    let (w, h, hz) = if ok && dm.dmPelsWidth > 0 { (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency.max(1)) } else { (3840, 2160, 60) };
    Mode { width: w, height: h, refresh: [hz, 1], format: 0, scanlines: 1, scaling: 0, flags: 0 }
}

/// 0xa8: the mode list, one mode (the host display's). Null `out` asks for the count.
unsafe extern "system" fn output_modes(_t: *mut c_void, _format: u32, _flags: u32, count: *mut u32, out: *mut Mode) -> i32 {
    CALLS[OUTPUT][0xa8 / 8].fetch_add(1, Ordering::Relaxed);
    let Some(count) = (unsafe { count.as_mut() }) else { return 0x8007_0057u32 as i32 };
    if out.is_null() {
        *count = 1;
        return 0;
    }
    if *count < 1 {
        return MORE_DATA;
    }
    *count = 1;
    unsafe { out.write(host_mode()) };
    0
}

static TABLES: OnceLock<[Box<[usize; SLOTS]>; 4]> = OnceLock::new();

fn table(kind: usize) -> &'static [usize; SLOTS] {
    &TABLES.get_or_init(|| {
        let mut all: [Box<[usize; SLOTS]>; 4] = std::array::from_fn(|k| {
            let mut t = Box::new([0usize; SLOTS]);
            for (i, f) in trap_table(k).into_iter().enumerate() {
                t[i] = f as usize;
            }
            t[1] = AddRef as *const () as usize;
            t[2] = Release as *const () as usize;
            t
        });
        let set = |t: &mut [usize; SLOTS], off: usize, f: *const ()| t[off / 8] = f as usize;
        set(&mut all[DEVICE], 0x00, device_query as *const ());
        set(&mut all[DEVICE], 0x30, device_parent as *const ());
        set(&mut all[DEVICE], 0x38, device_adapter as *const ());
        set(&mut all[ADAPTER], 0x00, self_query as *const ());
        set(&mut all[ADAPTER], 0x30, adapter_parent as *const ());
        set(&mut all[ADAPTER], 0x38, adapter_outputs as *const ());
        set(&mut all[ADAPTER], 0x40, adapter_desc as *const ());
        set(&mut all[FACTORY], 0x00, self_query as *const ());
        set(&mut all[FACTORY], 0x38, factory_adapters as *const ());
        set(&mut all[FACTORY], 0x60, factory_adapters as *const ());
        set(&mut all[OUTPUT], 0x00, self_query as *const ());
        set(&mut all[OUTPUT], 0x30, output_parent as *const ());
        set(&mut all[OUTPUT], 0xa8, output_modes as *const ());
        all
    })[kind]
}

#[cfg(test)]
mod tests {
    use super::*;

    type Get = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32;
    type Indexed = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32;
    type Modes = unsafe extern "system" fn(*mut c_void, u32, u32, *mut u32, *mut Mode) -> i32;

    fn slot<T>(obj: *mut c_void, off: usize) -> T {
        unsafe {
            let table = *(obj as *const *const usize);
            std::mem::transmute_copy::<usize, T>(&*table.add(off / 8))
        }
    }

    #[test]
    fn device_to_adapter_to_output_to_the_mode_list() {
        let dxgi = device();
        let mut adapter = std::ptr::null_mut();
        assert_eq!(unsafe { slot::<Get>(dxgi, 0x38)(dxgi, &mut adapter) }, 0);
        let mut output = std::ptr::null_mut();
        assert_eq!(unsafe { slot::<Indexed>(adapter, 0x38)(adapter, 0, &mut output) }, 0);
        let mut none = std::ptr::null_mut();
        assert_eq!(unsafe { slot::<Indexed>(adapter, 0x38)(adapter, 1, &mut none) }, NOT_FOUND);
        let modes = slot::<Modes>(output, 0xa8);
        let mut n = 0u32;
        assert_eq!(unsafe { modes(output, 0, 0, &mut n, std::ptr::null_mut()) }, 0);
        assert_eq!(n, 1);
        let mut m = [Mode { width: 0, height: 0, refresh: [0, 0], format: 0, scanlines: 0, scaling: 0, flags: 0 }];
        assert_eq!(unsafe { modes(output, 0, 0, &mut n, m.as_mut_ptr()) }, 0);
        assert!(m[0].width >= 640 && m[0].height >= 360 && m[0].refresh[0] >= 1, "{:?}", m[0]);
    }
}
