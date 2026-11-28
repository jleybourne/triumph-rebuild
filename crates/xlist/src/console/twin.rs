//! Depth textures the title also writes as unordered-access views (`docs/design/depth-uav.md`).
//!
//! The console lets one texture be a depth target and a UAV; the host refuses
//! `ALLOW_DEPTH_STENCIL | ALLOW_UNORDERED_ACCESS`. The resource side (`rebuild-d3d12x`) gives such a texture a
//! **twin**: a colour texture of the depth plane's typeless format with unordered access, which the title's UAVs
//! name instead. Here the title's enhanced barriers keep the two in step:
//!
//! ```text
//! console layout     host depth           twin
//! not UAV            the title's layout   UNORDERED_ACCESS, idle
//! UAV (3, 20, 26)    COPY_SOURCE (parked) UNORDERED_ACCESS, written by the title's compute
//!
//! enter (x -> UAV)   depth x -> COPY_SOURCE; copy depth -> twin (unless x is UNDEFINED, the barrier discards,
//!                    or this list left UAV with no work since: the two are still equal)
//! exit  (UAV -> y)   copy twin -> depth (unless the enter was in this list with no work since); depth -> y
//! UAV -> UAV         a barrier on the twin alone (the depth is parked)
//! ```
//!
//! Copies run only at a phase's edges, so a frame that never takes a depth texture through UAV pays nothing. A
//! texture without a twin (a format with none) only has its UAV layouts parked, so every barrier stays legal.

use super::Raw;
use windows::Win32::Graphics::Direct3D12::*;

/// A console texture whose host resource lost unordered access (depth): what [`super::ListHost::uav_twin`]
/// answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UavTwin {
    /// the host depth texture (borrowed)
    pub depth: Raw,
    /// the twin the title's UAVs write (borrowed), or null: none for this format
    pub twin: Raw,
    pub mips: u32,
    pub arrays: u32,
    /// planes of the host depth texture (2 with stencil); the twin has plane 0's
    pub planes: u32,
}

// SAFETY: borrowed COM pointers, used on the recording thread.
unsafe impl Send for UavTwin {}
unsafe impl Sync for UavTwin {}

/// `D3D12_TEXTURE_BARRIER` with a raw resource (64 bytes, the console's and the host's).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureBarrier {
    pub sync_before: i32,
    pub sync_after: i32,
    pub access_before: i32,
    pub access_after: i32,
    pub layout_before: i32,
    pub layout_after: i32,
    pub resource: Raw,
    /// index or first mip, mips, first slice, slices, first plane, planes
    pub range: [u32; 6],
    pub flags: i32,
}

const _: () = assert!(size_of::<TextureBarrier>() == size_of::<D3D12_TEXTURE_BARRIER>());
const _: () = assert!(std::mem::offset_of!(TextureBarrier, resource) == std::mem::offset_of!(D3D12_TEXTURE_BARRIER, pResource));
const _: () = assert!(std::mem::offset_of!(TextureBarrier, flags) == std::mem::offset_of!(D3D12_TEXTURE_BARRIER, Flags));

impl TextureBarrier {
    pub fn from_bytes(b: &[u8; 64]) -> TextureBarrier {
        // SAFETY: plain data of the same size; any bit pattern is a value.
        unsafe { std::ptr::read_unaligned(b.as_ptr().cast()) }
    }
    pub fn to_bytes(&self) -> [u8; 64] {
        // SAFETY: as above (no padding: 6 x 4 + 8 + 24 + 4 + 4 tail).
        let mut out = [0u8; 64];
        unsafe { std::ptr::write_unaligned(out.as_mut_ptr().cast(), *self) };
        out
    }
}

const UNDEFINED: i32 = D3D12_BARRIER_LAYOUT_UNDEFINED.0;
const COPY_SOURCE: i32 = D3D12_BARRIER_LAYOUT_COPY_SOURCE.0;
const COPY_DEST: i32 = D3D12_BARRIER_LAYOUT_COPY_DEST.0;
const UAV: i32 = D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS.0;
const SYNC_NONE: i32 = D3D12_BARRIER_SYNC_NONE.0;
const SYNC_ALL: i32 = D3D12_BARRIER_SYNC_ALL.0;
const SYNC_COPY: i32 = D3D12_BARRIER_SYNC_COPY.0;
const NO_ACCESS: i32 = D3D12_BARRIER_ACCESS_NO_ACCESS.0;
const A_UAV: i32 = D3D12_BARRIER_ACCESS_UNORDERED_ACCESS.0;
const A_COPY_SOURCE: i32 = D3D12_BARRIER_ACCESS_COPY_SOURCE.0;
const A_COPY_DEST: i32 = D3D12_BARRIER_ACCESS_COPY_DEST.0;
const DISCARD: i32 = D3D12_TEXTURE_BARRIER_FLAG_DISCARD.0;

