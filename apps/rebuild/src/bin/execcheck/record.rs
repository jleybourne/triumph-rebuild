//! The recording pass, for programs that index the heap from data over several kinds of view (bindless
//! materials): the harness cannot place their views without knowing each index. Our program is instrumented so
//! every heap access first records (index, kind) in a UAV, and reads a slot only when the plan already holds a
//! view of that kind there (else a dummy of the kind: no access ever reads a view of the wrong type). The program
//! runs on every input set; the slots it named are added to the plan, and it runs again until it names no new
//! slot (an index read through a slot found in one round can lead to more in the next). Both sides then run on
//! the plan with exactly those slots.

use std::collections::BTreeMap;

use super::gpu::{buffer_desc, HEAP_SLOTS, SAMPLER_SLOTS};
use super::run::Plan;
use super::View;

/// Kinds a heap access can name, by bit.
const KINDS: u32 = 19;
/// Dummy views, one per kind, from this resource slot (and the last two sampler slots).
pub const DUMMY_BASE: u32 = HEAP_SLOTS - 32;
/// The known-slot table (a raw SRV) and the record (a raw UAV).
pub const KNOWN_SLOT: u32 = HEAP_SLOTS - 2;
pub const REC_SLOT: u32 = HEAP_SLOTS - 1;
pub const SAMPLER_DUMMY: u32 = SAMPLER_SLOTS - 2;
/// Words in the known table and the record: resource slots, sampler slots, then two overflow flags.
pub const WORDS: u32 = HEAP_SLOTS + SAMPLER_SLOTS + 2;
/// Rounds before the plan must have settled.
pub const ROUNDS: usize = 6;

pub fn bit(v: View) -> u32 {
    match v {
        View::Cbv => 0,
        View::Raw => 1,
        View::RwRaw => 2,
        View::Typed => 3,
        View::RwTyped => 4,
        View::Tex(d) => 5 + d.min(5) as u32,
        View::RwTex(d) => 11 + d.min(5) as u32,
        View::Sampler => 17,
        View::SamplerCmp => 18,
    }
}

pub fn view(bit: u32) -> View {
    match bit {
        0 => View::Cbv,
        1 => View::Raw,
        2 => View::RwRaw,
        3 => View::Typed,
        4 => View::RwTyped,
        5..=10 => View::Tex((bit - 5) as u8),
        11..=16 => View::RwTex((bit - 11) as u8),
        17 => View::Sampler,
        _ => View::SamplerCmp,
    }
}

/// The view an HLSL resource type is (the emitter's spellings).
fn view_of(ty: &str) -> Option<View> {
    let base = ty.split('<').next().unwrap_or(ty);
    Some(match base {
        "ConstantBuffer" => View::Cbv,
        "ByteAddressBuffer" => View::Raw,
        "RWByteAddressBuffer" => View::RwRaw,
        "Buffer" => View::Typed,
        "RWBuffer" => View::RwTyped,
        "Texture1D" => View::Tex(0),
        "Texture2D" => View::Tex(1),
        "Texture3D" => View::Tex(2),
        "TextureCube" => View::Tex(3),
        "Texture1DArray" => View::Tex(4),
        "Texture2DArray" => View::Tex(5),
        "RWTexture1D" => View::RwTex(0),
        "RWTexture2D" => View::RwTex(1),
        "RWTexture3D" => View::RwTex(2),
        "RWTexture1DArray" => View::RwTex(4),
        "RWTexture2DArray" => View::RwTex(5),
        "SamplerState" => View::Sampler,
        "SamplerComparisonState" => View::SamplerCmp,
        _ => return None,
    })
}

