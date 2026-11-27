//! `xg_xs.dll`: the console's texture layout calculator, bound over `rebuild_xg` (lane `xg`, PR #9).
//!
//! `XGCreateTextureComputer` hands the title an object whose first field is a vtable of 13 slots, in the
//! order of `rebuild_xg::computer::SLOTS`. The title calls Release, GetResourceLayout (0x10),
//! GetTexelElementOffsetBytes (0x30), CopyInto/FromSubresource (0x40, 0x48) and GetResourceTiling (0x50);
//! the other slots answer `E_NOTIMPL`. Slot 0x30 returns 0 for a texel outside the texture, never an error
//! code: the fork saw an error read as an offset send the title into wild memory.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::sync::atomic::{AtomicU32, Ordering};

use rebuild_xg::computer::{
    suggest_swizzle_mode, Computer, HResult, LayoutOut, PackedMips, ResourceDesc, SubresourceTiling, E_INVALIDARG,
    E_NOTIMPL, S_OK,
};

const E_POINTER: HResult = 0x8000_4003_u32 as i32;

/// What the title holds: the vtable pointer first, then ours.
#[repr(C)]
pub struct Object {
    vtable: *const Vtable,
    refs: AtomicU32,
    computer: Computer,
}

type Unimplemented = unsafe extern "system" fn(*mut Object) -> HResult;

#[repr(C)]
pub struct Vtable {
    add_ref: unsafe extern "system" fn(*mut Object) -> u32,
    release: unsafe extern "system" fn(*mut Object) -> u32,
    get_resource_layout: unsafe extern "system" fn(*mut Object, *mut LayoutOut) -> HResult,
    get_resource_size_bytes: Unimplemented,
    get_resource_base_alignment_bytes: Unimplemented,
    get_mip_level_offset_bytes: Unimplemented,
    get_texel_element_offset_bytes: unsafe extern "system" fn(*mut Object, u32, u32, u64, u32, u32, u32, *mut u32) -> u64,
    get_texel_coordinate: Unimplemented,
    copy_into_subresource: unsafe extern "system" fn(*mut Object, *mut u8, u32, u32, *const u8, u32, u32) -> HResult,
    copy_from_subresource: unsafe extern "system" fn(*mut Object, *mut u8, u32, u32, u32, *const u8, u32, u32) -> HResult,
    get_resource_tiling: unsafe extern "system" fn(*mut Object, *mut u32, *mut PackedMips, *mut [u32; 3], *mut u32, u32, *mut SubresourceTiling) -> HResult,
    get_texture_view_descriptor: Unimplemented,
    tiled_resource_query: Unimplemented,
}

// The console interface has no QueryInterface: slot 0x00 is AddRef (`SLOTS`). Keep the offsets honest.
const _: () = assert!(std::mem::offset_of!(Vtable, release) == 0x08);
const _: () = assert!(std::mem::offset_of!(Vtable, get_resource_layout) == 0x10);
const _: () = assert!(std::mem::offset_of!(Vtable, get_texel_element_offset_bytes) == 0x30);
const _: () = assert!(std::mem::offset_of!(Vtable, copy_into_subresource) == 0x40);
const _: () = assert!(std::mem::offset_of!(Vtable, get_resource_tiling) == 0x50);
const _: () = assert!(std::mem::size_of::<Vtable>() == 13 * 8);

static VTABLE: Vtable = Vtable {
    add_ref,
    release,
    get_resource_layout,
    get_resource_size_bytes: unimplemented,
    get_resource_base_alignment_bytes: unimplemented,
    get_mip_level_offset_bytes: unimplemented,
    get_texel_element_offset_bytes,
    get_texel_coordinate: unimplemented,
    copy_into_subresource,
    copy_from_subresource,
    get_resource_tiling,
    get_texture_view_descriptor: unimplemented,
    tiled_resource_query: unimplemented,
};

