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

use super::Lifter;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl Lifter<'_> {
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
        for k in 1..dwords {
            self.wrote(dst + k);
            self.f.desc_words.insert(dst + k);
        }
        Some(())
    }

    /// `s_buffer_load_dword*` through a buffer descriptor from the heap: a constant buffer (a CBV in the
    /// title's table) or a raw buffer (an SRV).
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
        let bind = self.bind?;
        let cbv = bind.used.iter().any(|u| u.user_data == desc.user_data && u.kind == ResourceType::Cbv);
        let dw = off as u32 / 4;
        let _ = writeln!(self.out.body, "    {{");
        // with a register offset the dword index is dynamic: c.d[i >> 2][i & 3]
        if let Some(r) = &reg {
            let _ = writeln!(self.out.body, "        uint b = ({r} + {}u) >> 2;", off);
        }
        if cbv {
            self.out.uses_cb = true;
            let _ = writeln!(self.out.body, "        ConstantBuffer<SirCb> c = ResourceDescriptorHeap[s{base}];");
        } else {
            let _ = writeln!(self.out.body, "        ByteAddressBuffer c = ResourceDescriptorHeap[s{base}];");
        }
        for k in 0..n {
            let s = dst + k;
            self.out.sgprs.insert(s);
            self.wrote(s);
            let i = dw + u32::from(k);
            let e = match (&reg, cbv) {
                (None, true) => format!("c.d[{}].{}", i / 4, ["x", "y", "z", "w"][i as usize % 4]),
                (None, false) => format!("c.Load({}u)", i * 4),
                (Some(_), true) => format!("c.d[(b + {k}u) >> 2][(b + {k}u) & 3]"),
                (Some(_), false) => format!("c.Load((b + {k}u) << 2)"),
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
        let vaddr = d.get("vaddr")? as u16;
        let mut addr = format!("{}u + {soff}", d.get("offset")?);
        // with idxen, vaddr is the index and vaddr + 1 the offset (when offen)
        if offen {
            let v = vaddr + u16::from(idxen);
            self.out.vgprs.insert(v);
            addr = format!("v{v} + {addr}");
        }
        let mut guard = String::new();
        if idxen {
            // the stride and record count are the console descriptor's: from the shadow table
            self.out.vgprs.insert(vaddr);
            self.out.uses_shadow = true;
            addr = format!("v{vaddr} * sh.x + {addr}");
            guard = format!(" if (v{vaddr} >= sh.y) r = 0;");
        }
        let vdata = d.get("vdata")? as u16;
        let load = ["Load", "Load2", "Load3", "Load4"][n as usize - 1];
        let ty = if n == 1 { String::new() } else { n.to_string() };
        let sh = if idxen { format!(" uint2 sh = sir_shadow.Load2(s{rsrc} * 16u);") } else { String::new() };
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
            let _ = writeln!(self.out.body, "    {{ RWByteAddressBuffer b = ResourceDescriptorHeap[s{rsrc}];{sh} if ({cond}) b.{st}({addr}, {value}); }}");
            return Some(());
        }
        let _ = writeln!(self.out.body, "    {{ ByteAddressBuffer b = ResourceDescriptorHeap[s{rsrc}];{sh} uint{ty} r = b.{load}({addr});{guard}");
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
        let el = if idxen {
            self.out.vgprs.insert(vaddr);
            format!("v{vaddr}")
        } else {
            "0u".to_string()
        };
        let ch = ["x", "y", "z", "w"];
        if store {
            let vals: Vec<String> = (0..4u16)
                .map(|k| {
                    if k < n {
                        self.out.vgprs.insert(vdata + k);
                        format!("asfloat(v{})", vdata + k)
                    } else {
                        "0.0f".into()
                    }
                })
                .collect();
            let guard = if self.f.narrowed { "if (ex) " } else { "" };
            // only the channels the instruction names change: read, merge, write
            let merge: Vec<String> = (0..n).map(|k| format!("o.{} = {};", ch[k as usize], vals[k as usize])).collect();
            let _ = writeln!(
                self.out.body,
                "    {{ RWBuffer<float4> b = ResourceDescriptorHeap[s{rsrc}]; {guard}{{ float4 o = b[{el}]; {} b[{el}] = o; }} }}",
                merge.join(" ")
            );
            return Some(());
        }
        let _ = writeln!(self.out.body, "    {{ Buffer<float4> b = ResourceDescriptorHeap[s{rsrc}]; float4 r = b[{el}];");
        for k in 0..n {
            self.vwrite_raw(vdata + k, format!("asuint(r.{})", ch[k as usize]));
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
        let a16_ok = matches!(name, "image_sample" | "image_sample_lz" | "image_sample_l" | "image_load" | "image_store");
        for (flag, why) in [("a16", "16-bit address"), ("d16", "16-bit data"), ("tfe", "tfe"), ("lwe", "lwe"), ("unorm", "unnormalised")] {
            if d.get(flag)? == 1 && (flag != "unorm" || sampled) && (flag != "a16" || !a16_ok) {
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
            "image_gather4_lz" => {
                let s = sampler(self)?;
                let ch = match dmask {
                    1 => "Red",
                    2 => "Green",
                    4 => "Blue",
                    8 => "Alpha",
                    _ => return None,
                };
                let c = fcoord(self, 0);
                // a gather returns four texels: all four channels are written
                // a heap sampler is declared here; an immediate one is global
                let decl = match s.strip_prefix("sir_smp") {
                    Some(n) => format!(" SamplerState {s} = SamplerDescriptorHeap[s{n}];"),
                    None => String::new(),
                };
                let _ = writeln!(
                    self.out.body,
                    "    {{ {ty}<float4> t = ResourceDescriptorHeap[s{rsrc}];{decl} float4 r = t.Gather{ch}({s}, {c});"
                );
                // only active lanes write their result (inactive lanes keep their registers, as on the console)
                let guard = if self.out.exec_narrowed { "if (ex) " } else { "" };
                for k in 0..4u16 {
                    self.out.vgprs.insert(vdata + k);
                    let _ = writeln!(self.out.body, "      {guard}v{} = asuint(r.{});", vdata + k, ["x", "y", "z", "w"][k as usize]);
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
                let _ = writeln!(self.out.body, "    {{ {rw}<float4> t = ResourceDescriptorHeap[s{rsrc}]; {guard}t[{c}] = float4({}); }}", vals.join(", "));
                return Some(());
            }
            _ => return None,
        };
        let smp = match (result.find("sir_smp"), result.find("sir_cmp")) {
            (Some(i), _) => {
                let n: String = result[i + 7..].chars().take_while(|c| c.is_ascii_digit()).collect();
                format!(" SamplerState sir_smp{n} = SamplerDescriptorHeap[s{n}];")
            }
            (None, Some(i)) => {
                let n: String = result[i + 7..].chars().take_while(|c| c.is_ascii_digit()).collect();
                format!(" SamplerComparisonState sir_cmp{n} = SamplerDescriptorHeap[s{n}];")
            }
            _ => String::new(),
        };
        let _ = writeln!(self.out.body, "    {{ {ty}<float4> t = ResourceDescriptorHeap[s{rsrc}];{smp} float4 r = {result};");
        let mut k = 0u16;
        for c in 0..4u8 {
            if dmask >> c & 1 == 1 {
                self.out.vgprs.insert(vdata + k);
                let guard = if self.f.narrowed { "if (ex) " } else { "" };
                let _ = writeln!(self.out.body, "      {guard}v{} = asuint(r.{});", vdata + k, ["x", "y", "z", "w"][c as usize]);
                k += 1;
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
        let _ = writeln!(
            self.out.body,
            "    {{ {rw}<{ty}> t = ResourceDescriptorHeap[s{rsrc}]; {guard}{{ {ty} o; {f}(t[{c}], {src}{orig});{ret} }} }}"
        );
        Some(())
    }
}

/// The 21-bit signed SMEM offset.
fn sext21(v: u32) -> i32 {
    ((v << 11) as i32) >> 11
}
