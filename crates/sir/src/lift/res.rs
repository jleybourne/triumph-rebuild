//! Resources (decision 5: the fork's binding model, which is also plan decision 2).
//!
//! User data arrives as root constants at `b0, space0`: user-data dword k is the value of the user SGPR
//! that holds it. A descriptor table's dword is the **byte offset** of its first descriptor in its heap,
//! in the console's sizes (32 bytes a resource descriptor, 16 a sampler). The code adds byte offsets to it
//! and loads the descriptor with `s_load_dwordx4/x8`. On the host the descriptor is not loaded at all: the
//! loaded SGPRs become a heap index, and the instruction that uses them reads
//! `ResourceDescriptorHeap[index]` or `SamplerDescriptorHeap[index]` (SM 6.6), so the host heap keeps the
//! title's descriptor order. A program that reads descriptor words as data (to edit a field) is not
//! translated yet, and says so.

use std::fmt::Write as _;

use rebuild_shader::gfx10::enc::{self, Decoded, Src};
use rebuild_shader::native::{HwStage, ResourceType, RootParam, RootType, UsedResource};

use super::{Lifter, NullKind};

/// The program's root parameters, as the lifter needs them.
#[derive(Clone, Copy)]
pub struct Bindings<'a> {
    pub hw: HwStage,
    pub layout: &'a [RootParam],
    pub used: &'a [UsedResource],
}

impl Bindings<'_> {
    /// User-data dword held by SGPR `s`, if it is a user SGPR.
    pub fn user_dword(&self, s: u8) -> Option<u8> {
        self.layout.iter().find_map(|p| {
            let first = self.hw.user_sgpr(p.user_data);
            (s >= first && u32::from(s) < u32::from(first) + p.sgprs.max(1)).then(|| p.user_data + (s - first))
        })
    }

    /// The first SGPR after the user SGPRs: in a pixel shader, the hardware's primitive mask (PRIM_MASK).
    pub fn after_user(&self) -> u8 {
        self.layout.iter().map(|p| self.hw.user_sgpr(p.user_data) + p.sgprs.max(1) as u8).max().unwrap_or(0)
    }

    /// SGPR `s` holds a dynamic-resource heap base (0 on the host).
    pub fn is_heap_base(&self, s: u8) -> bool {
        self.layout
            .iter()
            .any(|p| self.hw.user_sgpr(p.user_data) == s && matches!(p.root, RootType::ResourceHeap | RootType::SamplerHeap))
    }

    fn param_at(&self, s: u8) -> Option<&RootParam> {
        self.layout.iter().find(|p| self.hw.user_sgpr(p.user_data) == s)
    }
}

/// Which heap a loaded descriptor indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Heap {
    Resource,
    Sampler,
}

/// A descriptor the program loaded into SGPRs, as the host sees it: a heap index in its first SGPR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desc {
    pub heap: Heap,
    pub dwords: u8,
    /// The table's user-data dword (to find what kind of view it is).
    pub user_data: u8,
}

/// The host view a descriptor is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum View {
    /// `ConstantBuffer<SirCb>`
    Cbv,
    /// `ByteAddressBuffer`
    Raw,
    /// `RWByteAddressBuffer`
    RwRaw,
    /// `Buffer<float4>`
    Typed,
    /// `RWBuffer<float4>`
    RwTyped,
    /// `Texture*<float4>` by the console's `dim` (0 1D, 1 2D, 2 3D, 3 cube, 4 1D array, 5 2D array)
    Tex(u8),
    /// `RWTexture*<float4>` by the console's `dim`
    RwTex(u8),
    /// `SamplerState`
    Sampler,
    /// `SamplerComparisonState`
    SamplerCmp,
}

/// One descriptor read ([`super::Lifted::desc_uses`]): the table it comes from, its byte offset in that table
/// when that is a constant (`None`: computed by the program), and the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescUse {
    pub heap: Heap,
    pub user_data: u8,
    pub byte_off: Option<i32>,
    pub view: View,
    /// When the access also reads the descriptor's shadow-table entry (stride and record count): where in the
    /// record it ends (the instruction's constant offset plus the bytes it moves, at most 255; else 0).
    pub shadow: u8,
}

