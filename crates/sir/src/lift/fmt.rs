//! Typed buffer access with the format in the instruction (`tbuffer_load_format_*`, MTBUF): a vertex fetch
//! the program does itself. The element's bytes are read as dwords (window addressing for a V# held as data,
//! the heap's raw view for a traced descriptor) and each component is unpacked by its number format, as the
//! console's texture unit does. The V#'s `dst_sel` (word 3, bits 0-11) then picks the channels.
//!
//! GFX10 unified buffer formats (`BUF_FMT_*`). AMD names the fields from the most significant: `2_10_10_10` is
//! x in bits 0-9 and w in bits 30-31; `10_11_11` is x 0-10, y 11-21, z 22-31.

use std::fmt::Write as _;

use rebuild_shader::gfx10::enc::Decoded;

use super::res::Heap;
use super::Lifter;

/// How a component's bits become a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Num {
    Unorm,
    Snorm,
    Uscaled,
    Sscaled,
    Uint,
    Sint,
    Float,
}

impl Num {
    /// The value is a float (the default `w` is 1.0) rather than an integer (1).
    pub fn is_float(self) -> bool {
        !matches!(self, Num::Uint | Num::Sint)
    }
}

/// A buffer format: component widths from bit 0 up, and the number format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    pub bits: &'static [u32],
    pub num: Num,
}

impl Format {
    /// Bytes in one element.
    pub fn bytes(&self) -> u32 {
        self.bits.iter().sum::<u32>() / 8
    }
}

/// The GFX10 buffer format `f` (7 bits), or `None` for an invalid or unknown one.
pub fn buf_format(f: u32) -> Option<Format> {
    use Num::*;
    const N6: [Num; 6] = [Unorm, Snorm, Uscaled, Sscaled, Uint, Sint];
    const N7: [Num; 7] = [Unorm, Snorm, Uscaled, Sscaled, Uint, Sint, Float];
    let (bits, num): (&'static [u32], Num) = match f {
        1..=6 => (&[8], N6[f as usize - 1]),
        7..=13 => (&[16], N7[f as usize - 7]),
        14..=19 => (&[8, 8], N6[f as usize - 14]),
        20..=22 => (&[32], [Uint, Sint, Float][f as usize - 20]),
        23..=29 => (&[16, 16], N7[f as usize - 23]),
        30..=36 => (&[11, 11, 10], N7[f as usize - 30]),
        37..=43 => (&[10, 11, 11], N7[f as usize - 37]),
        44..=49 => (&[2, 10, 10, 10], N6[f as usize - 44]),
        50..=55 => (&[10, 10, 10, 2], N6[f as usize - 50]),
        56..=61 => (&[8, 8, 8, 8], N6[f as usize - 56]),
        62..=64 => (&[32, 32], [Uint, Sint, Float][f as usize - 62]),
        65..=71 => (&[16, 16, 16, 16], N7[f as usize - 65]),
        72..=74 => (&[32, 32, 32], [Uint, Sint, Float][f as usize - 72]),
        75..=77 => (&[32, 32, 32, 32], [Uint, Sint, Float][f as usize - 75]),
        _ => return None,
    };
    Some(Format { bits, num })
}

