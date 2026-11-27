//! Method tables for the objects this crate hands out: our function in every slot we implement,
//! a trap in every other.
//!
//! A trap counts its calls (one relaxed atomic add), records the time of the first, tells
//! [`crate::Events::first_trap`] once, and fails with the slot's return kind: `E_NOTIMPL` for a
//! method that returns an `HRESULT`, 0 (null, zero) for one that returns a value, a pointer or
//! nothing. A value or pointer slot never returns an `HRESULT` (the fork's list 0x4a0 lesson,
//! `docs/research/d3d12x-device.md` §5.2).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::HRESULT;
use windows::Win32::Foundation::E_NOTIMPL;

use crate::Raw;

/// A function address in a method table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Fp(pub *const ());
// SAFETY: a code address; nothing is shared through it.
unsafe impl Sync for Fp {}
unsafe impl Send for Fp {}

/// Slots per object table. Resource: IUnknown 3, object 4, device child 1, resource 7 (PC), 3
/// console, then slack; heap and descriptor heap: 8 common + their own + slack. A call past
/// what is known lands in a trap, never past the table.
pub const SLOTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Object {
    Resource,
    Heap,
    DescriptorHeap,
}

impl Object {
    pub const ALL: [Object; 3] = [Object::Resource, Object::Heap, Object::DescriptorHeap];