impl Lifter<'_> {
    /// Records that the descriptor whose heap index SGPR `s` holds is read as `view`.
    fn use_desc(&mut self, s: u8, view: View, shadow: u8) {
        if let Some(d) = self.f.descs.get(&s) {
            let u = DescUse { heap: d.heap, user_data: d.user_data, byte_off: self.f.desc_off.get(&s).copied(), view, shadow };
            self.out.desc_uses.push(u);
        }
    }

    /// The kind of view the descriptor whose heap index SGPR `s` holds is, in the title's root signature: the
    /// range of its table that starts last at or before its offset (known only for a constant offset).
    fn desc_kind(&self, s: u8) -> Option<ResourceType> {
        let (d, off, bind) = (self.f.descs.get(&s)?, *self.f.desc_off.get(&s)?, self.bind?);
        let dword = u32::try_from(off).ok()? / 4;
        bind.used
            .iter()
            .filter(|u| u.root == RootType::Table && u.user_data == d.user_data && u.table_offset_dwords <= dword)
            .max_by_key(|u| u.table_offset_dwords)
            .map(|u| u.kind)
    }

    /// The descriptor is a UAV: a load through it reads the host's UAV, so it is declared as one (a UAV
    /// descriptor read as an SRV type is undefined).
    fn desc_is_uav(&self, s: u8) -> bool {
        self.desc_kind(s) == Some(ResourceType::Uav)
    }

    /// `s_load_dwordx4/x8` of a descriptor from a table: the destination's first SGPR becomes the heap index.
    pub(super) fn smem(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let bind = self.bind?;
        let base = (d.get("sbase")? * 2) as u8;
        let dst = d.get("sdata")? as u8;
        let off = sext21(d.get("offset")?);
        // a register offset (a per-draw index, say) is added as the value the translated code holds
        let soff = d.get("soffset")?;
        let reg = match enc::src(soff) {
            Src::Null => None,
            _ => match self.src(soff, None) {
                Some(e) => Some(e),
                None => {
                    self.why = Some(format!("register offset {:?} is not data", enc::src(soff)));
                    return None;
                }
            },
        };
        let byte_off = match &reg {
            Some(r) => format!("((int){r} + {off})"),
            None => format!("{off}"),
        };
        if name.starts_with("s_buffer_load_dword") {
            return self.buffer_load(name, base, dst, off, reg);
        }
        // data embedded in the program's code, at a byte offset from `s_getpc_b64`
        if self.f.code_of.contains(&base) {
            let n: u8 = match name {
                "s_load_dword" => 1,
                "s_load_dwordx2" => 2,
                "s_load_dwordx4" => 4,
                "s_load_dwordx8" => 8,
                "s_load_dwordx16" => 16,
                _ => return None,
            };
            if self.code.is_empty() {
                return None;
            }
            self.out.code = self.code.clone();
            let last = self.code.len() - 1;
            let at = format!("(uint)((int)s{base} + {byte_off}) >> 2");
            for k in 0..n {
                self.out.sgprs.insert(dst + k);
                self.wrote(dst + k);
                let _ = writeln!(self.out.body, "    s{} = sir_code[min(({at}) + {k}u, {last}u)];", dst + k);
            }
            return Some(());
        }
        let dwords: u8 = match name {
            "s_load_dwordx4" => 4,
            "s_load_dwordx8" => 8,
            _ => return None,
        };
        // the base is a table's offset, or computed from one (the translated SALU computes its value)
        let p = match self.table_origin(base) {
            Some(ud) => bind.layout.iter().find(|p| p.user_data == ud)?,
            None => match bind.param_at(base) {
                Some(p) if !self.f.may_written.contains(&base) => p,
                Some(_) => {
                    self.why = Some("base computed from no table".into());
                    return None;
                }
                None => {
                    self.why = Some("base is no root parameter".into());
                    return None;
                }
            },
        };
        let heap = match p.root {
            RootType::Table => {
                let sampler = bind.used.iter().any(|u| u.user_data == p.user_data && u.kind == ResourceType::Sampler);
                if sampler { Heap::Sampler } else { Heap::Resource }
            }
            // dynamic resources: the heap base is 0 on the host; the program adds index × size itself
            RootType::ResourceHeap => Heap::Resource,
            RootType::SamplerHeap => Heap::Sampler,
            r => {
                self.why = Some(format!("base is a {r:?} parameter"));
                return None;
            }
        };
        let stride = if heap == Heap::Sampler { 16 } else { 32 };
        // the offset in the table is a constant when the base is the table's own user SGPR
        let const_off = if reg.is_none() && matches!(p.root, RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap) { self.table_offset(base).map(|k| k.wrapping_add(off)) } else { None };
        self.out.sgprs.insert(base);
        if self.f.stale.contains(&base) {
            self.why = Some("a lane mask read as data".into());
            return None;
        }
        if !self.f.written.contains(&base) {
            self.out.live_in.insert(base);
        }
        self.out.sgprs.insert(dst);
        self.wrote(dst);
        let _ = writeln!(self.out.body, "    s{dst} = (uint)((int)s{base} + {byte_off}) / {stride}u;");
        self.f.descs.insert(dst, Desc { heap, dwords, user_data: p.user_data });
        if let Some(o) = const_off {
            self.f.desc_off.insert(dst, o);
        }
        for k in 1..dwords {
            self.wrote(dst + k);
            self.f.desc_words.insert(dst + k);
        }
        Some(())
    }

    /// `s_buffer_load_dword*` through a buffer descriptor from the heap: always a raw read (`ByteAddressBuffer`
    /// at the console index, or the `RWByteAddressBuffer` alternate when the title's range says UAV), never a
    /// CBV. Console descriptors are untyped: the title may read as constants a slot it filled with a buffer SRV
    /// (or the other way round), and the host needs one type per slot, so every buffer slot of a shader-visible
    /// heap holds a raw SRV (`crates/d3d12x`, CBVs included). The index stays an SGPR value (no
    /// `NonUniformResourceIndex`), so the loads stay scalar.
    fn buffer_load(&mut self, name: &str, base: u8, dst: u8, off: i32, reg: Option<String>) -> Option<()> {
        let n: u8 = match name {
            "s_buffer_load_dword" => 1,
            "s_buffer_load_dwordx2" => 2,
            "s_buffer_load_dwordx4" => 4,
            "s_buffer_load_dwordx8" => 8,
            "s_buffer_load_dwordx16" => 16,
            _ => return None,
        };
        let Some(desc) = self.f.descs.get(&base).copied() else {
            return self.window_load(n, base, dst, off, reg);
        };
        if desc.heap != Heap::Resource || off < 0 {
            return None;
        }
        // never a CBV: the slot holds a raw SRV on the host whatever the title's root signature says
        let uav = self.desc_is_uav(base);
        self.use_desc(base, if uav { View::RwRaw } else { View::Raw }, 0);
        let dw = off as u32 / 4;
        let _ = writeln!(self.out.body, "    {{");
        // with a register offset the dword index is dynamic
        if let Some(r) = &reg {
            let _ = writeln!(self.out.body, "        uint b = ({r} + {}u) >> 2;", off);
        }
        self.out.uses_shadow = true;
        // a UAV is read through its own view (the alternate: a raw UAV over the same 16-byte aligned start)
        let (rw, at) = if uav { ("RW", super::alt_index(&alternate(base), NullKind::RwRaw)) } else { ("", vi(base, NullKind::Raw)) };
        let _ = writeln!(self.out.body, "        {rw}ByteAddressBuffer c = ResourceDescriptorHeap[{at}]; uint sir_ld = {};", lead(base));
        for k in 0..n {
            let s = dst + k;
            self.out.sgprs.insert(s);
            self.wrote(s);
            let i = dw + u32::from(k);
            let e = match &reg {
                None => format!("c.Load(sir_ld + {}u)", i * 4),
                Some(_) => format!("c.Load(sir_ld + ((b + {k}u) << 2))"),
            };
            let _ = writeln!(self.out.body, "        s{s} = {e};");
        }
        let _ = writeln!(self.out.body, "    }}");
        Some(())
    }

    /// `s_buffer_load_dword*` through a descriptor the code built in SGPRs from a 64-bit address (a root
    /// descriptor's user data, say): the base address is the V#'s first 48 bits, so the load is window
    /// addressing (`docs/design/shader-ir.md`, "Memory"): `ResourceDescriptorHeap[WINDOW_HEAP_BASE + (address >>
    /// 32)]` at the 32-bit offset. The window index is an SGPR, so it is wave-uniform. The descriptor's record
    /// count is not applied: a read past the buffer reads what the window holds there (0 when unmapped).
    fn window_load(&mut self, n: u8, base: u8, dst: u8, off: i32, reg: Option<String>) -> Option<()> {
        let (Some(lo), Some(hi)) = (self.src(u32::from(base), None), self.src(u32::from(base) + 1, None)) else {
            self.why = Some("descriptor not traced".into());
            return None;
        };
        self.out.notes.insert("s_buffer_load through a code-built descriptor: window addressing, no record clamp");
        let at = match &reg {
            Some(r) => format!("{lo} + {r} + {off}u"),
            None => format!("{lo} + {off}u"),
        };
        let _ = writeln!(
            self.out.body,
            "    {{ RWByteAddressBuffer w = ResourceDescriptorHeap[{}u + ({hi} & {}u)]; uint a = ({at}) & ~3u;",
            super::WINDOW_HEAP_BASE,
            super::WINDOW_COUNT - 1
        );
        for k in 0..n {
            let s = dst + k;
            self.out.sgprs.insert(s);
            self.wrote(s);
            let _ = writeln!(self.out.body, "      s{s} = w.Load(a + {}u);", 4 * u32::from(k));
        }
        self.out.body.push_str("    }\n");
        Some(())
    }

    /// Untyped buffer loads (`buffer_load_dword`, `x2`, `x3`, `x4`) through a heap descriptor, as
    /// `ByteAddressBuffer` loads at `vaddr (offen) + offset + soffset`. Out of range reads 0 on both. The
    /// indexed form (`idxen`: index times the descriptor's stride) is refused until the stride is known.
    pub(super) fn mubuf(&mut self, name: &str, d: &Decoded) -> Option<()> {
        if name.contains("_format_") {
            return self.mubuf_typed(name, d);
        }
        if name.starts_with("buffer_atomic_") {
            return self.buffer_atomic(name, d);
        }
        let store = name.starts_with("buffer_store_dword");
        let n: u16 = match name.trim_start_matches("buffer_load_").trim_start_matches("buffer_store_") {
            "dword" => 1,
            "dwordx2" => 2,
            "dwordx3" => 3,
            "dwordx4" => 4,
            _ => return None,
        };
        if !store && !name.starts_with("buffer_load_dword") {
            return None;
        }
        if d.get("lds")? == 1 || d.get("tfe")? == 1 {
            self.why = Some("lds or tfe".into());
            return None;
        }
        let rsrc = (d.get("srsrc")? * 4) as u8;
        let Some(res) = self.f.descs.get(&rsrc).copied() else {
            return self.window_mubuf(d, rsrc, n, store);
        };
        if res.heap != Heap::Resource {
            self.why = Some("not a resource descriptor".into());
            return None;
        }
        let soff = self.usrc(d, "soffset")?;
        let (idxen, offen) = (d.get("idxen")? == 1, d.get("offen")? == 1);
        let uav = store || self.desc_is_uav(rsrc);
        // never a CBV (as `buffer_load`): a load through a descriptor the title's root signature calls a CBV reads
        // the raw SRV at its index
        let view = if uav { View::RwRaw } else { View::Raw };
        self.use_desc(rsrc, view, if idxen { (d.get("offset")? + u32::from(n) * 4).min(255) as u8 } else { 0 });
        let vaddr = d.get("vaddr")? as u16;
        let mut addr = format!("{}u + {soff}", d.get("offset")?);
        // with idxen, vaddr is the index and vaddr + 1 the offset (when offen)
        if offen {
            let v = vaddr + u16::from(idxen);
            self.out.vgprs.insert(v);
            addr = format!("v{v} + {addr}");
        }
        let mut guard = String::new();
        // the stride, record count, alternate view and lead are the shadow entry's
        self.out.uses_shadow = true;
        if idxen {
            self.out.vgprs.insert(vaddr);
            addr = format!("v{vaddr} * sh.x + {addr}");
            guard = format!(" if (v{vaddr} >= sh.y) r = 0;");
        }
        // the raw views start at the 16-byte aligned address below the buffer's
        addr = format!("sh.w + {addr}");
        let vdata = d.get("vdata")? as u16;
        let load = ["Load", "Load2", "Load3", "Load4"][n as usize - 1];
        let ty = if n == 1 { String::new() } else { n.to_string() };
        let sh = format!(" uint4 sh = sir_shadow.Load4(sir_sh(s{rsrc}));");
        if store {
            // out of range writes are dropped on both
            let vals: Vec<String> = (0..n).map(|k| {
                self.out.vgprs.insert(vdata + k);
                format!("v{}", vdata + k)
            }).collect();
            let value = if n == 1 { vals[0].clone() } else { format!("uint{n}({})", vals.join(", ")) };
            let mut cond = if self.f.narrowed { "ex".to_string() } else { "true".to_string() };
            if idxen {
                cond = format!("{cond} && v{vaddr} < sh.y");
            }
            let st = ["Store", "Store2", "Store3", "Store4"][n as usize - 1];
            // the UAV is the alternate view
            let _ = writeln!(self.out.body, "    {{{sh} RWByteAddressBuffer b = ResourceDescriptorHeap[sir_va(sh.z, 1u)]; if ({cond}) b.{st}({addr}, {value}); }}");
            return Some(());
        }
        // a UAV is read through its own view, the alternate
        if uav {
            let _ = writeln!(self.out.body, "    {{{sh} RWByteAddressBuffer b = ResourceDescriptorHeap[sir_va(sh.z, 1u)]; uint{ty} r = b.{load}({addr});{guard}");
        } else {
            let _ = writeln!(self.out.body, "    {{ ByteAddressBuffer b = ResourceDescriptorHeap[sir_vi(s{rsrc}, 0u)];{sh} uint{ty} r = b.{load}({addr});{guard}");
        }
        for k in 0..n {
            let e = if n == 1 { "r".to_string() } else { format!("r.{}", ["x", "y", "z", "w"][k as usize]) };
            self.vwrite_raw(vdata + k, e);
        }
        self.out.body.push_str("    }\n");
        Some(())
    }

    /// Untyped `buffer_load_dword*` / `buffer_store_dword*` through a V# the code holds as data (built from an
    /// address, or loaded from memory): window addressing at `base + soffset + index * stride + offset`, with
    /// the V#'s stride (word 1, bits 16-29) and record count (word 2). Range checks follow GFX10's
    /// `OOB_SELECT` (word 3, bits 28-29): 0 index and offset (structured), 1 index only, 2 the byte offset
    /// (raw), 3 none; a lane out of range reads 0 and writes nothing. Folded when word 3 is a known constant.
    /// `add_tid_enable` and swizzled buffers are refused when word 3 is known to set them.
    fn window_mubuf(&mut self, d: &Decoded, rsrc: u8, n: u16, store: bool) -> Option<()> {
        let words: Option<Vec<String>> = (0..4u32).map(|k| self.src(u32::from(rsrc) + k, None)).collect();
        let Some(w) = words else {
            self.why = Some("descriptor not traced".into());
            return None;
        };
        let w3 = self.f.consts.get(&(rsrc + 3)).copied();
        if w3.is_some_and(|x| x >> 23 & 1 == 1) {
            self.why = Some("add_tid_enable".into());
            return None;
        }
        let soff = self.usrc(d, "soffset")?;
        let (idxen, offen) = (d.get("idxen")? == 1, d.get("offen")? == 1);
        let vaddr = d.get("vaddr")? as u16;
        let idx = if idxen {
            self.out.vgprs.insert(vaddr);
            format!("v{vaddr}")
        } else {
            "0u".into()
        };
        let voff = if offen {
            let v = vaddr + u16::from(idxen);
            self.out.vgprs.insert(v);
            format!("v{v} + {}u", d.get("offset")?)
        } else {
            format!("{}u", d.get("offset")?)
        };
        self.out.notes.insert("buffer access through a V# held as data: window addressing");
        let mut b = String::new();
        let _ = write!(
            b,
            "    {{ RWByteAddressBuffer w = ResourceDescriptorHeap[{}u + ({} & {}u)]; uint sir_st = ({} >> 16) & 0x3fffu, sir_nr = {}, sir_i = {idx}, sir_o = {voff}; uint a = ({} + {soff} + sir_i * sir_st + sir_o) & ~3u;",
            super::WINDOW_HEAP_BASE,
            w[1],
            super::WINDOW_COUNT - 1,
            w[1],
            w[2],
            w[0]
        );
        // whether dword k of the access is in range
        let inside = |k: u16| -> String {
            let raw = format!("sir_o + {}u < sir_nr", 4 * k);
            let structured = format!("(sir_i < sir_nr && sir_o + {}u < sir_st)", 4 * k);
            match w3.map(|x| x >> 28 & 3) {
                Some(0) => structured,
                Some(1) => "sir_i < sir_nr".into(),
                Some(2) => raw,
                Some(_) => "true".into(),
                None => format!("(sir_ob == 3u || (sir_ob == 2u ? {raw} : (sir_ob == 1u ? sir_i < sir_nr : {structured})))"),
            }
        };
        if w3.is_none() {
            let _ = write!(b, " uint sir_ob = ({} >> 28) & 3u;", w[3]);
        }
        self.out.body.push_str(&b);
        self.out.body.push('\n');
        let vdata = d.get("vdata")? as u16;
        let guard = if self.f.narrowed { "ex && " } else { "" };
        for k in 0..n {
            self.out.vgprs.insert(vdata + k);
            let i = inside(k);
            if store {
                let _ = writeln!(self.out.body, "      if ({guard}{i}) w.Store(a + {}u, v{});", 4 * k, vdata + k);
            } else {
                self.vwrite_raw(vdata + k, format!("{i} ? w.Load(a + {}u) : 0u", 4 * k));
            }
        }
        self.out.body.push_str("    }\n");
        Some(())
    }

    /// `buffer_atomic_*` (32-bit) as `Interlocked*` on the buffer's raw UAV: a heap descriptor's own view, or the
    /// window of a V# held as data. The address and range check are the loads' (an access out of range does
    /// nothing and returns 0). `glc` returns the old value; `cmpswap` takes (new, compare) from `vdata`.
    fn buffer_atomic(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let op = name.strip_prefix("buffer_atomic_")?;
        let f = match op {
            "add" | "sub" => "InterlockedAdd",
            "smin" | "umin" => "InterlockedMin",
            "smax" | "umax" => "InterlockedMax",
            "and" => "InterlockedAnd",
            "or" => "InterlockedOr",
            "xor" => "InterlockedXor",
            "swap" => "InterlockedExchange",
            "cmpswap" => "InterlockedCompareExchange",
            _ => {
                self.why = Some(format!("buffer atomic {op}"));
                return None;
            }
        };
        if d.get("lds")? == 1 || d.get("tfe")? == 1 {
            self.why = Some("lds or tfe".into());
            return None;
        }
        let rsrc = (d.get("srsrc")? * 4) as u8;
        let soff = self.usrc(d, "soffset")?;
        let (idxen, offen) = (d.get("idxen")? == 1, d.get("offen")? == 1);
        let vaddr = d.get("vaddr")? as u16;
        let idx = if idxen {
            self.out.vgprs.insert(vaddr);
            format!("v{vaddr}")
        } else {
            "0u".into()
        };
        let voff = if offen {
            let v = vaddr + u16::from(idxen);
            self.out.vgprs.insert(v);
            format!("v{v} + {}u", d.get("offset")?)
        } else {
            format!("{}u", d.get("offset")?)
        };
        let (head, inside) = match self.f.descs.get(&rsrc).copied() {
            Some(res) if res.heap == Heap::Resource => {
                self.out.uses_shadow = true;
                let check = if idxen { "sir_i < sh.y" } else { "true" };
                let st = if idxen { "sh.x" } else { "0u" };
                // the UAV is the alternate view, from the 16-byte aligned address `lead` bytes below
                (
                    format!("uint4 sh = sir_shadow.Load4(sir_sh(s{rsrc})); RWByteAddressBuffer w = ResourceDescriptorHeap[sir_va(sh.z, 1u)]; uint sir_i = {idx}, sir_o = {voff}; uint a = sh.w + sir_i * {st} + sir_o + {soff};"),
                    check.to_string(),
                )
            }
            Some(_) => {
                self.why = Some("not a resource descriptor".into());
                return None;
            }
            None => {
                let words: Option<Vec<String>> = (0..4u32).map(|k| self.src(u32::from(rsrc) + k, None)).collect();
                let Some(w) = words else {
                    self.why = Some("descriptor not traced".into());
                    return None;
                };
                self.out.notes.insert("buffer access through a V# held as data: window addressing");
                let w3 = self.f.consts.get(&(rsrc + 3)).copied();
                let raw = "sir_o < sir_nr".to_string();
                let structured = "(sir_i < sir_nr && sir_o < sir_st)".to_string();
                let inside = match w3.map(|x| x >> 28 & 3) {
                    Some(0) => structured,
                    Some(1) => "sir_i < sir_nr".into(),
                    Some(2) => raw,
                    Some(_) => "true".into(),
                    None => format!("(sir_ob == 3u || (sir_ob == 2u ? {raw} : (sir_ob == 1u ? sir_i < sir_nr : {structured})))"),
                };
                let ob = if w3.is_none() { format!(" uint sir_ob = ({} >> 28) & 3u;", w[3]) } else { String::new() };
                (
                    format!(
                        "RWByteAddressBuffer w = ResourceDescriptorHeap[{}u + ({} & {}u)]; uint sir_st = ({} >> 16) & 0x3fffu, sir_nr = {}, sir_i = {idx}, sir_o = {voff}; uint a = {} + {soff} + sir_i * sir_st + sir_o;{ob}",
                        super::WINDOW_HEAP_BASE,
                        w[1],
                        super::WINDOW_COUNT - 1,
                        w[1],
                        w[2],
                        w[0]
                    ),
                    inside,
                )
            }
        };
        let vdata = d.get("vdata")? as u16;
        self.out.vgprs.insert(vdata);
        let signed = matches!(op, "smin" | "smax");
        let v = |k: u16| if signed { format!("(int)v{}", vdata + k) } else { format!("v{}", vdata + k) };
        let args = match op {
            "sub" => format!("0u - v{vdata}"),
            "cmpswap" => {
                self.out.vgprs.insert(vdata + 1);
                format!("{}, {}", v(1), v(0))
            }
            _ => v(0),
        };
        let ty = if signed { "int" } else { "uint" };
        let guard = if self.f.narrowed { "ex && " } else { "" };
        let _ = writeln!(self.out.body, "    {{ {head} {ty} o = 0; if ({guard}{inside}) w.{f}(a & ~3u, {args}, o);");
        if d.get("glc")? == 1 {
            self.vwrite_raw(vdata, "(uint)o".into());
        }
        self.out.body.push_str("    }\n");
        Some(())
    }

    /// Typed buffer loads and stores (`buffer_load_format_x`-`xyzw`, `buffer_store_format_*`): the element format
    /// is the console descriptor's, and the runtime builds the host's typed view from that descriptor, so the
    /// shader indexes `Buffer<float4>` / `RWBuffer<float4>` by element and the view converts. The indexed form
    /// with no byte offset, and element 0 (no index, no offset); others are refused with the reason.
    pub(super) fn mubuf_typed(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let store = name.starts_with("buffer_store_");
        let n = match name.rsplit('_').next()? {
            "x" => 1u16,
            "xy" => 2,
            "xyz" => 3,
            "xyzw" => 4,
            _ => return None,
        };
        let soff_zero = matches!(enc::src(d.get("soffset")?), Src::Inline(128) | Src::Null);
        // the element is the index (idxen), or element 0 with no index and no offset at all
        let idxen = d.get("idxen")? == 1;
        if d.get("offen")? == 1 || d.get("offset")? != 0 || !soff_zero {
            self.why = Some("typed access by byte offset".into());
            return None;
        }
        if d.get("lds")? == 1 || d.get("tfe")? == 1 {
            self.why = Some("lds or tfe".into());
            return None;
        }
        let rsrc = (d.get("srsrc")? * 4) as u8;
        let Some(res) = self.f.descs.get(&rsrc).copied() else {
            self.why = Some("descriptor not traced".into());
            return None;
        };
        if res.heap != Heap::Resource {
            self.why = Some("not a resource descriptor".into());
            return None;
        }
        let (vaddr, vdata) = (d.get("vaddr")? as u16, d.get("vdata")? as u16);
        self.out.uses_shadow = true;
        let el = if idxen {
            self.out.vgprs.insert(vaddr);
            format!("v{vaddr}")
        } else {
            "0u".to_string()
        };
        let uav = store || self.desc_is_uav(rsrc);
        self.use_desc(rsrc, if uav { View::RwTyped } else { View::Typed }, 0);
        let ch = ["x", "y", "z", "w"];
        // d16: the data are halves, two to a register (component k in register k / 2, half k % 2); the host view
        // converts the format to and from float, and the halves are that float's f16 bits
        let d16 = name.contains("_d16_");
        if store {
            let vals: Vec<String> = (0..4u16)
                .map(|k| {
                    if k >= n {
                        "0.0f".into()
                    } else if d16 {
                        self.out.vgprs.insert(vdata + k / 2);
                        format!("f16tof32(v{} >> {}u)", vdata + k / 2, 16 * (k % 2))
                    } else {
                        self.out.vgprs.insert(vdata + k);
                        format!("asfloat(v{})", vdata + k)
                    }
                })
                .collect();
            let guard = if self.f.narrowed { "if (ex) " } else { "" };
            // only the channels the instruction names change: read, merge, write
            let merge: Vec<String> = (0..n).map(|k| format!("o.{} = {};", ch[k as usize], vals[k as usize])).collect();
            let _ = writeln!(
                self.out.body,
                "    {{ RWBuffer<float4> b = ResourceDescriptorHeap[{}]; {guard}{{ float4 o = b[{el}]; {} b[{el}] = o; }} }}",
                super::alt_index(&alternate(rsrc), NullKind::RwTyped),
                merge.join(" ")
            );
            return Some(());
        }
        // the typed view (RW for a UAV) is the alternate
        let rw = if uav { "RW" } else { "" };
        let at = super::alt_index(&alternate(rsrc), if uav { NullKind::RwTyped } else { NullKind::Typed });
        let _ = writeln!(self.out.body, "    {{ {rw}Buffer<float4> b = ResourceDescriptorHeap[{at}]; float4 r = b[{el}];");
        if d16 {
            // a register with one half loaded keeps its other half (as the console's d16 loads do)
            for r in 0..n.div_ceil(2) {
                let lo = format!("f32tof16(r.{})", ch[2 * r as usize]);
                let e = if 2 * r + 1 < n {
                    format!("{lo} | (f32tof16(r.{}) << 16)", ch[2 * r as usize + 1])
                } else {
                    self.out.vgprs.insert(vdata + r);
                    format!("(v{} & 0xffff0000u) | {lo}", vdata + r)
                };
                self.vwrite_raw(vdata + r, e);
            }
        } else {
            for k in 0..n {
                self.vwrite_raw(vdata + k, format!("asuint(r.{})", ch[k as usize]));
            }
        }
        self.out.body.push_str("    }\n");
        Some(())
    }

    /// Image sampling, loads and stores (2D, 2D array, 3D, 1D) through heap descriptors.
    pub(super) fn mimg(&mut self, name: &str, d: &Decoded) -> Option<()> {
        // unorm only changes how a sampler filters: loads, stores and atomics address texels by integer anyway
        let sampled = name.starts_with("image_sample") || name.starts_with("image_gather");
        // a16: float addresses packed two fp16 per register (sampling with no bias only, so far)
        let a16 = d.get("a16")? == 1;
        let a16_ok = matches!(name, "image_sample" | "image_sample_lz" | "image_sample_l" | "image_load" | "image_store" | "image_gather4_lz");
        // d16: sampled results as f16, two channels a register (low half first, an odd last high half 0)
        let d16 = d.get("d16")? == 1 && sampled;
        for (flag, why) in [("a16", "16-bit address"), ("d16", "16-bit data"), ("tfe", "tfe"), ("lwe", "lwe"), ("unorm", "unnormalised")] {
            if d.get(flag)? == 1 && (flag != "unorm" || sampled) && (flag != "a16" || !a16_ok) && (flag != "d16" || !d16) {
                self.why = Some(why.into());
                return None;
            }
        }
        // NSA: address k is vaddr for k = 0, else the k-th extra register byte (padding bytes are never read)
        let nsa: Vec<u16> = d.nsa.iter().map(|&b| b as u16).collect();
        let rsrc = (d.get("srsrc")? * 4) as u8;
        let Some(res) = self.f.descs.get(&rsrc).copied() else {
            self.why = Some("descriptor not traced".into());
            return None;
        };
        if res.heap != Heap::Resource || res.dwords < 4 {
            self.why = Some("not a resource descriptor".into());
            return None;
        }
        let cube = d.get("dim")? == 3;
        if matches!(d.get("dim")?, 6 | 7) || (cube && !matches!(name, "image_sample" | "image_sample_lz" | "image_sample_l" | "image_sample_b")) {
            self.why = Some(["", "", "", "cube", "", "", "msaa", "msaa array"][d.get("dim")? as usize].into());
            return None;
        }
        let (ty, rw, ncoord) = match d.get("dim")? {
            0 => ("Texture1D", "RWTexture1D", 1),
            1 => ("Texture2D", "RWTexture2D", 2),
            2 => ("Texture3D", "RWTexture3D", 3),
            4 => ("Texture1DArray", "RWTexture1DArray", 2),
            5 => ("Texture2DArray", "RWTexture2DArray", 3),
            3 => ("TextureCube", "", 3),
            _ => return None, // MSAA: next step
        };
        // a load through a UAV descriptor reads the host's UAV
        let ty = if name == "image_load" && !rw.is_empty() && self.desc_is_uav(rsrc) { rw } else { ty };
        let vaddr = d.get("vaddr")? as u16;
        let vdata = d.get("vdata")? as u16;
        let dmask = d.get("dmask")? as u8;
        let addr = |l: &mut Self, k: u16| -> String {
            let r = if nsa.is_empty() || k == 0 { vaddr + k } else { nsa.get(k as usize - 1).copied().unwrap_or(vaddr + k) };
            l.out.vgprs.insert(r);
            format!("v{r}")
        };
        // address component j as a float: a dword, or with a16 the half j % 2 of register j / 2
        let fcomp = |l: &mut Self, j: u16| -> String {
            if a16 {
                format!("f16tof32({} >> {}u)", addr(l, j / 2), 16 * (j % 2))
            } else {
                format!("asfloat({})", addr(l, j))
            }
        };
        let fcoord = |l: &mut Self, first: u16| -> String {
            let parts: Vec<String> = (0..ncoord as u16).map(|k| fcomp(l, first + k)).collect();
            if cube {
                // (s, t, face) as the console takes them, back to the direction the host takes
                return format!("sir_cube_dir({})", parts.join(", "));
            }
            format!("float{}({})", ncoord, parts.join(", ")).replace("float1(", "(")
        };
        let icoord = |l: &mut Self, extra_mip: bool| -> String {
            // with a16, integer coordinates are 16 bits each, two per register
            let mut parts: Vec<String> = (0..ncoord as u16)
                .map(|k| if a16 { format!("(int)(({} >> {}u) & 0xffffu)", addr(l, k / 2), 16 * (k % 2)) } else { format!("(int){}", addr(l, k)) })
                .collect();
            if extra_mip {
                parts.push("0".into());
            }
            format!("int{}({})", parts.len(), parts.join(", ")).replace("int1(", "(")
        };
        let sampler = |l: &mut Self| -> Option<String> {
            let s = (d.get("ssamp")? * 4) as u8;
            let Some(sd) = l.f.descs.get(&s).copied() else {
                // an immediate sampler: the four words are constants the code wrote
                let words: Option<Vec<u32>> = (0..4u8).map(|k| l.f.consts.get(&(s + k)).copied()).collect();
                let Some(w) = words else {
                    l.why = Some("sampler not traced".into());
                    return None;
                };
                let w = [w[0], w[1], w[2], w[3]];
                let k = match l.out.immediate_samplers.iter().position(|x| *x == w) {
                    Some(k) => k,
                    None => {
                        l.out.immediate_samplers.push(w);
                        l.out.immediate_samplers.len() - 1
                    }
                };
                return Some(format!("sir_imm{k}"));
            };
            if sd.heap != Heap::Sampler {
                l.why = Some("sampler is no sampler descriptor".into());
                return None;
            }
            Some(format!("sir_smp{s}"))
        };
        let result = match name {
            "image_sample" => {
                let s = sampler(self)?;
                format!("t.Sample({s}, {})", fcoord(self, 0))
            }
            "image_sample_lz" => {
                let s = sampler(self)?;
                format!("t.SampleLevel({s}, {}, 0)", fcoord(self, 0))
            }
            "image_sample_l" => {
                let s = sampler(self)?;
                let c = fcoord(self, 0);
                format!("t.SampleLevel({s}, {c}, {})", fcomp(self, ncoord as u16))
            }
            "image_sample_b" => {
                let s = sampler(self)?;
                let b = addr(self, 0);
                format!("t.SampleBias({s}, {}, asfloat({b}))", fcoord(self, 1))
            }
            // the address is (z-compare, coordinates); the comparison result is one channel
            "image_sample_c_lz" | "image_sample_c" => {
                if cube {
                    self.why = Some("cube compare".into());
                    return None;
                }
                let s = sampler(self)?;
                // a heap sampler is read as a comparison sampler; an immediate one is declared as one
                let smp = match s.strip_prefix("sir_smp") {
                    Some(n) => format!("sir_cmp{n}"),
                    None => {
                        let k: usize = s.strip_prefix("sir_imm")?.parse().ok()?;
                        self.out.immediate_compare.insert(k);
                        s
                    }
                };
                let (dref, c) = (fcomp(self, 0), fcoord(self, 1));
                let f = if name == "image_sample_c_lz" { "SampleCmpLevelZero" } else { "SampleCmp" };
                format!("t.{f}({smp}, {c}, {dref}).xxxx")
            }
            // the address is (derivatives along x, derivatives along y, coordinates)
            "image_sample_d" => {
                if cube {
                    self.why = Some("cube gradients".into());
                    return None;
                }
                let g = match d.get("dim")? {
                    0 | 4 => 1u16,
                    1 | 5 => 2,
                    _ => 3,
                };
                let s = sampler(self)?;
                let vec = |l: &mut Self, first: u16| -> String {
                    let parts: Vec<String> = (0..g).map(|k| fcomp(l, first + k)).collect();
                    format!("float{g}({})", parts.join(", ")).replace("float1(", "(")
                };
                let (dx, dy) = (vec(self, 0), vec(self, g));
                let c = fcoord(self, 2 * g);
                format!("t.SampleGrad({s}, {c}, {dx}, {dy})")
            }
            // (the clamped level, the unclamped level)
            "image_get_lod" => {
                if !matches!(self.stage, super::Stage::Ps(_)) {
                    self.why = Some("level of detail outside a pixel shader".into());
                    return None;
                }
                let s = sampler(self)?;
                let c = fcoord(self, 0);
                format!("float4(t.CalculateLevelOfDetail({s}, {c}), t.CalculateLevelOfDetailUnclamped({s}, {c}), 0.0f, 0.0f)")
            }
            "image_gather4_lz" | "image_gather4_lz_o" => {
                let s = sampler(self)?;
                // _o: the first address register packs the texel offset (x bits 0-5, y bits 8-13, signed)
                let first = u16::from(name.ends_with("_o"));
                let offset = if first == 1 {
                    let o = addr(self, 0);
                    format!(", int2(((int){o} << 26) >> 26, ((int){o} << 18) >> 26)")
                } else {
                    String::new()
                };
                let ch = match dmask {
                    1 => "Red",
                    2 => "Green",
                    4 => "Blue",
                    8 => "Alpha",
                    _ => return None,
                };
                let c = fcoord(self, first);
                // a gather returns four texels: all four channels are written
                // a heap sampler is declared here; an immediate one is global
                let decl = match s.strip_prefix("sir_smp") {
                    Some(n) => {
                        if let Ok(k) = n.parse() {
                            self.use_desc(k, View::Sampler, 0);
                        }
                        format!(" SamplerState {s} = SamplerDescriptorHeap[s{n}];")
                    }
                    None => String::new(),
                };
                self.use_desc(rsrc, View::Tex(d.get("dim")? as u8), 0);
                let ix = vi(rsrc, NullKind::Tex(d.get("dim")? as u8));
                let _ = writeln!(
                    self.out.body,
                    "    {{ {ty}<float4> t = ResourceDescriptorHeap[{ix}];{decl} float4 r = t.Gather{ch}({s}, {c}{offset});"
                );
                // only active lanes write their result (inactive lanes keep their registers, as on the console)
                let guard = if self.out.exec_narrowed { "if (ex) " } else { "" };
                let xyzw = ["x", "y", "z", "w"];
                if d16 {
                    for k in 0..2u16 {
                        self.out.vgprs.insert(vdata + k);
                        let (a, b) = (xyzw[2 * k as usize], xyzw[2 * k as usize + 1]);
                        let _ = writeln!(self.out.body, "      {guard}v{} = sir_f16(r.{a}) | (sir_f16(r.{b}) << 16);", vdata + k);
                    }
                } else {
                    for k in 0..4u16 {
                        self.out.vgprs.insert(vdata + k);
                        let _ = writeln!(self.out.body, "      {guard}v{} = asuint(r.{});", vdata + k, xyzw[k as usize]);
                    }
                }
                self.out.body.push_str("    }\n");
                return Some(());
            }
            n if n.starts_with("image_atomic_") => {
                if cube {
                    self.why = Some("cube atomic".into());
                    return None;
                }
                let c = icoord(self, false).replace("int", "uint");
                return self.image_atomic(n, d, rw, rsrc, &c);
            }
            "image_load" => format!("t.Load({})", icoord(self, !ty.starts_with("RW") && ncoord < 4)),
            "image_load_mip" if !cube && ncoord < 4 => {
                let c = icoord(self, true);
                let mip = addr(self, ncoord as u16);
                format!("t.Load({})", c.replacen(", 0)", &format!(", (int){mip})"), 1))
            }
            "image_store" => {
                let c = icoord(self, false).replace("int", "uint");
                let vals: Vec<String> = (0..4u16).map(|k| {
                    if dmask >> k & 1 == 1 {
                        let idx = (dmask & ((1 << k) - 1)).count_ones() as u16;
                        self.out.vgprs.insert(vdata + idx);
                        format!("asfloat(v{})", vdata + idx)
                    } else {
                        "0.0f".into()
                    }
                }).collect();
                let guard = if self.f.narrowed { "if (ex) " } else { "" };
                self.use_desc(rsrc, View::RwTex(d.get("dim")? as u8), 0);
                let ix = vi(rsrc, NullKind::RwTex(d.get("dim")? as u8));
                let _ = writeln!(self.out.body, "    {{ {rw}<float4> t = ResourceDescriptorHeap[{ix}]; {guard}t[{c}] = float4({}); }}", vals.join(", "));
                return Some(());
            }
            _ => return None,
        };
        let smp = match (result.find("sir_smp"), result.find("sir_cmp")) {
            (Some(i), _) => {
                let n: String = result[i + 7..].chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(k) = n.parse() {
                    self.use_desc(k, View::Sampler, 0);
                }
                format!(" SamplerState sir_smp{n} = SamplerDescriptorHeap[s{n}];")
            }
            (None, Some(i)) => {
                let n: String = result[i + 7..].chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(k) = n.parse() {
                    self.use_desc(k, View::SamplerCmp, 0);
                }
                format!(" SamplerComparisonState sir_cmp{n} = SamplerDescriptorHeap[s{n}];")
            }
            _ => String::new(),
        };
        let dim = d.get("dim")? as u8;
        self.use_desc(rsrc, if ty.starts_with("RW") { View::RwTex(dim) } else { View::Tex(dim) }, 0);
        let ix = vi(rsrc, if ty.starts_with("RW") { NullKind::RwTex(dim) } else { NullKind::Tex(dim) });
        let _ = writeln!(self.out.body, "    {{ {ty}<float4> t = ResourceDescriptorHeap[{ix}];{smp} float4 r = {result};");
        let guard = if self.f.narrowed { "if (ex) " } else { "" };
        let chans: Vec<&str> = (0..4usize).filter(|c| dmask >> c & 1 == 1).map(|c| ["x", "y", "z", "w"][c]).collect();
        if d16 {
            for (k, pair) in chans.chunks(2).enumerate() {
                let v = vdata + k as u16;
                self.out.vgprs.insert(v);
                let hi = pair.get(1).map_or("0u".to_string(), |c| format!("(sir_f16(r.{c}) << 16)"));
                let _ = writeln!(self.out.body, "      {guard}v{v} = sir_f16(r.{}) | {hi};", pair[0]);
            }
        } else {
            for (k, c) in chans.iter().enumerate() {
                let v = vdata + k as u16;
                self.out.vgprs.insert(v);
                let _ = writeln!(self.out.body, "      {guard}v{v} = asuint(r.{c});");
            }
        }
        self.out.body.push_str("    }\n");
        Some(())
    }
}

