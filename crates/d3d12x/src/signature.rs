//! Query heaps (device 0x138 `CreateQueryHeap(*desc, iid, *out)`) and command signatures (device 0x148
//! `CreateCommandSignature(*desc, root signature, iid, *out)`; most of the title's draws are ExecuteIndirect).
//!
//! Plain device children (no inline field reads recorded). Each keeps its description and the host object made
//! from it. A description the host refuses (a console-only query type or argument) still gives the title a valid
//! object, counted and named once through [`crate::Events::note`]: the title's creation path completes and the
//! lists refuse what uses it, instead of the title failing at start.

use std::sync::atomic::Ordering;
use std::sync::Mutex;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::*;

use rebuild_xlist::console::{indirect, HostSignature, IndirectShape, LibrarySignature, UserMap};

use crate::object::{self, Head};
use crate::slots::{Fp, Object};
use crate::{Raw, Resources};

#[repr(C)]
pub struct QueryHeap {
    head: Head,
    pub desc: D3D12_QUERY_HEAP_DESC,
    pub host: Option<ID3D12QueryHeap>,
}

#[repr(C)]
pub struct CommandSignature {
    head: Head,
    pub stride: u32,
    pub arguments: Box<[D3D12_INDIRECT_ARGUMENT_DESC]>,
    pub shape: IndirectShape,
    pub host: Option<ID3D12CommandSignature>,
    /// the host root signature `host` was made against, kept when an argument changes root parameters
    pub host_root: Option<ID3D12RootSignature>,
    /// rewritten into the program library's layout, per (user map, library root): the host signature and the
    /// stages it writes, or `None` when it cannot be (made once, then looked up)
    library: Mutex<Vec<LibraryEntry>>,
}

struct LibraryEntry {
    map: usize,
    root: usize,
    made: Option<(ID3D12CommandSignature, u8)>,
}

unsafe extern "system" fn query_release(this: Raw) -> u32 {
    unsafe { object::release_boxed::<QueryHeap>(this) }
}

unsafe extern "system" fn release(this: Raw) -> u32 {
    unsafe { object::release_boxed::<CommandSignature>(this) }
}

pub(crate) fn query_methods() -> Vec<(usize, &'static str, Fp)> {
    object::common_slots::<4>(query_release)
}

pub(crate) fn methods() -> Vec<(usize, &'static str, Fp)> {
    object::common_slots::<5>(release)
}

pub fn query_heap_of<'a>(p: Raw) -> Option<&'a QueryHeap> {
    // SAFETY: our table means our object.
    object::is(p, Object::QueryHeap).then(|| unsafe { &*p.cast::<QueryHeap>() })
}

pub fn of<'a>(p: Raw) -> Option<&'a CommandSignature> {
    // SAFETY: our table means our object.
    object::is(p, Object::CommandSignature).then(|| unsafe { &*p.cast::<CommandSignature>() })
}

/// Not a draw, draw-indexed, dispatch or mesh argument: one that changes root or input state (or a console one).
fn shape_arg(t: D3D12_INDIRECT_ARGUMENT_TYPE) -> bool {
    !matches!(t, D3D12_INDIRECT_ARGUMENT_TYPE_DRAW | D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED | D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH | D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH)
}

impl Resources {
    /// 0x138 CreateQueryHeap.
    ///
    /// # Safety
    /// Pointers as the title passes them.
    pub unsafe fn create_query_heap(&self, desc: *const D3D12_QUERY_HEAP_DESC, iid: *const GUID, out: *mut Raw) -> HRESULT {
        let Some(&d) = (unsafe { desc.as_ref() }) else { return E_INVALIDARG };
        let ctx = &self.inner;
        let host = ctx.host.query_heap(&d).unwrap_or_else(|_| {
            ctx.events.note("query heap: a type the host refuses (no host heap)", d.Type.0 as u64);
            None
        });
        let q = Box::new(QueryHeap { head: Head::new(Object::QueryHeap, ctx), desc: d, host });
        ctx.counters.query_heaps.fetch_add(1, Ordering::Relaxed);
        object::hand_out(Box::into_raw(q).cast(), Object::QueryHeap, iid, out, query_release)
    }

