//! A small PE32+ DLL built byte by byte for the tests: five sections, DIR64 relocations, imports by
//! name and by ordinal, a delay-load import, exports with a forwarder, a TLS callback and `.pdata`.

pub const BASE: u64 = 0x1_8000_0000;
pub const SIZE_OF_IMAGE: u32 = 0x6000;

pub const F0: u32 = 0x1000;
pub const F1: u32 = 0x1010;
pub const TLS_CB: u32 = 0x1020;
pub const DELAY_THUNK: u32 = 0x1030;
pub const ENTRY: u32 = 0x1040;

pub const STR: u32 = 0x2000;
pub const STR_TEXT: &[u8] = b"hello from rdata";
const IMPORT_DESC: u32 = 0x2100;
const ILT_KERNELX: u32 = 0x2200;
const ILT_XGR: u32 = 0x2220;
pub const IAT_KERNELX: u32 = 0x2240;
pub const IAT_XGR: u32 = 0x2260;
const HN_XMEMALLOC: u32 = 0x2280;
const HN_XGRI: u32 = 0x22a0;
const HN_BINKOPEN: u32 = 0x22d0;
const NAME_KERNELX: u32 = 0x2300;
const NAME_XGR: u32 = 0x2320;
const NAME_BINK: u32 = 0x2340;
const DELAY_DESC: u32 = 0x2400;
const DELAY_INT: u32 = 0x2440;
pub const DELAY_IAT: u32 = 0x2460;
const EXPORT_DIR: u32 = 0x2500;

pub const P_STR: u32 = 0x3000;
pub const P_F1: u32 = 0x3008;
pub const PLAIN: u32 = 0x3010;
pub const P_SELF: u32 = 0x3018;
const TLS_DIR: u32 = 0x3040;
const TLS_CBS: u32 = 0x3060;
const DELAY_HMOD: u32 = 0x3100;
const TLS_START: u32 = 0x3200;
const TLS_END: u32 = 0x3210;
pub const TLS_INDEX: u32 = 0x3220;

const PDATA: u32 = 0x4000;
const UNWIND: u32 = 0x4100;
const RELOC: u32 = 0x5000;

pub struct Synth {
    pub file: Vec<u8>,
    /// Every RVA that holds a DIR64 relocation.
    pub dir64: Vec<u32>,
}

struct V(Vec<u8>);
impl V {
    fn b(&mut self, at: u32, x: &[u8]) {
        self.0[at as usize..at as usize + x.len()].copy_from_slice(x);
    }
    fn u16(&mut self, at: u32, x: u16) {
        self.b(at, &x.to_le_bytes())
    }
    fn u32(&mut self, at: u32, x: u32) {
        self.b(at, &x.to_le_bytes())
    }
    fn u64(&mut self, at: u32, x: u64) {
        self.b(at, &x.to_le_bytes())
    }
    fn hint_name(&mut self, at: u32, hint: u16, name: &str) {
        self.u16(at, hint);
        self.b(at + 2, name.as_bytes());
    }
}