impl Lifter<'_> {
    /// `image_atomic_*` as `Interlocked*` on `RW<dim><uint>`, or on `<uint64_t>` for the 64-bit forms (dmask 3,
    /// cmpswap 0xf; the runtime gives an atomic image an `R32_UINT` / `R32G32_UINT` view; signed min/max
    /// compare as `int` on the same bits). `glc` returns the texel's value before the operation into `vdata`
    /// (and `vdata + 1`). `cmpswap` takes (new value, compare) from `vdata`. Only active lanes take part.
    /// Wrapping `inc`/`dec` and the float forms are refused.
    fn image_atomic(&mut self, name: &str, d: &Decoded, rw: &str, rsrc: u8, c: &str) -> Option<()> {
        let op = name.strip_prefix("image_atomic_")?;
        let f = match op {
            "add" | "sub" => "InterlockedAdd",
            "smin" | "umin" => "InterlockedMin",
            "smax" | "umax" => "InterlockedMax",
            "and" => "InterlockedAnd",
            "or" => "InterlockedOr",
            "xor" => "InterlockedXor",
            "swap" => "InterlockedExchange",
            "cmpswap" => "InterlockedCompareExchange",
            _ => {
                self.why = Some(format!("image atomic {op}"));
                return None;
            }
        };
        let (dmask, glc) = (d.get("dmask")?, d.get("glc")? == 1);
        // one dword, or two (dmask 3; cmpswap 0xf): 64-bit atomics on a `uint64_t` texel (SM 6.6)
        let wide = match (dmask, op == "cmpswap") {
            (1, false) | (3, true) => false,
            (3, false) | (15, true) => true,
            _ => {
                self.why = Some(format!("image atomic dmask {dmask}"));
                return None;
            }
        };
        let vdata = d.get("vdata")? as u16;
        let signed = matches!(op, "smin" | "smax");
        let ty = match (wide, signed) {
            (false, false) => "uint",
            (false, true) => "int",
            (true, false) => "uint64_t",
            (true, true) => "int64_t",
        };
        let n = if wide { 2 } else { 1 };
        let val = |l: &mut Self, v: u16| -> String {
            for k in 0..n {
                l.out.vgprs.insert(v + k);
            }
            if wide {
                format!("({ty})(((uint64_t)v{} << 32) | (uint64_t)v{v})", v + 1)
            } else if signed {
                format!("(int)v{v}")
            } else {
                format!("v{v}")
            }
        };
        let src = match op {
            "sub" => format!("({ty})0 - {}", val(self, vdata)),
            "cmpswap" => {
                let (new, cmp) = (val(self, vdata), val(self, vdata + n));
                format!("{cmp}, {new}")
            }
            _ => val(self, vdata),
        };
        let guard = if self.f.narrowed { "if (ex) " } else { "" };
        let ret = match (glc, wide) {
            (false, _) => String::new(),
            (true, false) => format!(" v{vdata} = (uint)o;"),
            (true, true) => format!(" v{vdata} = (uint)o; v{} = (uint)((uint64_t)o >> 32);", vdata + 1),
        };
        // an exchange must name the original; the others only when it is returned
        let orig = if glc || matches!(op, "swap" | "cmpswap") { ", o" } else { "" };
        let ix = vi(rsrc, NullKind::RwTexUint(d.get("dim")? as u8));
        let _ = writeln!(
            self.out.body,
            "    {{ {rw}<{ty}> t = ResourceDescriptorHeap[{ix}]; {guard}{{ {ty} o; {f}(t[{c}], {src}{orig});{ret} }} }}"
        );
        Some(())
    }
}