/// A layout with unordered access: the common one and the direct and compute queue ones.
pub fn is_uav_layout(l: i32) -> bool {
    l == UAV || l == D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_UNORDERED_ACCESS.0 || l == D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_UNORDERED_ACCESS.0
}

/// The barrier moves into, out of or within unordered access.
pub fn touches_uav(b: &TextureBarrier) -> bool {
    is_uav_layout(b.layout_before) || is_uav_layout(b.layout_after)
}

/// Where a barrier leaves its texture's UAV phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Enter,
    Exit,
    Within,
}

/// One copy: `(dst, dst subresource, src, src subresource)`; `u32::MAX` subresources = the whole resource.
pub type Copy = (Raw, u32, Raw, u32);

/// What one title barrier becomes: barriers that go in its group, copies after the group, barriers after them.
#[derive(Default)]
pub struct Plan {
    pub pre: Vec<TextureBarrier>,
    pub copies: Vec<Copy>,
    pub post: Vec<TextureBarrier>,
}

impl Plan {
    pub fn clear(&mut self) {
        self.pre.clear();
        self.copies.clear();
        self.post.clear();
    }
}

/// Plane 0's subresources a range names, as `(mip, slice)`; `None` for every one.
fn plane0(r: &[u32; 6], t: &UavTwin) -> Option<Vec<(u32, u32)>> {
    let (m, a) = (t.mips.max(1), t.arrays.max(1));
    if r[1] == 0 {
        if r[0] == u32::MAX {
            return None;
        }
        let i = r[0];
        return Some(if i / (m * a) == 0 { vec![(i % m, i / m)] } else { Vec::new() });
    }
    let planes = r[4]..r[4] + r[5].max(1);
    if !planes.contains(&0) {
        return Some(Vec::new());
    }
    let mut v = Vec::new();
    for s in r[2]..(r[2] + r[3].max(1)).min(a) {
        for mip in r[0]..(r[0] + r[1]).min(m) {
            v.push((mip, s));
        }
    }
    Some(v)
}

/// The twin's range for a depth range (plane 0 only), and the copies between them (`to_twin`: depth into the
/// twin). `None` when the range has no plane-0 subresource.
fn twin_range(r: &[u32; 6], t: &UavTwin, to_twin: bool, copies: &mut Vec<Copy>) -> Option<[u32; 6]> {
    let (src, dst) = if to_twin { (t.depth, t.twin) } else { (t.twin, t.depth) };
    match plane0(r, t) {
        None if t.planes <= 1 => {
            copies.push((dst, u32::MAX, src, u32::MAX));
            Some([u32::MAX, 0, 0, 0, 0, 0])
        }
        None => {
            for s in 0..t.arrays.max(1) {
                for mip in 0..t.mips.max(1) {
                    let i = mip + s * t.mips.max(1);
                    copies.push((dst, i, src, i));
                }
            }
            Some([u32::MAX, 0, 0, 0, 0, 0])
        }
        Some(v) if v.is_empty() => None,
        Some(v) => {
            for &(mip, s) in &v {
                let i = mip + s * t.mips.max(1);
                copies.push((dst, i, src, i));
            }
            Some(if r[1] == 0 { [v[0].0 + v[0].1 * t.mips.max(1), 0, 0, 0, 0, 0] } else { [r[0], r[1], r[2], r[3].max(1), 0, 1] })
        }
    }
}