unsafe extern "system" fn add_ref(this: *mut Object) -> u32 {
    (*this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn release(this: *mut Object) -> u32 {
    let left = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        drop(Box::from_raw(this));
    }
    left
}

unsafe extern "system" fn unimplemented(_this: *mut Object) -> HResult {
    E_NOTIMPL
}

unsafe extern "system" fn get_resource_layout(this: *mut Object, out: *mut LayoutOut) -> HResult {
    let Some(out) = out.as_mut() else { return E_POINTER };
    (*this).computer.layout(out);
    S_OK
}

unsafe extern "system" fn get_texel_element_offset_bytes(
    this: *mut Object,
    plane: u32,
    mip: u32,
    x: u64,
    y: u32,
    z: u32,
    _sample: u32,
    bit: *mut u32,
) -> u64 {
    if !bit.is_null() {
        *bit = 0;
    }
    (*this).computer.element_offset(plane, mip, x, y, z)
}

unsafe extern "system" fn copy_into_subresource(
    this: *mut Object,
    tiled: *mut u8,
    plane: u32,
    subresource: u32,
    linear: *const u8,
    row: u32,
    depth: u32,
) -> HResult {
    let c = &(*this).computer;
    let Some((t, l)) = c.copy_extent(plane, subresource, row, depth) else { return E_INVALIDARG };
    if tiled.is_null() || linear.is_null() {
        return E_POINTER;
    }
    c.copy_into(std::slice::from_raw_parts_mut(tiled, t), plane, subresource, std::slice::from_raw_parts(linear, l), row, depth)
}

unsafe extern "system" fn copy_from_subresource(
    this: *mut Object,
    linear: *mut u8,
    plane: u32,
    subresource: u32,
    _sample: u32,
    tiled: *const u8,
    row: u32,
    depth: u32,
) -> HResult {
    let c = &(*this).computer;
    let Some((t, l)) = c.copy_extent(plane, subresource, row, depth) else { return E_INVALIDARG };
    if tiled.is_null() || linear.is_null() {
        return E_POINTER;
    }
    c.copy_from(std::slice::from_raw_parts_mut(linear, l), plane, subresource, std::slice::from_raw_parts(tiled, t), row, depth)
}

unsafe extern "system" fn get_resource_tiling(
    this: *mut Object,
    tiles: *mut u32,
    packed: *mut PackedMips,
    shape: *mut [u32; 3],
    count: *mut u32,
    first: u32,
    out: *mut SubresourceTiling,
) -> HResult {
    let room = if count.is_null() { 0 } else { *count };
    let t = match (*this).computer.tiling(first, room) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Some(p) = tiles.as_mut() {
        *p = t.tiles;
    }
    if let Some(p) = packed.as_mut() {
        *p = t.packed;
    }
    if let Some(p) = shape.as_mut() {
        *p = t.shape;
    }
    if let Some(p) = count.as_mut() {
        *p = t.subresources.len() as u32;
    }
    if !out.is_null() {
        std::ptr::copy_nonoverlapping(t.subresources.as_ptr(), out, t.subresources.len());
    }
    S_OK
}

/// `XGCreateTextureComputer(desc, out)`.
pub unsafe extern "system" fn XGCreateTextureComputer(desc: *const ResourceDesc, out: *mut *mut Object) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    let Some(desc) = desc.as_ref() else { return E_POINTER };
    match Computer::new(desc) {
        Ok(computer) => {
            *out = Box::into_raw(Box::new(Object { vtable: &VTABLE, refs: AtomicU32::new(1), computer }));
            S_OK
        }
        Err(e) => e,
    }
}

/// `XGSuggestSwizzleMode(desc, flags, out)`: the value for the description's `layout` field.
pub unsafe extern "system" fn XGSuggestSwizzleMode(desc: *const ResourceDesc, _flags: u32, out: *mut u32) -> HResult {
    let (Some(desc), Some(out)) = (desc.as_ref(), out.as_mut()) else { return E_POINTER };
    *out = suggest_swizzle_mode(desc);
    S_OK
}

pub fn export(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "XGCreateTextureComputer" => XGCreateTextureComputer as *const (),
        "XGSuggestSwizzleMode" => XGSuggestSwizzleMode as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_xg::computer::{SLOTS, TEXTURE_2D};

    #[test]
    fn the_vtable_follows_the_slot_table_and_the_title_calls_work_through_it() {
        assert_eq!(std::mem::size_of::<Vtable>(), 8 * SLOTS.len());
        for (i, s) in SLOTS.iter().enumerate() {
            assert_eq!(s.offset as usize, 8 * i, "slot {} at {:#x}", s.name, s.offset);
        }
        let desc = ResourceDesc {
            dimension: TEXTURE_2D,
            alignment: 0,
            width: 128,
            height: 4,
            depth_or_array: 1,
            mips: 1,
            format: 28, // R8G8B8A8_UNORM
            samples: 1,
            quality: 0,
            layout: 0,
            flags: 0,
        };
        unsafe {
            let mut mode = 0u32;
            assert_eq!(XGSuggestSwizzleMode(&desc, 0, &mut mode), S_OK);
            assert_ne!(mode, 0);
            let mut obj: *mut Object = std::ptr::null_mut();
            assert_eq!(XGCreateTextureComputer(&desc, &mut obj), S_OK);
            let vt = &*(*obj).vtable;
            let mut layout = LayoutOut::zeroed();
            assert_eq!((vt.get_resource_layout)(obj, &mut *layout), S_OK);
            assert!(layout.size >= 128 * 4 * 4);
            // inside: an offset within the texture; outside: 0, never an error code
            let mut bit = 7u32;
            let inside = (vt.get_texel_element_offset_bytes)(obj, 0, 0, 127, 3, 0, 0, &mut bit);
            assert!(inside < layout.size && bit == 0);
            assert_eq!((vt.get_texel_element_offset_bytes)(obj, 0, 0, 128, 0, 0, 0, &mut bit), 0);
            assert_eq!((vt.get_texel_element_offset_bytes)(obj, 0, 5, 0, 0, 0, 0, &mut bit), 0);
            // a linear round trip through the tiled layout
            let linear: Vec<u8> = (0..128 * 4 * 4).map(|i| (i * 7) as u8).collect();
            let mut tiled = vec![0u8; layout.size as usize];
            assert_eq!((vt.copy_into_subresource)(obj, tiled.as_mut_ptr(), 0, 0, linear.as_ptr(), 128 * 4, 0), S_OK);
            let mut back = vec![0u8; linear.len()];
            assert_eq!((vt.copy_from_subresource)(obj, back.as_mut_ptr(), 0, 0, 0, tiled.as_ptr(), 128 * 4, 0), S_OK);
            assert_eq!(back, linear);
            assert_eq!((vt.get_resource_size_bytes)(obj), E_NOTIMPL);
            assert_eq!((vt.add_ref)(obj), 2);
            assert_eq!((vt.release)(obj), 1);
            assert_eq!((vt.release)(obj), 0);
        }
    }
}
