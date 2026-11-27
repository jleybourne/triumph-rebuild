//! Runtime objects as title code calls them, built from the declarative [`InterfaceDef`]s in
//! [`crate::table`]: one static object per interface, whose method table has our function in every
//! slot the definition names and a discovery handler (logs area and slot, returns `E_NOTIMPL`) in
//! every other. The objects never die, so `AddRef` / `Release` count nothing.

use std::ffi::{c_char, c_void, CStr};
use std::sync::OnceLock;

use crate::discovery;
use crate::guid::Guid;
use crate::hr::*;

/// A function address in a static table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fp(pub *const ());
// SAFETY: a code address; nothing is shared through it.
unsafe impl Sync for Fp {}
unsafe impl Send for Fp {}

/// One implemented method: its byte offset in the table, the public API name it stands for, our
/// function, and the arguments in console order after `this`.
#[derive(Debug)]
pub struct Slot {
    pub offset: usize,
    pub name: &'static str,
    pub func: Fp,
    pub args: &'static str,
}

/// One interface a title may ask `QueryApiImpl` for.
#[derive(Debug)]
pub struct InterfaceDef {
    /// Named in discovery lines.
    pub area: &'static str,
    pub provider: Guid,
    /// Every interface id answered with this object.
    pub iids: &'static [Guid],
    /// Number of slots in the method table (slots past it are not ours to guess).
    pub len: usize,
    pub slots: &'static [Slot],
}

/// What `QueryApiImpl` hands out.
#[repr(C)]
pub struct Object {
    vtable: *const Fp,
    pub def: &'static InterfaceDef,
    /// Slots laid out: the definition's, then [`SLACK`] discovery slots (up to 80), so a call past
    /// what we know is named instead of reading past the table.
    len: usize,
}

pub const SLACK: usize = 8;
// SAFETY: the table is leaked, immutable function addresses.
unsafe impl Sync for Object {}
unsafe impl Send for Object {}

impl Object {
    fn build(def: &'static InterfaceDef) -> Object {
        assert!(def.len >= 3 && def.len <= MISSING.len(), "{}: table length", def.area);
        let len = (def.len + SLACK).min(MISSING.len());
        let mut table: Vec<Fp> = MISSING[..len].to_vec();
        table[1] = Fp(add_ref as *const ());
        table[2] = Fp(add_ref as *const ());
        for slot in def.slots {
            assert!(slot.offset % 8 == 0 && slot.offset >= 0x18, "{}: {}", def.area, slot.name);
            assert!(slot.offset / 8 < def.len, "{}: {} past the table", def.area, slot.name);
            table[slot.offset / 8] = slot.func;
        }
        Object { vtable: Box::leak(table.into_boxed_slice()).as_ptr(), def, len }
    }

    /// The function in the slot at byte `offset`, as title code would load it.
    pub fn slot(&self, offset: usize) -> *const () {
        assert!(offset % 8 == 0 && offset / 8 < self.len);
        // SAFETY: inside the table, checked above.
        unsafe { (*self.vtable.add(offset / 8)).0 }
    }

    /// The pointer the title receives.
    pub fn as_ptr(&'static self) -> *mut c_void {
        (self as *const Object).cast_mut().cast()
    }
}

static OBJECTS: OnceLock<Vec<Object>> = OnceLock::new();

pub fn objects() -> &'static [Object] {
    OBJECTS.get_or_init(|| crate::table::ALL.iter().map(|d| Object::build(d)).collect())
}

/// The object for (provider, iid), if we serve it.
pub fn find(provider: &Guid, iid: &Guid) -> Option<&'static Object> {
    objects()
        .iter()
        .find(|o| o.def.provider == *provider && o.def.iids.contains(iid))
}