impl Lifter<'_> {
    /// `global_load_*` / `global_store_*` (FLAT, global segment): window addressing of the 64-bit address,
    /// `saddr` (an SGPR pair, wave-uniform window) plus the 32-bit `vaddr` offset, or the VGPR pair `vaddr`
    /// (the window per lane), plus the signed 12-bit offset. A carry out of the low 32 bits is not followed (a
    /// buffer does not cross a 4 GiB window).
    pub(super) fn global(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let (store, rest) = match (name.strip_prefix("global_load_"), name.strip_prefix("global_store_")) {
            (Some(r), _) => (false, r),
            (_, Some(r)) => (true, r),
            _ => return None,
        };
        let n: u16 = match rest {
            "dword" => 1,
            "dwordx2" => 2,
            "dwordx3" => 3,
            "dwordx4" => 4,
            _ => {
                self.why = Some(format!("global {rest}"));
                return None;
            }
        };
        if d.get("lds")? == 1 {
            return None;
        }
        let off = ((d.get("offset")? << 20) as i32) >> 20;
        let a = d.get("addr")? as u16;
        let saddr = d.get("saddr")?;
        self.out.vgprs.insert(a);
        let (lo, win) = if matches!(enc::src(saddr), Src::Null) || saddr == 0x7f {
            self.out.vgprs.insert(a + 1);
            (format!("v{a}"), format!("NonUniformResourceIndex({}u + (v{} & {}u))", super::WINDOW_HEAP_BASE, a + 1, super::WINDOW_COUNT - 1))
        } else {
            let (sl, sh) = (self.src(saddr, None)?, self.src(saddr + 1, None)?);
            (format!("{sl} + v{a}"), format!("{}u + ({sh} & {}u)", super::WINDOW_HEAP_BASE, super::WINDOW_COUNT - 1))
        };
        self.out.notes.insert("global memory: window addressing");
        let _ = writeln!(self.out.body, "    {{ RWByteAddressBuffer w = ResourceDescriptorHeap[{win}]; uint a = ({lo} + (uint){off}) & ~3u;");
        let data = if store { d.get("data")? } else { d.get("vdst")? } as u16;
        let guard = if self.f.narrowed { "if (ex) " } else { "" };
        for k in 0..n {
            self.out.vgprs.insert(data + k);
            if store {
                let _ = writeln!(self.out.body, "      {guard}w.Store(a + {}u, v{});", 4 * k, data + k);
            } else {
                self.vwrite_raw(data + k, format!("w.Load(a + {}u)", 4 * k));
            }
        }
        self.out.body.push_str("    }\n");
        Some(())
    }
}

/// The host heap index of a buffer descriptor's own view (CBV, UAV, typed view): the shadow entry's alternate
/// (`crates/d3d12x` heap layout: the console index itself holds a raw SRV of the buffer's bytes).
fn alternate(index_sgpr: u8) -> String {
    format!("sir_shadow.Load(sir_sh(s{index_sgpr}) + 8u)")
}

/// The console index in SGPR `s` as the host heap index to read as `kind` ([`super::CONSOLE_VIEWS`]).
fn vi(s: u8, kind: NullKind) -> String {
    super::view_index(&format!("s{s}"), kind)
}

/// The shadow entry's lead: bytes from the 16-byte aligned start of the raw views to the buffer's first byte.
fn lead(index_sgpr: u8) -> String {
    format!("sir_shadow.Load(sir_sh(s{index_sgpr}) + 12u)")
}

/// The 21-bit signed SMEM offset.
fn sext21(v: u32) -> i32 {
    ((v << 11) as i32) >> 11
}