    pub fn name(self) -> &'static str {
        match self {
            Object::Resource => "resource",
            Object::Heap => "heap",
            Object::DescriptorHeap => "descriptor heap",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// What a trapped slot returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Returns {
    /// `E_NOTIMPL`
    Hresult,
    /// 0: a null pointer, a zero value; for a `void` method the register is ignored
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

/// Slots that return an `HRESULT`: IUnknown QI, the object methods (private data, name), the
/// device child's `GetDevice`, and per object: resource Map 0x40, WriteToSubresource 0x60,
/// ReadFromSubresource 0x68, GetHeapProperties 0x70, GetProtectedResourceSession 0x78; heap
/// GetProtectedResourceSession 0x48. Everything else returns a value, a pointer or nothing.
fn returns(object: Object, offset: usize) -> Returns {
    const COMMON: [usize; 6] = [0x00, 0x18, 0x20, 0x28, 0x30, 0x38];
    let own: &[usize] = match object {
        Object::Resource => &[0x40, 0x60, 0x68, 0x70, 0x78],
        Object::Heap => &[0x48],
        Object::DescriptorHeap => &[],
    };
    if COMMON.contains(&offset) || own.contains(&offset) {
        Returns::Hresult
    } else {
        Returns::Value
    }
}

struct Cell {
    count: AtomicU64,
    first_ns: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const CELL: Cell = Cell { count: AtomicU64::new(0), first_ns: AtomicU64::new(0) };
#[allow(clippy::declare_interior_mutable_const)]
const ROW: [Cell; SLOTS] = [CELL; SLOTS];
static TRAPS: [[Cell; SLOTS]; 3] = [ROW; 3];
static EPOCH: OnceLock<Instant> = OnceLock::new();

pub(crate) fn start_clock() {
    EPOCH.get_or_init(Instant::now);
}

#[cold]
fn first(object: Object, slot: usize, this: Raw, cell: &Cell) {
    let ns = EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64;
    cell.first_ns.store(ns.max(1), Ordering::Relaxed);
    if let Some(ctx) = crate::object::context_of(this) {
        ctx.events.first_trap(object.name(), slot * 8);
    }
}

#[inline]
fn hit(object: Object, slot: usize, this: Raw) {
    let cell = &TRAPS[object.index()][slot];
    if cell.count.fetch_add(1, Ordering::Relaxed) == 0 {
        first(object, slot, this, cell);
    }
}

unsafe extern "system" fn trap_hr<const O: usize, const N: usize>(this: Raw) -> HRESULT {
    hit(Object::ALL[O], N, this);
    E_NOTIMPL
}
unsafe extern "system" fn trap_value<const O: usize, const N: usize>(this: Raw) -> u64 {
    hit(Object::ALL[O], N, this);
    0
}

macro_rules! traps {
    ($name:ident, $f:ident, $o:literal) => {
        static $name: [Fp; SLOTS] = [
            Fp($f::<$o, 0> as *const ()), Fp($f::<$o, 1> as *const ()), Fp($f::<$o, 2> as *const ()), Fp($f::<$o, 3> as *const ()),
            Fp($f::<$o, 4> as *const ()), Fp($f::<$o, 5> as *const ()), Fp($f::<$o, 6> as *const ()), Fp($f::<$o, 7> as *const ()),
            Fp($f::<$o, 8> as *const ()), Fp($f::<$o, 9> as *const ()), Fp($f::<$o, 10> as *const ()), Fp($f::<$o, 11> as *const ()),
            Fp($f::<$o, 12> as *const ()), Fp($f::<$o, 13> as *const ()), Fp($f::<$o, 14> as *const ()), Fp($f::<$o, 15> as *const ()),
            Fp($f::<$o, 16> as *const ()), Fp($f::<$o, 17> as *const ()), Fp($f::<$o, 18> as *const ()), Fp($f::<$o, 19> as *const ()),
            Fp($f::<$o, 20> as *const ()), Fp($f::<$o, 21> as *const ()), Fp($f::<$o, 22> as *const ()), Fp($f::<$o, 23> as *const ()),
            Fp($f::<$o, 24> as *const ()), Fp($f::<$o, 25> as *const ()), Fp($f::<$o, 26> as *const ()), Fp($f::<$o, 27> as *const ()),
            Fp($f::<$o, 28> as *const ()), Fp($f::<$o, 29> as *const ()), Fp($f::<$o, 30> as *const ()), Fp($f::<$o, 31> as *const ()),
        ];
    };
}
traps!(RES_HR, trap_hr, 0);
traps!(RES_VALUE, trap_value, 0);
traps!(HEAP_HR, trap_hr, 1);
traps!(HEAP_VALUE, trap_value, 1);
traps!(DH_HR, trap_hr, 2);
traps!(DH_VALUE, trap_value, 2);

/// A built table: the functions, and what each slot is.
pub(crate) struct Table {
    pub funcs: Box<[Fp]>,
    pub info: Box<[SlotInfo]>,
}

pub(crate) fn build(object: Object, implemented: &[(usize, &'static str, Fp)]) -> Table {
    let (hr, value): (&[Fp; SLOTS], &[Fp; SLOTS]) = match object {
        Object::Resource => (&RES_HR, &RES_VALUE),
        Object::Heap => (&HEAP_HR, &HEAP_VALUE),
        Object::DescriptorHeap => (&DH_HR, &DH_VALUE),
    };
    let mut funcs = Vec::with_capacity(SLOTS);
    let mut info = Vec::with_capacity(SLOTS);
    for slot in 0..SLOTS {
        let r = returns(object, slot * 8);
        funcs.push(if r == Returns::Hresult { hr[slot] } else { value[slot] });
        info.push(SlotInfo { offset: slot * 8, kind: SlotKind::Trap(r) });
    }
    for &(offset, name, f) in implemented {
        assert!(offset % 8 == 0 && offset / 8 < SLOTS, "{}: {name} at {offset:#x}", object.name());
        assert!(matches!(info[offset / 8].kind, SlotKind::Trap(_)), "{}: {offset:#x} twice", object.name());
        funcs[offset / 8] = f;
        info[offset / 8].kind = SlotKind::Implemented(name);
    }
    Table { funcs: funcs.into_boxed_slice(), info: info.into_boxed_slice() }
}

/// What each slot of `object`'s table is.
pub fn slots(object: Object) -> &'static [SlotInfo] {
    &crate::object::table(object).info
}

/// Every trapped slot called so far, with its count and first-call time.
pub fn traps() -> Vec<Trap> {
    let mut out = Vec::new();
    for o in Object::ALL {
        for (slot, cell) in TRAPS[o.index()].iter().enumerate() {
            let count = cell.count.load(Ordering::Relaxed);
            if count > 0 {
                out.push(Trap { object: o.name(), offset: slot * 8, count, first_ns: cell.first_ns.load(Ordering::Relaxed) });
            }
        }
    }
    out
}
