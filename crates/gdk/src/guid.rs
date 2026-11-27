//! GUIDs as the title passes them (Windows `GUID` layout).

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

const fn hex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("not a GUID digit"),
    }
}

impl Guid {
    /// From the usual text form, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
    pub const fn parse(s: &str) -> Guid {
        let b = s.as_bytes();
        assert!(b.len() == 36, "a GUID has 36 characters");
        let mut n = [0u8; 16];
        let (mut i, mut j) = (0, 0);
        while i < 36 {
            if b[i] == b'-' {
                i += 1;
                continue;
            }
            n[j] = hex(b[i]) << 4 | hex(b[i + 1]);
            j += 1;
            i += 2;
        }
        assert!(j == 16, "a GUID has 32 digits");
        let mut data4 = [0u8; 8];
        let mut k = 0;
        while k < 8 {
            data4[k] = n[8 + k];
            k += 1;
        }
        Guid {
            data1: u32::from_be_bytes([n[0], n[1], n[2], n[3]]),
            data2: u16::from_be_bytes([n[4], n[5]]),
            data3: u16::from_be_bytes([n[6], n[7]]),
            data4,
        }
    }

    /// The 16 bytes as they sit in memory (and in an image's `.rdata`).
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..4].copy_from_slice(&self.data1.to_le_bytes());
        out[4..6].copy_from_slice(&self.data2.to_le_bytes());
        out[6..8].copy_from_slice(&self.data3.to_le_bytes());
        out[8..].copy_from_slice(&self.data4);
        out
    }

    pub fn from_bytes(b: &[u8; 16]) -> Guid {
        Guid {
            data1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            data2: u16::from_le_bytes([b[4], b[5]]),
            data3: u16::from_le_bytes([b[6], b[7]]),
            data4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
        }
    }

    /// A random (version 4, RFC 4122 variant) GUID: the shape every runtime GUID has.
    pub fn is_v4(b: &[u8; 16]) -> bool {
        b[7] >> 4 == 4 && b[8] >> 6 == 2
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let d = self.data4;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_bytes_and_back() {
        let text = "073b7dcb-1fcf-4030-94be-e3c9eb623428";
        let g = Guid::parse(text);
        assert_eq!(g.data1, 0x073b_7dcb);
        assert_eq!(g.data4, [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28]);
        assert_eq!(g.to_string(), text);
        let b = g.to_bytes();
        assert_eq!(&b[..4], &[0xcb, 0x7d, 0x3b, 0x07]);
        assert_eq!(Guid::from_bytes(&b), g);
        assert!(Guid::is_v4(&b));
        assert_eq!(size_of::<Guid>(), 16);
    }
}