    /// 0x148 CreateCommandSignature. `root` is the title's root signature (needed by the host when the
    /// arguments change root parameters).
    ///
    /// # Safety
    /// Pointers as the title passes them; `pArgumentDescs` holds `NumArgumentDescs` entries.
    pub unsafe fn create_command_signature(&self, desc: *const D3D12_COMMAND_SIGNATURE_DESC, root: Raw, iid: *const GUID, out: *mut Raw) -> HRESULT {
        let Some(d) = (unsafe { desc.as_ref() }) else { return E_INVALIDARG };
        if d.NumArgumentDescs > 0 && d.pArgumentDescs.is_null() {
            return E_INVALIDARG;
        }
        let ctx = &self.inner;
        // SAFETY: as documented
        let arguments: Box<[D3D12_INDIRECT_ARGUMENT_DESC]> =
            if d.NumArgumentDescs == 0 { Box::new([]) } else { unsafe { std::slice::from_raw_parts(d.pArgumentDescs, d.NumArgumentDescs as usize) }.into() };
        let shape = IndirectShape::of(&arguments);
        // a root is named only when an argument changes root parameters (then it must be the one bound)
        let host_root = if shape.changes_root() { crate::root::of(root).and_then(|r| r.host.clone()) } else { None };
        // every signature the title makes, once each (few): what its ExecuteIndirects can do on the host
        let types = arguments.iter().take(6).enumerate().fold(0u64, |t, (i, a)| t | (u64::from(a.Type.0 as u8) << (8 * i)));
        ctx.events.note("command signature: made (argument types, low byte first)", types);
        ctx.events.note("command signature: made (stride, argument count)", (u64::from(d.ByteStride) << 32) | u64::from(d.NumArgumentDescs));
        for a in arguments.iter().filter(|a| shape_arg(a.Type)) {
            // type, root parameter (or slot), destination offset, count: the union's three words
            // SAFETY: every variant of the union is plain words, three at most
            let w: [u32; 3] = unsafe { std::mem::transmute_copy(&a.Anonymous) };
            ctx.events.note(
                "command signature: root argument (type, parameter, offset, count)",
                (u64::from(a.Type.0 as u8) << 56) | (u64::from(w[0] & 0xff) << 48) | (u64::from(w[1] & 0xffff) << 32) | u64::from(w[2]),
            );
        }
        let host = ctx.host.command_signature(d, host_root.as_ref()).unwrap_or_else(|_| {
            // every argument's type, a byte each (first in the low byte), then the stride
            let types = arguments.iter().take(6).enumerate().fold(0u64, |t, (i, a)| t | (u64::from(a.Type.0 as u8) << (8 * i)));
            ctx.events.note("command signature: the host refuses it (argument types, low byte first)", types);
            ctx.events.note("command signature: its stride and argument count", (u64::from(d.ByteStride) << 32) | u64::from(d.NumArgumentDescs));
            None
        });
        let host_root = host.as_ref().and(host_root);
        let c = Box::new(CommandSignature { head: Head::new(Object::CommandSignature, ctx), stride: d.ByteStride, arguments, shape, host, host_root, library: Mutex::default() });
        ctx.counters.command_signatures.fetch_add(1, Ordering::Relaxed);
        object::hand_out(Box::into_raw(c).cast(), Object::CommandSignature, iid, out, release)
    }

    /// The host command signature of a console one (borrowed), with the root it was made against.
    pub fn host_command_signature(&self, console: Raw) -> Option<HostSignature> {
        let c = of(console)?;
        let signature = c.host.as_ref()?.as_raw();
        Some(HostSignature { signature, root: c.host_root.as_ref().map_or(std::ptr::null_mut(), |r| r.as_raw()), shape: c.shape })
    }

    /// Console signature `console` in the program library's layout of `map`, made against the library root
    /// `root` (borrowed `ID3D12RootSignature`): once per (map, root), then looked up. `None` when its arguments
    /// cannot be mapped ([`indirect::library_arguments`]) or the host refuses the result (named once each).
    pub fn library_signature(&self, console: Raw, root: Raw, map: &'static UserMap) -> Option<LibrarySignature> {
        let c = of(console)?;
        let key = (map as *const UserMap as usize, root as usize);
        let mut lib = c.library.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = lib.iter().find(|e| (e.map, e.root) == key) {
            return e.made.as_ref().map(|(s, stages)| LibrarySignature { signature: s.as_raw(), stages: *stages });
        }
        let ctx = &self.inner;
        let made = match indirect::library_arguments(&c.arguments, map) {
            Err(why) => {
                ctx.events.note("command signature: cannot run under the library root (0 buffer view, 1 read twice, 2 no room, 3 other, 4 order)", why as u64);
                None
            }
            Ok(l) => {
                let d = D3D12_COMMAND_SIGNATURE_DESC { ByteStride: c.stride, NumArgumentDescs: l.arguments.len() as u32, pArgumentDescs: l.arguments.as_ptr(), NodeMask: 0 };
                // SAFETY: a live root signature, borrowed for the call
                let r = unsafe { ID3D12RootSignature::from_raw_borrowed(&root) };
                match ctx.host.command_signature(&d, r) {
                    Ok(Some(s)) => Some((s, l.stages)),
                    Ok(None) => None,
                    Err(hr) => {
                        ctx.events.note("command signature: the host refuses the library layout of one", hr.0 as u32 as u64);
                        None
                    }
                }
            }
        };
        let out = made.as_ref().map(|(s, stages)| LibrarySignature { signature: s.as_raw(), stages: *stages });
        lib.push(LibraryEntry { map: key.0, root: key.1, made });
        out
    }

    /// The host query heap of a console one (borrowed).
    pub fn host_query_heap(&self, console: Raw) -> Option<Raw> {
        query_heap_of(console)?.host.as_ref().map(|h| h.as_raw())
    }
}
