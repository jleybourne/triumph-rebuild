//! Query heaps (device 0x138 `CreateQueryHeap(*desc, iid, *out)`) and command signatures (device 0x148
//! `CreateCommandSignature(*desc, root signature, iid, *out)`; most of the title's draws are ExecuteIndirect).
//!
//! Plain device children (no inline field reads recorded). Each keeps its description and the host object made
//! from it. A description the host refuses (a console-only query type or argument) still gives the title a valid
//! object, counted and named once through [`crate::Events::note`]: the title's creation path completes and the
//! lists refuse what uses it, instead of the title failing at start.

use std::sync::atomic::Ordering;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::*;

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
    pub host: Option<ID3D12CommandSignature>,
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
        let host_root = crate::root::of(root).and_then(|r| r.host.clone());
        let host = ctx.host.command_signature(d, host_root.as_ref()).unwrap_or_else(|_| {
            ctx.events.note("command signature: the host refuses it (arguments)", arguments.first().map_or(0, |a| a.Type.0 as u64));
            None
        });
        let c = Box::new(CommandSignature { head: Head::new(Object::CommandSignature, ctx), stride: d.ByteStride, arguments, host });
        ctx.counters.command_signatures.fetch_add(1, Ordering::Relaxed);
        object::hand_out(Box::into_raw(c).cast(), Object::CommandSignature, iid, out, release)
    }

    /// The host command signature of a console one (borrowed).
    pub fn host_command_signature(&self, console: Raw) -> Option<Raw> {
        of(console)?.host.as_ref().map(|h| h.as_raw())
    }

    /// The host query heap of a console one (borrowed).
    pub fn host_query_heap(&self, console: Raw) -> Option<Raw> {
        query_heap_of(console)?.host.as_ref().map(|h| h.as_raw())
    }
}