/// (The debug layer's rules: an access of NO_ACCESS with a layout change needs that side's sync to be NONE, and
/// every sync bit must suit the access, so the parked depth is always "read by a copy" at `SYNC_COPY` and the
/// resting twin "in unordered access" at `SYNC_ALL`.)
/// Plan the host side of title barrier `b` (resource already the host depth texture) on a texture with twin `t`.
/// `quiet`: this list recorded the phase's other edge itself and no work since: at an exit, the twin cannot have
/// been written since the enter; at an enter, depth and twin are still equal since the exit. `None` when the barrier does not touch unordered access (it goes through as it is).
pub fn plan(b: &TextureBarrier, t: &UavTwin, quiet: bool, out: &mut Plan) -> Option<Phase> {
    let (ub, ua) = (is_uav_layout(b.layout_before), is_uav_layout(b.layout_after));
    let d = TextureBarrier { resource: t.depth, ..*b };
    let mut scratch = Vec::new();
    let twin = (!t.twin.is_null()).then(|| twin_range(&b.range, t, true, &mut scratch)).flatten();
    let tb = |range: [u32; 6], sb, sa, ab, aa, lb, la| TextureBarrier {
        sync_before: sb,
        sync_after: sa,
        access_before: ab,
        access_after: aa,
        layout_before: lb,
        layout_after: la,
        resource: t.twin,
        range,
        flags: 0,
    };
    match (ub, ua) {
        (false, false) => None,
        (true, true) => {
            // ordering between UAV writers: the twin's alone (the depth stays parked)
            if let Some(r) = twin {
                let aa = if b.sync_after == SYNC_NONE { NO_ACCESS } else { A_UAV };
                let ab = if b.sync_before == SYNC_NONE { NO_ACCESS } else { A_UAV };
                out.pre.push(tb(r, b.sync_before, b.sync_after, ab, aa, UAV, UAV));
            }
            Some(Phase::Within)
        }
        (false, true) => {
            // park the depth where the copy reads it
            out.pre.push(TextureBarrier { sync_after: SYNC_COPY, access_after: A_COPY_SOURCE, layout_after: COPY_SOURCE, flags: d.flags & !DISCARD, ..d });
            let keep = b.layout_before != UNDEFINED && b.flags & DISCARD == 0 && !quiet;
            if let (Some(r), true) = (twin, keep) {
                twin_range(&b.range, t, true, &mut out.copies);
                out.pre.push(tb(r, SYNC_ALL, SYNC_COPY, A_UAV, A_COPY_DEST, UAV, COPY_DEST));
                let (sa, aa) = if b.sync_after == SYNC_NONE { (SYNC_NONE, NO_ACCESS) } else { (b.sync_after, A_UAV) };
                out.post.push(tb(r, SYNC_COPY, sa, A_COPY_DEST, aa, COPY_DEST, UAV));
            }
            Some(Phase::Enter)
        }
        (true, false) => {
            let keep = b.layout_after != UNDEFINED && b.flags & DISCARD == 0 && !quiet;
            match (twin, keep) {
                (Some(r), true) => {
                    twin_range(&b.range, t, false, &mut out.copies);
                    out.pre.push(tb(r, SYNC_ALL, SYNC_COPY, A_UAV, A_COPY_SOURCE, UAV, COPY_SOURCE));
                    out.pre.push(TextureBarrier { sync_before: SYNC_COPY, sync_after: SYNC_COPY, access_before: A_COPY_SOURCE, access_after: A_COPY_DEST, layout_before: COPY_SOURCE, layout_after: COPY_DEST, flags: 0, ..d });
                    out.post.push(TextureBarrier { sync_before: SYNC_COPY, access_before: A_COPY_DEST, layout_before: COPY_DEST, flags: 0, ..d });
                    // later work waits for the copy's read before writing the twin again
                    out.post.push(tb(r, SYNC_COPY, SYNC_ALL, A_COPY_SOURCE, A_UAV, COPY_SOURCE, UAV));
                }
                // only a copy touched the parked depth (sync bits must suit the access: COPY alone)
                _ => out.pre.push(TextureBarrier { sync_before: SYNC_COPY, access_before: A_COPY_SOURCE, layout_before: COPY_SOURCE, flags: d.flags & !DISCARD, ..d }),
            }
            Some(Phase::Exit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPTH: Raw = 0x1000 as Raw;
    const TWIN: Raw = 0x2000 as Raw;
    const ALL: [u32; 6] = [u32::MAX, 0, 0, 0, 0, 0];

    fn twin(planes: u32) -> UavTwin {
        UavTwin { depth: DEPTH, twin: TWIN, mips: 1, arrays: 3, planes }
    }

    fn b(lb: i32, la: i32, sb: i32, sa: i32, ab: i32, aa: i32) -> TextureBarrier {
        TextureBarrier { sync_before: sb, sync_after: sa, access_before: ab, access_after: aa, layout_before: lb, layout_after: la, resource: DEPTH, range: ALL, flags: 0 }
    }

    const DW: i32 = D3D12_BARRIER_LAYOUT_DEPTH_STENCIL_WRITE.0;
    const SR: i32 = D3D12_BARRIER_LAYOUT_SHADER_RESOURCE.0;
    const S_DS: i32 = D3D12_BARRIER_SYNC_DEPTH_STENCIL.0;
    const S_CS: i32 = D3D12_BARRIER_SYNC_COMPUTE_SHADING.0;
    const A_DSW: i32 = D3D12_BARRIER_ACCESS_DEPTH_STENCIL_WRITE.0;

    #[test]
    fn barriers_off_unordered_access_pass_through() {
        let mut p = Plan::default();
        assert_eq!(plan(&b(DW, SR, S_DS, S_CS, A_DSW, 0x80), &twin(1), false, &mut p), None);
        assert!(p.pre.is_empty() && p.copies.is_empty() && p.post.is_empty());
    }

    #[test]
    fn entering_parks_the_depth_and_copies_it_into_the_twin() {
        let mut p = Plan::default();
        assert_eq!(plan(&b(DW, UAV, S_DS, S_CS, A_DSW, A_UAV), &twin(1), false, &mut p), Some(Phase::Enter));
        assert_eq!(p.pre.len(), 2);
        let d = p.pre[0];
        assert_eq!((d.resource, d.layout_before, d.layout_after, d.sync_before, d.sync_after, d.access_after), (DEPTH, DW, COPY_SOURCE, S_DS, SYNC_COPY, A_COPY_SOURCE));
        let t = p.pre[1];
        assert_eq!((t.resource, t.layout_before, t.layout_after, t.access_after), (TWIN, UAV, COPY_DEST, A_COPY_DEST));
        assert_eq!(p.copies, vec![(TWIN, u32::MAX, DEPTH, u32::MAX)], "one whole-resource copy (single plane, all subresources)");
        assert_eq!(p.post.len(), 1);
        let t = p.post[0];
        assert_eq!((t.resource, t.layout_before, t.layout_after, t.sync_before, t.sync_after, t.access_after), (TWIN, COPY_DEST, UAV, SYNC_COPY, S_CS, A_UAV));
    }

    #[test]
    fn entering_from_undefined_or_with_discard_copies_nothing() {
        for (lb, flags) in [(UNDEFINED, 0), (DW, DISCARD)] {
            let mut p = Plan::default();
            let mut x = b(lb, UAV, SYNC_NONE, S_CS, NO_ACCESS, A_UAV);
            x.flags = flags;
            assert_eq!(plan(&x, &twin(1), false, &mut p), Some(Phase::Enter));
            assert_eq!(p.pre.len(), 1, "the depth parked, nothing on the twin");
            assert_eq!(p.pre[0].flags & DISCARD, 0, "discard never reaches a copy layout");
            assert!(p.copies.is_empty() && p.post.is_empty());
        }
    }

    #[test]
    fn leaving_copies_the_twin_back_then_gives_the_title_its_layout() {
        let mut p = Plan::default();
        assert_eq!(plan(&b(UAV, DW, S_CS, S_DS, A_UAV, A_DSW), &twin(1), false, &mut p), Some(Phase::Exit));
        assert_eq!(p.copies, vec![(DEPTH, u32::MAX, TWIN, u32::MAX)]);
        let (t, d) = (p.pre[0], p.pre[1]);
        assert_eq!((t.resource, t.layout_before, t.layout_after, t.access_before), (TWIN, UAV, COPY_SOURCE, A_UAV));
        assert_eq!((d.resource, d.layout_before, d.layout_after, d.sync_before), (DEPTH, COPY_SOURCE, COPY_DEST, SYNC_COPY));
        let (d, t) = (p.post[0], p.post[1]);
        assert_eq!((d.resource, d.layout_before, d.layout_after, d.sync_after, d.access_after), (DEPTH, COPY_DEST, DW, S_DS, A_DSW));
        assert_eq!((t.resource, t.layout_before, t.layout_after, t.sync_after), (TWIN, COPY_SOURCE, UAV, SYNC_ALL));
    }

    #[test]
    fn entering_again_with_no_work_since_this_lists_exit_copies_nothing() {
        let mut p = Plan::default();
        assert_eq!(plan(&b(SR, UAV, S_CS, S_CS, 0x80, A_UAV), &twin(1), true, &mut p), Some(Phase::Enter));
        assert!(p.copies.is_empty() && p.post.is_empty());
        assert_eq!((p.pre[0].resource, p.pre[0].layout_after), (DEPTH, COPY_SOURCE));
    }

    #[test]
    fn leaving_with_no_work_since_this_lists_enter_copies_nothing() {
        let mut p = Plan::default();
        assert_eq!(plan(&b(UAV, SR, S_CS, S_CS, A_UAV, 0x80), &twin(1), true, &mut p), Some(Phase::Exit));
        assert!(p.copies.is_empty() && p.post.is_empty());
        let d = p.pre[0];
        assert_eq!((d.resource, d.layout_before, d.layout_after, d.access_before, d.sync_before), (DEPTH, COPY_SOURCE, SR, A_COPY_SOURCE, SYNC_COPY));
    }

    #[test]
    fn within_unordered_access_only_the_twin_is_ordered() {
        let mut p = Plan::default();
        assert_eq!(plan(&b(UAV, UAV, S_CS, S_CS, A_UAV, A_UAV), &twin(1), false, &mut p), Some(Phase::Within));
        assert_eq!(p.pre.len(), 1);
        assert_eq!((p.pre[0].resource, p.pre[0].layout_before, p.pre[0].layout_after), (TWIN, UAV, UAV));
    }

    #[test]
    fn without_a_twin_the_uav_layout_is_only_parked() {
        let none = UavTwin { twin: std::ptr::null_mut(), ..twin(2) };
        let mut p = Plan::default();
        plan(&b(DW, UAV, S_DS, S_CS, A_DSW, A_UAV), &none, false, &mut p);
        plan(&b(UAV, UAV, S_CS, S_CS, A_UAV, A_UAV), &none, false, &mut p);
        plan(&b(UAV, DW, S_CS, S_DS, A_UAV, A_DSW), &none, false, &mut p);
        assert!(p.copies.is_empty() && p.post.is_empty());
        let layouts: Vec<_> = p.pre.iter().map(|x| (x.resource, x.layout_before, x.layout_after)).collect();
        assert_eq!(layouts, vec![(DEPTH, DW, COPY_SOURCE), (DEPTH, COPY_SOURCE, DW)]);
    }

    #[test]
    fn stencil_planes_copy_plane_zero_by_subresource() {
        let t = UavTwin { mips: 2, arrays: 2, ..twin(2) };
        let mut p = Plan::default();
        plan(&b(UAV, DW, S_CS, S_DS, A_UAV, A_DSW), &t, false, &mut p);
        assert_eq!(p.copies, vec![(DEPTH, 0, TWIN, 0), (DEPTH, 1, TWIN, 1), (DEPTH, 2, TWIN, 2), (DEPTH, 3, TWIN, 3)]);
        // a stencil-only barrier leaves the twin alone
        let mut s = b(DW, UAV, S_DS, S_CS, A_DSW, A_UAV);
        s.range = [0, 2, 0, 2, 1, 1];
        let mut p = Plan::default();
        plan(&s, &t, false, &mut p);
        assert!(p.copies.is_empty() && p.post.is_empty());
        assert_eq!(p.pre.len(), 1);
        // one subresource by index: slice 1, mip 0, plane 0 = index 2
        let mut one = b(UAV, DW, S_CS, S_DS, A_UAV, A_DSW);
        one.range = [2, 0, 0, 0, 0, 0];
        let mut p = Plan::default();
        plan(&one, &t, false, &mut p);
        assert_eq!(p.copies, vec![(DEPTH, 2, TWIN, 2)]);
        assert_eq!(p.pre[0].range, [2, 0, 0, 0, 0, 0], "the twin's barrier names the same subresource");
        // a range of mips and slices on plane 0
        let mut r = b(UAV, DW, S_CS, S_DS, A_UAV, A_DSW);
        r.range = [1, 1, 0, 2, 0, 2];
        let mut p = Plan::default();
        plan(&r, &t, false, &mut p);
        assert_eq!(p.copies, vec![(DEPTH, 1, TWIN, 1), (DEPTH, 3, TWIN, 3)]);
        assert_eq!(p.pre[0].range, [1, 1, 0, 2, 0, 1]);
    }

    #[test]
    fn barriers_round_trip_as_bytes() {
        let x = b(DW, UAV, S_DS, S_CS, A_DSW, A_UAV);
        assert_eq!(TextureBarrier::from_bytes(&x.to_bytes()), x);
    }
}
