//! The method tables: our function in every slot we implement, a trap in every other.
//!
//! A trap counts its calls, records the time of the first, tells [`ListHost::first_trap`] once,
//! and fails with the slot's return kind. The count is one relaxed atomic add, so a trapped slot
//! the title calls every frame costs nothing worth measuring.
//!
//! [`ListHost::first_trap`]: super::ListHost::first_trap

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::HRESULT;
use windows::Win32::Foundation::E_NOTIMPL;

use super::Raw;

/// A function address in a method table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Fp(pub *const ());
// SAFETY: a code address; nothing is shared through it.
unsafe impl Sync for Fp {}
unsafe impl Send for Fp {}

/// Slots in the list's table: 0x000-0x4f8, as the device's (`agents/d3d12x-child-slots.csv`).
pub const LIST_SLOTS: usize = 160;
/// Slots in the allocator's table: IUnknown 3, object 4, device child 1, allocator 2, and slack
/// to 0xb8 so a call past what is known lands in a trap, not past the table.
pub const ALLOCATOR_SLOTS: usize = 24;

/// What a trapped slot returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returns {
    /// `E_NOTIMPL`
    Hresult,
    /// 0: a null pointer, a zero value, `false`; for a `void` method the register is ignored
    Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    Implemented(&'static str),
    Trap(Returns),
}

#[derive(Clone, Copy, Debug)]
pub struct SlotInfo {
    pub offset: usize,
    pub kind: SlotKind,
}

/// One trapped slot that was called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trap {
    pub object: &'static str,
    pub offset: usize,
    pub count: u64,
    /// nanoseconds from the first object's creation to the first call
    pub first_ns: u64,
}

/// Slots that return an `HRESULT` on the list (PC `ID3D12GraphicsCommandList` plus console
/// ResetX); every other list method returns a value or nothing.
const LIST_HRESULT: [usize; 9] = [0x00, 0x18, 0x20, 0x28, 0x30, 0x38, 0x48, 0x50, 0x458];
const ALLOCATOR_HRESULT: [usize; 8] = [0x00, 0x18, 0x20, 0x28, 0x30, 0x38, 0x40, 0x48];