/// HLSL for component `c` (as `uint` bits of the value: a float's bits, or the integer) of an element whose
/// dwords are `sir_e[k]`.
pub fn component(f: &Format, c: usize) -> Option<String> {
    let Some(&b) = f.bits.get(c) else {
        // a channel the format does not have: 0, and 1 for w
        return Some(match (c, f.num.is_float()) {
            (3, true) => "0x3f800000u".into(),
            (3, false) => "1u".into(),
            _ => "0u".into(),
        });
    };
    let o: u32 = f.bits[..c].iter().sum();
    let raw = if b == 32 { format!("sir_e[{}]", o / 32) } else { format!("((sir_e[{}] >> {}u) & {}u)", o / 32, o % 32, (1u64 << b) - 1) };
    let sext = if b == 32 { format!("(int){raw}") } else { format!("(((int){raw} << {}) >> {})", 32 - b, 32 - b) };
    let max_u = ((1u64 << b) - 1) as f64;
    let max_s = ((1u64 << (b - 1)) - 1) as f64;
    Some(match f.num {
        Num::Uint => raw,
        Num::Sint => format!("(uint){sext}"),
        Num::Unorm => format!("asuint((float){raw} / {max_u:.1}f)"),
        Num::Snorm => format!("asuint(max((float){sext} / {max_s:.1}f, -1.0f))"),
        Num::Uscaled => format!("asuint((float){raw})"),
        Num::Sscaled => format!("asuint((float){sext})"),
        Num::Float => match b {
            32 => raw,
            16 => format!("asuint(f16tof32({raw}))"),
            // 11- and 10-bit floats: a 5-bit exponent (bias 15) and no sign, so the half with the same bits
            11 | 10 => format!("asuint(f16tof32({raw} << {}u))", 15 - b),
            _ => return None,
        },
    })
}