/// The DLL, linked for preferred base `base`.
pub fn dll(base: u64) -> Synth {
    let mut v = V(vec![0; SIZE_OF_IMAGE as usize]);
    let va = |rva: u32| base + rva as u64;

    // .text: a `ret` at each function
    for f in [F0, F1, TLS_CB, DELAY_THUNK, ENTRY] {
        v.b(f, &[0xc3]);
    }

    // .rdata: a string, the imports, the delay-load import, the exports
    v.b(STR, STR_TEXT);
    v.hint_name(HN_XMEMALLOC, 7, "XMemAlloc");
    v.hint_name(HN_XGRI, 0, "XGameRuntimeInitialize");
    v.hint_name(HN_BINKOPEN, 0, "BinkOpen");
    v.b(NAME_KERNELX, b"kernelx.dll");
    v.b(NAME_XGR, b"xgameruntime.dll");
    v.b(NAME_BINK, b"bink2.dll");
    for (ilt, iat) in [(ILT_KERNELX, IAT_KERNELX), (ILT_XGR, IAT_XGR)] {
        let thunks: &[u64] = if ilt == ILT_KERNELX {
            &[HN_XMEMALLOC as u64, (1 << 63) | 5]
        } else {
            &[HN_XGRI as u64]
        };
        for (i, t) in thunks.iter().enumerate() {
            v.u64(ilt + i as u32 * 8, *t);
            v.u64(iat + i as u32 * 8, *t);
        }
    }
    for (i, (ilt, name, iat)) in [(ILT_KERNELX, NAME_KERNELX, IAT_KERNELX), (ILT_XGR, NAME_XGR, IAT_XGR)]
        .into_iter()
        .enumerate()
    {
        let d = IMPORT_DESC + i as u32 * 20;
        v.u32(d, ilt);
        v.u32(d + 12, name);
        v.u32(d + 16, iat);
    }
    // delay-load: attributes 1 (RVAs), name, module handle, IAT, INT
    v.u32(DELAY_DESC, 1);
    v.u32(DELAY_DESC + 4, NAME_BINK);
    v.u32(DELAY_DESC + 8, DELAY_HMOD);
    v.u32(DELAY_DESC + 12, DELAY_IAT);
    v.u32(DELAY_DESC + 16, DELAY_INT);
    v.u64(DELAY_INT, HN_BINKOPEN as u64);
    v.u64(DELAY_IAT, va(DELAY_THUNK)); // the IAT starts at the image's own thunk: relocated

    // exports: base 1; Alpha, Beta, Gamma (forwarded)
    let (funcs, names, ords) = (EXPORT_DIR + 0x40, EXPORT_DIR + 0x50, EXPORT_DIR + 0x60);
    v.u32(EXPORT_DIR + 12, EXPORT_DIR + 0x80);
    v.u32(EXPORT_DIR + 16, 1);
    v.u32(EXPORT_DIR + 20, 3);
    v.u32(EXPORT_DIR + 24, 3);
    v.u32(EXPORT_DIR + 28, funcs);
    v.u32(EXPORT_DIR + 32, names);
    v.u32(EXPORT_DIR + 36, ords);
    v.b(EXPORT_DIR + 0x80, b"synth.dll");
    let fwd = EXPORT_DIR + 0xd0;
    v.b(fwd, b"kernelx.XMemFree");
    for (i, (name, at, target)) in [("Alpha", 0xa0, F0), ("Beta", 0xb0, F1), ("Gamma", 0xc0, fwd)]
        .into_iter()
        .enumerate()
    {
        v.b(EXPORT_DIR + at, name.as_bytes());
        v.u32(funcs + i as u32 * 4, target);
        v.u32(names + i as u32 * 4, EXPORT_DIR + at);
        v.u16(ords + i as u32 * 2, i as u16);
    }

    // .data: absolute pointers, a plain value, the TLS directory and its callbacks
    v.u64(P_STR, va(STR));
    v.u64(P_F1, va(F1));
    v.u32(PLAIN, 0xdead_beef);
    v.u64(P_SELF, va(P_SELF));
    v.u64(TLS_DIR, va(TLS_START));
    v.u64(TLS_DIR + 8, va(TLS_END));
    v.u64(TLS_DIR + 16, va(TLS_INDEX));
    v.u64(TLS_DIR + 24, va(TLS_CBS));
    v.u64(TLS_CBS, va(TLS_CB));

    // .pdata: two functions sharing one unwind record (version 1, no codes)
    for (i, f) in [F0, F1].into_iter().enumerate() {
        let e = PDATA + i as u32 * 12;
        v.u32(e, f);
        v.u32(e + 4, f + 8);
        v.u32(e + 8, UNWIND);
    }
    v.b(UNWIND, &[1, 0, 0, 0]);

    // .reloc
    let dir64 = vec![DELAY_IAT, P_STR, P_F1, P_SELF, TLS_DIR, TLS_DIR + 8, TLS_DIR + 16, TLS_DIR + 24, TLS_CBS];
    let mut at = RELOC;
    for page in [0x2000u32, 0x3000] {
        let mut e: Vec<u16> = dir64.iter().filter(|r| *r & !0xfff == page).map(|r| (10 << 12) | (*r & 0xfff) as u16).collect();
        if e.len() % 2 == 1 {
            e.push(0); // ABSOLUTE padding to a 4-byte block
        }
        let size = 8 + e.len() as u32 * 2;
        v.u32(at, page);
        v.u32(at + 4, size);
        for (i, x) in e.iter().enumerate() {
            v.u16(at + 8 + i as u32 * 2, *x);
        }
        at += size;
    }
    let reloc_size = at - RELOC;

    // headers
    let nt = 0x80u32;
    v.b(0, b"MZ");
    v.u32(0x3c, nt);
    v.b(nt, b"PE\0\0");
    let fh = nt + 4;
    v.u16(fh, 0x8664);
    v.u16(fh + 2, 5);
    v.u16(fh + 16, 240);
    v.u16(fh + 18, 0x2022); // DLL | EXECUTABLE | LARGE_ADDRESS_AWARE
    let oh = fh + 20;
    v.u16(oh, 0x20b);
    v.u32(oh + 16, ENTRY);
    v.u64(oh + 24, base);
    v.u32(oh + 32, 0x1000);
    v.u32(oh + 36, 0x200);
    v.u32(oh + 56, SIZE_OF_IMAGE);
    v.u32(oh + 60, 0x400);
    v.u16(oh + 68, 3);
    v.u16(oh + 70, 0x160);
    v.u64(oh + 72, 0x10_0000);
    v.u64(oh + 80, 0x1000);
    v.u32(oh + 108, 16);
    let dirs: [(usize, u32, u32); 7] = [
        (0, EXPORT_DIR, 0x100),
        (1, IMPORT_DESC, 3 * 20),
        (3, PDATA, 24),
        (5, RELOC, reloc_size),
        (9, TLS_DIR, 40),
        (12, IAT_KERNELX, 0x28),
        (13, DELAY_DESC, 64),
    ];
    for (i, rva, size) in dirs {
        v.u32(oh + 112 + i as u32 * 8, rva);
        v.u32(oh + 116 + i as u32 * 8, size);
    }
    // name, rva, virtual size, raw size, characteristics
    let sections: [(&[u8], u32, u32, u32, u32); 5] = [
        (b".text", 0x1000, 0x100, 0x200, 0x6000_0020),
        (b".rdata", 0x2000, 0x600, 0x600, 0x4000_0040),
        (b".data", 0x3000, 0x300, 0x200, 0xc000_0040),
        (b".pdata", 0x4000, 0x104, 0x200, 0x4000_0040),
        (b".reloc", 0x5000, reloc_size, 0x200, 0x4200_0040),
    ];
    let mut file = v.0[..0x400].to_vec();
    let st = oh + 240;
    let mut raw_at = 0x400u32;
    let mut out_headers = Vec::new();
    for (i, (name, rva, vsize, raw, ch)) in sections.into_iter().enumerate() {
        let s = (st + i as u32 * 40) as usize;
        let mut h = [0u8; 40];
        h[..name.len()].copy_from_slice(name);
        h[8..12].copy_from_slice(&vsize.to_le_bytes());
        h[12..16].copy_from_slice(&rva.to_le_bytes());
        h[16..20].copy_from_slice(&raw.to_le_bytes());
        h[20..24].copy_from_slice(&raw_at.to_le_bytes());
        h[36..40].copy_from_slice(&ch.to_le_bytes());
        out_headers.push((s, h));
        file.extend_from_slice(&v.0[rva as usize..(rva + raw) as usize]);
        raw_at += raw;
    }
    for (s, h) in out_headers {
        file[s..s + 40].copy_from_slice(&h);
    }
    Synth { file, dir64 }
}