pub(crate) fn returns(object: Object, offset: usize) -> Returns {
    let hr: &[usize] = match object {
        Object::List => &LIST_HRESULT,
        Object::Allocator => &ALLOCATOR_HRESULT,
    };
    if hr.contains(&offset) {
        Returns::Hresult
    } else {
        Returns::Value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Object {
    List,
    Allocator,
}

impl Object {
    pub fn name(self) -> &'static str {
        match self {
            Object::List => "list",
            Object::Allocator => "allocator",
        }
    }
}

struct Cell {
    count: AtomicU64,
    first_ns: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const CELL: Cell = Cell { count: AtomicU64::new(0), first_ns: AtomicU64::new(0) };
static LIST_TRAPS: [Cell; LIST_SLOTS] = [CELL; LIST_SLOTS];
static ALLOCATOR_TRAPS: [Cell; ALLOCATOR_SLOTS] = [CELL; ALLOCATOR_SLOTS];
static EPOCH: OnceLock<Instant> = OnceLock::new();

pub(crate) fn start_clock() {
    EPOCH.get_or_init(Instant::now);
}

#[cold]
fn first(object: Object, slot: usize, this: Raw, cell: &Cell) {
    let ns = EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64;
    cell.first_ns.store(ns.max(1), Ordering::Relaxed);
    let host = match object {
        Object::List => super::list::host_of(this),
        Object::Allocator => super::alloc::host_of(this),
    };
    if let Some(h) = host {
        h.first_trap(object.name(), slot * 8);
    }
}

#[inline]
fn hit(object: Object, slot: usize, this: Raw) {
    let cell = match object {
        Object::List => &LIST_TRAPS[slot],
        Object::Allocator => &ALLOCATOR_TRAPS[slot],
    };
    if cell.count.fetch_add(1, Ordering::Relaxed) == 0 {
        first(object, slot, this, cell);
    }
}

unsafe extern "system" fn list_hr<const N: usize>(this: Raw) -> HRESULT {
    hit(Object::List, N, this);
    E_NOTIMPL
}
unsafe extern "system" fn list_value<const N: usize>(this: Raw) -> u64 {
    hit(Object::List, N, this);
    0
}
unsafe extern "system" fn allocator_hr<const N: usize>(this: Raw) -> HRESULT {
    hit(Object::Allocator, N, this);
    E_NOTIMPL
}
unsafe extern "system" fn allocator_value<const N: usize>(this: Raw) -> u64 {
    hit(Object::Allocator, N, this);
    0
}

macro_rules! traps {
    ($name:ident, $f:ident, $($n:literal)*) => {
        static $name: [Fp; [$($n),*].len()] = [$(Fp($f::<$n> as *const ())),*];
    };
}
traps!(LIST_HR, list_hr,
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39
    40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76
    77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110
    111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127 128 129 130 131 132 133 134 135 136 137 138
    139 140 141 142 143 144 145 146 147 148 149 150 151 152 153 154 155 156 157 158 159);
traps!(LIST_VALUE, list_value,
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39
    40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76
    77 78 79 80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95 96 97 98 99 100 101 102 103 104 105 106 107 108 109 110
    111 112 113 114 115 116 117 118 119 120 121 122 123 124 125 126 127 128 129 130 131 132 133 134 135 136 137 138
    139 140 141 142 143 144 145 146 147 148 149 150 151 152 153 154 155 156 157 158 159);
traps!(ALLOCATOR_HR_TRAPS, allocator_hr, 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23);
traps!(ALLOCATOR_VALUE_TRAPS, allocator_value, 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23);

const _: () = assert!(LIST_HR.len() == LIST_SLOTS && ALLOCATOR_HR_TRAPS.len() == ALLOCATOR_SLOTS);

/// A built table: the functions, and what each slot is.
pub(crate) struct Table {
    pub funcs: Box<[Fp]>,
    pub info: Box<[SlotInfo]>,
}

fn build(object: Object, implemented: &[(usize, &'static str, Fp)]) -> Table {
    let (n, hr, value): (usize, &[Fp], &[Fp]) = match object {
        Object::List => (LIST_SLOTS, &LIST_HR, &LIST_VALUE),
        Object::Allocator => (ALLOCATOR_SLOTS, &ALLOCATOR_HR_TRAPS, &ALLOCATOR_VALUE_TRAPS),
    };
    let mut funcs = Vec::with_capacity(n);
    let mut info = Vec::with_capacity(n);
    for slot in 0..n {
        let r = returns(object, slot * 8);
        funcs.push(if r == Returns::Hresult { hr[slot] } else { value[slot] });
        info.push(SlotInfo { offset: slot * 8, kind: SlotKind::Trap(r) });
    }
    for &(offset, name, f) in implemented {
        assert!(offset % 8 == 0 && offset / 8 < n, "{}: {name} at {offset:#x}", object.name());
        assert!(matches!(info[offset / 8].kind, SlotKind::Trap(_)), "{}: {offset:#x} twice", object.name());
        funcs[offset / 8] = f;
        info[offset / 8].kind = SlotKind::Implemented(name);
    }
    Table { funcs: funcs.into_boxed_slice(), info: info.into_boxed_slice() }
}

pub(crate) fn list_table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| build(Object::List, super::list::IMPLEMENTED))
}

pub(crate) fn allocator_table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| build(Object::Allocator, super::alloc::IMPLEMENTED))
}

/// Every list slot (160) and what it is.
pub fn list_slots() -> &'static [SlotInfo] {
    &list_table().info
}

/// Every allocator slot (24) and what it is.
pub fn allocator_slots() -> &'static [SlotInfo] {
    &allocator_table().info
}

/// Every trapped slot called so far, process-wide, in slot order (lists first).
pub fn traps() -> Vec<Trap> {
    let mut out = Vec::new();
    for (object, cells) in [(Object::List, &LIST_TRAPS[..]), (Object::Allocator, &ALLOCATOR_TRAPS[..])] {
        for (slot, c) in cells.iter().enumerate() {
            let count = c.count.load(Ordering::Relaxed);
            if count > 0 {
                out.push(Trap { object: object.name(), offset: slot * 8, count, first_ns: c.first_ns.load(Ordering::Relaxed) });
            }
        }
    }
    out
}
