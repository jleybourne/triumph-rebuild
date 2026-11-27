//! Root signatures (device 0x80 `CreateRootSignature(mask, *blob, size, iid, *out)`).
//!
//! The console object is a plain device child (no inline field reads recorded); ours keeps the
//! serialized blob and the host `ID3D12RootSignature` made from it. Console blobs come from the
//! title's own serialize calls (console-only flag bits already stripped by the lead's
//! `D3D12SerializeVersionedRootSignature`) or from compiled shaders.

use std::sync::atomic::Ordering;

use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::ID3D12RootSignature;

use crate::object::{self, Head};
use crate::slots::{Fp, Object};
use crate::{Raw, Resources};

#[repr(C)]
pub struct RootSignature {
    head: Head,
    pub blob: Box<[u8]>,
    /// `None` with no GPU (the stand-in host)
    pub host: Option<ID3D12RootSignature>,
}

pub(crate) unsafe extern "system" fn release(this: Raw) -> u32 {
    unsafe { object::release_boxed::<RootSignature>(this) }
}

pub(crate) fn methods() -> Vec<(usize, &'static str, Fp)> {
    object::common_slots::<3>(release)
}

/// One of ours, or `None`.
pub fn of<'a>(p: Raw) -> Option<&'a RootSignature> {
    // SAFETY: our table means our object.
    object::is(p, Object::RootSignature).then(|| unsafe { &*p.cast::<RootSignature>() })
}

impl Resources {
    /// 0x80 CreateRootSignature.
    ///
    /// # Safety
    /// `blob` holds `size` bytes; `iid` and `out` as the title passes them.
    pub unsafe fn create_root_signature(&self, blob: *const u8, size: usize, iid: *const GUID, out: *mut Raw) -> HRESULT {
        if blob.is_null() || size == 0 {
            return E_INVALIDARG;
        }
        let ctx = &self.inner;
        // SAFETY: the title passes `size` bytes.
        let bytes: Box<[u8]> = unsafe { std::slice::from_raw_parts(blob, size) }.into();
        let host = match ctx.host.root_signature(&bytes) {
            Ok(h) => h,
            Err(e) => {
                ctx.events.note("root signature: the host refused a blob of bytes", bytes.len() as u64);
                return e;
            }
        };
        let r = Box::new(RootSignature { head: Head::new(Object::RootSignature, ctx), blob: bytes, host });
        ctx.counters.root_signatures.fetch_add(1, Ordering::Relaxed);
        object::hand_out(Box::into_raw(r).cast(), Object::RootSignature, iid, out, release)
    }

    /// The host root signature of a console one (borrowed; alive while the console object is).
    pub fn host_root_signature(&self, console: Raw) -> Option<Raw> {
        of(console)?.host.as_ref().map(|h| h.as_raw())
    }
}
