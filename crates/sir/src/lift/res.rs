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
                Some(p) if !self.out.written.contains(&base) => p,
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
        if !self.out.written.contains(&base) {
            self.out.live_in.insert(base);
        }
        self.out.sgprs.insert(dst);
        self.out.written.insert(dst);
        self.consts.remove(&dst);
        let _ = writeln!(self.out.body, "    s{dst} = (uint)((int)s{base} + {byte_off}) / {stride}u;");
        self.descs.insert(dst, Desc { heap, dwords, user_data: p.user_data });
        for k in 1..dwords {
            self.desc_words.insert(dst + k);
            self.out.written.insert(dst + k);
            self.consts.remove(&(dst + k));
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
        let desc = *self.descs.get(&base)?;
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
            self.out.written.insert(s);
            self.consts.remove(&s);
            self.desc_words.remove(&s);
            self.descs.remove(&s);
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

    /// Image sampling, loads and stores (2D, 2D array, 3D, 1D) through heap descriptors.
    pub(super) fn mimg(&mut self, name: &str, d: &Decoded) -> Option<()> {
        // unorm only changes how a sampler filters: loads, stores and atomics address texels by integer anyway
        let sampled = name.starts_with("image_sample") || name.starts_with("image_gather");
        for (flag, why) in [("a16", "16-bit address"), ("d16", "16-bit data"), ("tfe", "tfe"), ("lwe", "lwe"), ("unorm", "unnormalised")] {
            if d.get(flag)? == 1 && (flag != "unorm" || sampled) {
                self.why = Some(why.into());
                return None;
            }
        }
        // NSA: address k is vaddr for k = 0, else the k-th extra register byte (padding bytes are never read)
        let nsa: Vec<u16> = d.nsa.iter().map(|&b| b as u16).collect();
        let rsrc = (d.get("srsrc")? * 4) as u8;
        let Some(res) = self.descs.get(&rsrc).copied() else {
            self.why = Some("descriptor not traced".into());
            return None;
        };
        if res.heap != Heap::Resource || res.dwords < 4 {
            self.why = Some("not a resource descriptor".into());
            return None;
        }
        if matches!(d.get("dim")?, 3 | 6 | 7) {
            self.why = Some(["", "", "", "cube", "", "", "msaa", "msaa array"][d.get("dim")? as usize].into());
            return None;
        }
        let (ty, rw, ncoord) = match d.get("dim")? {
            0 => ("Texture1D", "RWTexture1D", 1),
            1 => ("Texture2D", "RWTexture2D", 2),
            2 => ("Texture3D", "RWTexture3D", 3),
            4 => ("Texture1DArray", "RWTexture1DArray", 2),
            5 => ("Texture2DArray", "RWTexture2DArray", 3),
            _ => return None, // cube and MSAA: next step
        };
        let vaddr = d.get("vaddr")? as u16;
        let vdata = d.get("vdata")? as u16;
        let dmask = d.get("dmask")? as u8;
        let addr = |l: &mut Self, k: u16| -> String {
            let r = if nsa.is_empty() || k == 0 { vaddr + k } else { nsa.get(k as usize - 1).copied().unwrap_or(vaddr + k) };
            l.out.vgprs.insert(r);
            format!("v{r}")
        };
        let fcoord = |l: &mut Self, first: u16| -> String {
            let parts: Vec<String> = (0..ncoord as u16).map(|k| format!("asfloat({})", addr(l, first + k))).collect();
            format!("float{}({})", ncoord, parts.join(", ")).replace("float1(", "(")
        };
        let icoord = |l: &mut Self, extra_mip: bool| -> String {
            let mut parts: Vec<String> = (0..ncoord as u16).map(|k| format!("(int){}", addr(l, k))).collect();
            if extra_mip {
                parts.push("0".into());
            }
            format!("int{}({})", parts.len(), parts.join(", ")).replace("int1(", "(")
        };
        let sampler = |l: &mut Self| -> Option<String> {
            let s = (d.get("ssamp")? * 4) as u8;
            let Some(sd) = l.descs.get(&s).copied() else {
                // an immediate sampler: the four words are constants the code wrote
                let words: Option<Vec<u32>> = (0..4u8).map(|k| l.consts.get(&(s + k)).copied()).collect();
                let Some(w) = words.filter(|_| (0..4u8).all(|k| !l.out.masks.contains(&(s + k)))) else {
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
                format!("t.SampleLevel({s}, {c}, asfloat({}))", addr(self, ncoord as u16))
            }
            "image_sample_b" => {
                let s = sampler(self)?;
                let b = addr(self, 0);
                format!("t.SampleBias({s}, {}, asfloat({b}))", fcoord(self, 1))
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
                for k in 0..4u16 {
                    self.out.vgprs.insert(vdata + k);
                    let _ = writeln!(self.out.body, "      v{} = asuint(r.{});", vdata + k, ["x", "y", "z", "w"][k as usize]);
                }
                self.out.body.push_str("    }\n");
                return Some(());
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
                let guard = if self.out.exec_narrowed { "if (ex) " } else { "" };
                let _ = writeln!(self.out.body, "    {{ {rw}<float4> t = ResourceDescriptorHeap[s{rsrc}]; {guard}t[{c}] = float4({}); }}", vals.join(", "));
                return Some(());
            }
            _ => return None,
        };
        let smp = match result.find("sir_smp") {
            Some(i) => {
                let n: String = result[i + 7..].chars().take_while(|c| c.is_ascii_digit()).collect();
                format!(" SamplerState sir_smp{n} = SamplerDescriptorHeap[s{n}];")
            }
            None => String::new(),
        };
        let _ = writeln!(self.out.body, "    {{ {ty}<float4> t = ResourceDescriptorHeap[s{rsrc}];{smp} float4 r = {result};");
        let mut k = 0u16;
        for c in 0..4u8 {
            if dmask >> c & 1 == 1 {
                self.out.vgprs.insert(vdata + k);
                let guard = if self.out.exec_narrowed { "if (ex) " } else { "" };
                let _ = writeln!(self.out.body, "      {guard}v{} = asuint(r.{});", vdata + k, ["x", "y", "z", "w"][c as usize]);
                k += 1;
            }
        }
        self.out.body.push_str("    }\n");
        Some(())
    }
}

/// The 21-bit signed SMEM offset.
fn sext21(v: u32) -> i32 {
    ((v << 11) as i32) >> 11
}