impl Lifter<'_> {
    /// `tbuffer_load_format_x`-`xyzw`: see the module documentation. The address is `base + soffset + index *
    /// stride + offset` as for untyped access; an element out of range reads 0 (and 1 in `w`).
    pub(super) fn tbuffer(&mut self, name: &str, d: &Decoded) -> Option<()> {
        let n = match name.strip_prefix("tbuffer_load_format_")? {
            "x" => 1u16,
            "xy" => 2,
            "xyz" => 3,
            "xyzw" => 4,
            _ => return None, // stores and d16 forms
        };
        if d.get("tfe")? == 1 {
            self.why = Some("tfe".into());
            return None;
        }
        let Some(f) = buf_format(d.get("format")?) else {
            self.why = Some(format!("buffer format {}", d.get("format")?));
            return None;
        };
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
        let mut head = String::new();
        // (buffer, base address, stride, records, word 3 when known as a constant or `None` when read at run time)
        let w3: Option<String>;
        match self.f.descs.get(&rsrc).copied() {
            Some(res) => {
                if res.heap != Heap::Resource {
                    self.why = Some("not a resource descriptor".into());
                    return None;
                }
                // the heap's raw view of the buffer (from `lead` bytes below it), its stride and records from the
                // shadow table
                self.out.uses_shadow = true;
                let _ = write!(
                    head,
                    "    {{ ByteAddressBuffer w = ResourceDescriptorHeap[s{rsrc}]; uint4 sh = sir_shadow.Load4(s{rsrc} * 16u); uint sir_st = sh.x, sir_nr = sh.y, sir_i = {idx}, sir_o = {voff}; uint a = sh.w + {soff} + sir_i * sir_st + sir_o;"
                );
                // a heap view's channel order is the view's: identity here
                w3 = Some(format!("{}u", 4 | 5 << 3 | 6 << 6 | 7 << 9));
            }
            None => {
                let words: Option<Vec<String>> = (0..4u32).map(|k| self.src(u32::from(rsrc) + k, None)).collect();
                let Some(w) = words else {
                    self.why = Some("descriptor not traced".into());
                    return None;
                };
                let known = self.f.consts.get(&(rsrc + 3)).copied();
                if known.is_some_and(|x| x >> 23 & 1 == 1) {
                    self.why = Some("add_tid_enable".into());
                    return None;
                }
                self.out.notes.insert("buffer access through a V# held as data: window addressing");
                let _ = write!(
                    head,
                    "    {{ RWByteAddressBuffer w = ResourceDescriptorHeap[{}u + ({} & {}u)]; uint sir_st = ({} >> 16) & 0x3fffu, sir_nr = {}, sir_i = {idx}, sir_o = {voff}; uint a = {} + {soff} + sir_i * sir_st + sir_o;",
                    super::WINDOW_HEAP_BASE,
                    w[1],
                    super::WINDOW_COUNT - 1,
                    w[1],
                    w[2],
                    w[0]
                );
                w3 = Some(match known {
                    Some(k) => format!("{k}u"),
                    None => w[3].clone(),
                });
            }
        }
        // in range: a structured buffer (stride set) by index, a raw one by the element's last byte
        let raw = format!("sir_o + {}u <= sir_nr", f.bytes());
        if idxen {
            let _ = write!(head, " bool sir_in = sir_st != 0u ? sir_i < sir_nr : {raw};");
        } else {
            let _ = write!(head, " bool sir_in = {raw};");
        }
        self.out.body.push_str(&head);
        self.out.body.push('\n');
        // the element's dwords; a format with sub-dword components may sit at any byte
        let dwords = f.bits.iter().sum::<u32>().div_ceil(32);
        let aligned = f.bits.iter().all(|b| *b == 32);
        let load = |k: u32| {
            if aligned {
                format!("w.Load((a & ~3u) + {}u)", 4 * k)
            } else {
                format!("sir_unaligned(w.Load((a & ~3u) + {}u), w.Load((a & ~3u) + {}u), a)", 4 * k, 4 * k + 4)
            }
        };
        let loads: Vec<String> = (0..dwords).map(load).collect();
        let _ = writeln!(self.out.body, "      uint sir_e[{dwords}] = {{ {} }};", loads.join(", "));
        let comps: Option<Vec<String>> = (0..4).map(|c| component(&f, c)).collect();
        let comps = comps?;
        let one = if f.num.is_float() { "0x3f800000u" } else { "1u" };
        let _ = writeln!(
            self.out.body,
            "      uint4 sir_c = sir_in ? uint4({}) : uint4(0u, 0u, 0u, {one}); uint sir_ds = {};",
            comps.join(", "),
            w3.unwrap_or_default()
        );
        let vdata = d.get("vdata")? as u16;
        for k in 0..n {
            self.vwrite_raw(vdata + k, format!("sir_dst_sel(sir_c, (sir_ds >> {}u) & 7u, {one})", 3 * k));
        }
        self.out.body.push_str("    }\n");
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_follow_the_gfx10_table() {
        assert_eq!(buf_format(77), Some(Format { bits: &[32, 32, 32, 32], num: Num::Float }));
        assert_eq!(buf_format(56), Some(Format { bits: &[8, 8, 8, 8], num: Num::Unorm }));
        assert_eq!(buf_format(29).unwrap().num, Num::Float);
        assert_eq!(buf_format(36).unwrap().bits, &[11, 11, 10]);
        assert_eq!(buf_format(50).unwrap().bits, &[10, 10, 10, 2]);
        assert_eq!(buf_format(0), None);
        assert_eq!(buf_format(78), None);
        assert_eq!(buf_format(65).unwrap().bytes(), 8);
    }

    #[test]
    fn components_unpack_by_number_format() {
        let f = buf_format(57).unwrap(); // 8_8_8_8 snorm
        assert_eq!(component(&f, 1).unwrap(), "asuint(max((float)(((int)((sir_e[0] >> 8u) & 255u) << 24) >> 24) / 127.0f, -1.0f))");
        let f = buf_format(29).unwrap(); // 16_16 float: z 0, w 1.0
        assert_eq!(component(&f, 0).unwrap(), "asuint(f16tof32(((sir_e[0] >> 0u) & 65535u)))");
        assert_eq!(component(&f, 2).unwrap(), "0u");
        assert_eq!(component(&f, 3).unwrap(), "0x3f800000u");
        let f = buf_format(36).unwrap(); // 11_11_10 float: z is a 10-bit float
        assert_eq!(component(&f, 2).unwrap(), "asuint(f16tof32(((sir_e[0] >> 22u) & 1023u) << 5u))");
        let f = buf_format(75).unwrap(); // 32 x4 uint
        assert_eq!(component(&f, 3).unwrap(), "sir_e[3]");
    }
}