/// The object for `def`.
pub fn of(def: &'static InterfaceDef) -> &'static Object {
    objects()
        .iter()
        .find(|o| std::ptr::eq(o.def, def))
        .expect("every definition is in table::ALL")
}

unsafe extern "system" fn add_ref(_: *const Object) -> u32 {
    1
}

/// A slot no title has been seen to need. Wrong for a slot returning `bool` or a value; that is
/// what the discovery line is for.
unsafe extern "system" fn missing<const SLOT: usize>(this: *const Object) -> HRESULT {
    // SAFETY: title code calls methods on objects we handed out.
    let area = unsafe { this.as_ref() }.map_or("?", |o| o.def.area);
    discovery::note(format!("slot not implemented: {area} {:#x}", SLOT * 8));
    E_NOTIMPL
}

macro_rules! missing_table {
    ($($n:literal)*) => {
        static MISSING: [Fp; 80] = [$(Fp(missing::<$n> as *const ())),*];
    };
}
missing_table!(
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35
    36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68
    69 70 71 72 73 74 75 76 77 78 79
);

// Helpers for slot functions.

/// Writes `value` through `out`; `E_INVALIDARG` for null.
///
/// # Safety
/// `out` is null or has room for a `T`.
pub unsafe fn put<T>(out: *mut T, value: T) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { out.write_unaligned(value) };
    S_OK
}

/// Copies `text` and a NUL into a `size`-byte buffer and reports the bytes used, as the runtime's
/// string getters do; `E_NOT_SUFFICIENT_BUFFER` (nothing written) if it does not fit.
///
/// # Safety
/// `buffer` is null or holds `size` bytes; `used` is null or has room for a `usize`.
pub unsafe fn put_str(text: &str, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    let need = text.len() + 1;
    if buffer.is_null() || size < need {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    unsafe {
        buffer.copy_from_nonoverlapping(text.as_ptr(), text.len());
        buffer.add(text.len()).write(0);
        if !used.is_null() {
            used.write_unaligned(need);
        }
    }
    S_OK
}

/// The title's NUL-terminated string.
///
/// # Safety
/// `s` is null or NUL-terminated.
pub unsafe fn text(s: *const c_char) -> Option<String> {
    (!s.is_null()).then(|| unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
}

/// A registration token for callbacks nothing ever raises offline. Never 0.
pub fn token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A fixed address standing for a handle that holds nothing.
pub fn handle_of(anchor: &'static u8) -> *const c_void {
    (anchor as *const u8).cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unimplemented_slots_are_discovered() {
        let o = &objects()[0];
        // Slot 0, QueryInterface, is never ours.
        let qi: unsafe extern "system" fn(*const Object) -> HRESULT =
            unsafe { std::mem::transmute(o.slot(0)) };
        assert_eq!(unsafe { qi(o) }, E_NOTIMPL);
        assert!(discovery::contains(&format!("slot not implemented: {} 0x0", o.def.area)));
        let add: unsafe extern "system" fn(*const Object) -> u32 =
            unsafe { std::mem::transmute(o.slot(0x10)) };
        assert_eq!(unsafe { add(o) }, 1);
        // Past the definition's last slot: still a discovery handler, not the end of the table.
        let store = of(&crate::areas::xstore::STORE_IFACE);
        let past: unsafe extern "system" fn(*const Object) -> HRESULT =
            unsafe { std::mem::transmute(store.slot(0x260)) };
        assert_eq!(unsafe { past(store) }, E_NOTIMPL);
        assert!(discovery::contains("slot not implemented: store 0x260"));
    }

    #[test]
    fn strings_fit_or_fail_cleanly() {
        let mut buf = [0xffu8; 8];
        let mut used = 0usize;
        assert_eq!(unsafe { put_str("abc", 8, buf.as_mut_ptr(), &mut used) }, S_OK);
        assert_eq!((&buf[..4], used), (&b"abc\0"[..], 4));
        let mut small = [0xffu8; 3];
        assert_eq!(
            unsafe { put_str("abc", 3, small.as_mut_ptr(), &mut used) },
            E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(small, [0xff; 3], "nothing written");
    }
}