/// Our HLSL with every `T name = ResourceDescriptorHeap[i]` (and the sampler heap's) read through `sir_rec`, and
/// the kinds it names. Refused when a heap access has another form.
pub fn instrument(src: &str) -> Result<(String, Vec<View>), String> {
    let mut out = String::with_capacity(src.len() + 2048);
    let mut kinds = Vec::new();
    let mut at = 0;
    while let Some(k) = src[at..].find("DescriptorHeap[") {
        let open = at + k + "DescriptorHeap[".len();
        let sampler = src[..at + k].ends_with("Sampler");
        let head = &src[..at + k - if sampler { "Sampler".len() } else { "Resource".len() }];
        // `<type> <name> = ` before it
        let before = head.trim_end().strip_suffix('=').ok_or("a heap access that is not a declaration")?.trim_end();
        let name_at = before.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
        let ty_part = before[..name_at].trim_end();
        let ty_at = ty_part.rfind(|c: char| c.is_whitespace() || c == '{' || c == ';' || c == '(').map_or(0, |i| i + 1);
        let ty = &ty_part[ty_at..];
        let v = view_of(ty).ok_or_else(|| format!("heap access of type {ty}"))?;
        if !kinds.contains(&v) {
            kinds.push(v);
        }
        // the index, to its matching bracket
        let mut depth = 1;
        let mut close = open;
        for (i, c) in src[open..].char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err("heap index not closed".into());
        }
        out.push_str(&src[at..open]);
        out.push_str(&format!("{}({}, {}u)", if sampler { "sir_recs" } else { "sir_rec" }, &src[open..close], bit(v)));
        at = close;
    }
    out.push_str(&src[at..]);
    let prelude = format!(
        "// execcheck: the recording pass\n\
         // a plain load first: every lane setting the same bit with an atomic serialises on WARP\n\
         void sir_rec_or(RWByteAddressBuffer rec, uint a, uint k) {{ if (((rec.Load(a) >> k) & 1u) == 0u) {{ rec.InterlockedOr(a, 1u << k); }} }}\n\
         uint sir_rec(uint i, uint k) {{\n\
         \x20   RWByteAddressBuffer rec = ResourceDescriptorHeap[{REC_SLOT}];\n\
         \x20   ByteAddressBuffer known = ResourceDescriptorHeap[{KNOWN_SLOT}];\n\
         \x20   if (i >= {DUMMY_BASE}u) {{ sir_rec_or(rec, {o}u, k); return {DUMMY_BASE}u + k; }}\n\
         \x20   sir_rec_or(rec, i * 4u, k);\n\
         \x20   return ((known.Load(i * 4u) >> k) & 1u) != 0 ? i : {DUMMY_BASE}u + k;\n\
         }}\n\
         uint sir_recs(uint i, uint k) {{\n\
         \x20   RWByteAddressBuffer rec = ResourceDescriptorHeap[{REC_SLOT}];\n\
         \x20   ByteAddressBuffer known = ResourceDescriptorHeap[{KNOWN_SLOT}];\n\
         \x20   uint d = {SAMPLER_DUMMY}u + (k == 18u ? 1u : 0u);\n\
         \x20   if (i >= {SAMPLER_DUMMY}u) {{ sir_rec_or(rec, {os}u, k); return d; }}\n\
         \x20   sir_rec_or(rec, ({HEAP_SLOTS}u + i) * 4u, k);\n\
         \x20   return ((known.Load(({HEAP_SLOTS}u + i) * 4u) >> k) & 1u) != 0 ? i : d;\n\
         }}\n",
        o = (HEAP_SLOTS + SAMPLER_SLOTS) * 4,
        os = (HEAP_SLOTS + SAMPLER_SLOTS + 1) * 4,
    );
    Ok((prelude + &out, kinds))
}

/// The known table for a plan: per slot, the kinds a read may take there.
pub fn known(pl: &Plan) -> Vec<u8> {
    let mut w = vec![0u32; WORDS as usize];
    for (s, v) in &pl.slots {
        let mut m = 1u32 << bit(*v);
        // a raw or typed UAV is read as its SRV too (the merged view the harness binds)
        if *v == View::RwRaw {
            m |= 1 << bit(View::Raw);
        }
        if *v == View::RwTyped {
            m |= 1 << bit(View::Typed);
        }
        w[*s as usize] = m;
    }
    for (s, v) in &pl.sampler_slots {
        w[(HEAP_SLOTS + s) as usize] = 1 << bit(*v);
    }
    super::run::bytes_of(w.into_iter())
}

/// The record's starting contents and its buffer.
pub fn rec_desc() -> windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DESC {
    buffer_desc(WORDS as u64 * 4, true)
}

/// Adds what one record names to the plan. `Ok(true)`: a slot was new.
pub fn absorb(pl: &mut Plan, rec: &[u8]) -> Result<bool, String> {
    let w = |i: u32| u32::from_le_bytes(rec[i as usize * 4..i as usize * 4 + 4].try_into().unwrap());
    if w(HEAP_SLOTS + SAMPLER_SLOTS) != 0 || w(HEAP_SLOTS + SAMPLER_SLOTS + 1) != 0 {
        return Err("a recorded descriptor index past the harness heap".into());
    }
    let mut new = false;
    let mut add = |map: &mut BTreeMap<u32, View>, slot: u32, bits: u32| -> Result<(), String> {
        for b in (0..KINDS).filter(|b| bits >> b & 1 != 0) {
            let v = view(b);
            let m = match map.get(&slot) {
                Some(old) => super::run::merge(*old, v).map_err(|e| format!("{e} (recorded)"))?,
                None => v,
            };
            if map.get(&slot) != Some(&m) {
                new = true;
                map.insert(slot, m);
            }
        }
        Ok(())
    };
    for s in 0..DUMMY_BASE {
        let b = w(s);
        if b != 0 {
            add(&mut pl.slots, s, b)?;
        }
    }
    for s in 0..SAMPLER_DUMMY {
        let b = w(HEAP_SLOTS + s);
        if b != 0 {
            add(&mut pl.sampler_slots, s, b)?;
        }
    }
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruments_declarations() {
        let src = "{ Texture2D<float4> t = ResourceDescriptorHeap[sir_vi(s8, 0u)]; SamplerState q = SamplerDescriptorHeap[s28]; }\n    ConstantBuffer<SirCb> c = ResourceDescriptorHeap[s64];";
        let (out, kinds) = instrument(src).unwrap();
        assert!(out.contains("Texture2D<float4> t = ResourceDescriptorHeap[sir_rec(sir_vi(s8, 0u), 6u)];"), "{out}");
        assert!(out.contains("SamplerState q = SamplerDescriptorHeap[sir_recs(s28, 17u)];"), "{out}");
        assert!(out.contains("ConstantBuffer<SirCb> c = ResourceDescriptorHeap[sir_rec(s64, 0u)];"), "{out}");
        assert_eq!(kinds, vec![View::Tex(1), View::Sampler, View::Cbv]);
        for b in 0..KINDS {
            assert_eq!(bit(view(b)), b);
        }
    }
}
