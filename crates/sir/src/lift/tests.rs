use super::*;
use rebuild_shader::gfx10::{self, name};
use std::path::PathBuf;
use std::process::Command;

/// The opcode with AMD name `n` in encoding `e` (from the decoder's own table).
fn op(e: Encoding, n: &str) -> u32 {
    (0..1024u16).find(|&o| name(e, o) == Some(n)).unwrap_or_else(|| panic!("no {n} in {e:?}")) as u32
}

fn vop1(n: &str, vdst: u32, src0: u32) -> Vec<u32> {
    vec![0x7E00_0000 | vdst << 17 | op(Encoding::Vop1, n) << 9 | src0]
}
fn vop2(n: &str, vdst: u32, src0: u32, vsrc1: u32) -> Vec<u32> {
    vec![op(Encoding::Vop2, n) << 25 | vdst << 17 | vsrc1 << 9 | src0]
}
fn vopc(n: &str, src0: u32, vsrc1: u32) -> Vec<u32> {
    vec![0x7C00_0000 | op(Encoding::Vopc, n) << 17 | vsrc1 << 9 | src0]
}
/// VOP3A: abs/neg bit masks per operand, clamp, omod.
#[allow(clippy::too_many_arguments)]
fn vop3(n: &str, vdst: u32, srcs: [u32; 3], abs: u32, neg: u32, clamp: u32, omod: u32) -> Vec<u32> {
    vec![
        0x35 << 26 | op(Encoding::Vop3, n) << 16 | clamp << 15 | abs << 8 | vdst,
        neg << 29 | omod << 27 | srcs[2] << 18 | srcs[1] << 9 | srcs[0],
    ]
}
fn sop2(n: &str, sdst: u32, a: u32, b: u32) -> Vec<u32> {
    vec![0b10 << 30 | op(Encoding::Sop2, n) << 23 | sdst << 16 | b << 8 | a]
}
fn sop1(n: &str, sdst: u32, a: u32) -> Vec<u32> {
    vec![0x17D << 23 | sdst << 16 | op(Encoding::Sop1, n) << 8 | a]
}
fn sopc(n: &str, a: u32, b: u32) -> Vec<u32> {
    vec![0x17E << 23 | op(Encoding::Sopc, n) << 16 | b << 8 | a]
}
fn endpgm() -> Vec<u32> {
    vec![0x17F << 23 | op(Encoding::Sopp, "s_endpgm") << 16]
}

const V: u32 = 256; // VGPR operand base
const LIT: u32 = 255;

fn program(parts: &[Vec<u32>]) -> Vec<Instruction> {
    let bytes: Vec<u8> = parts.iter().flatten().chain(endpgm().iter()).flat_map(|w| w.to_le_bytes()).collect();
    gfx10::decode(&bytes).expect("decodes")
}

#[test]
fn moves_float_math_modifiers_and_literals() {
    let p = program(&[
        vop1("v_mov_b32", 1, V + 0),
        vop2("v_add_f32", 2, V + 0, 1),
        vop2("v_mul_legacy_f32", 3, 242, 2), // 1.0 * v2, legacy
        vec![vop2("v_madak_f32", 4, V + 0, 1)[0], 0x4000_0000], // v0 * v1 + 2.0
        vop3("v_fma_f32", 5, [V + 0, V + 1, V + 2], 0b001, 0b010, 1, 1), // fma(|v0|, -v1, v2) * 2, clamped
        vop1("v_rcp_f32", 6, V + 5),
        vop2("v_cvt_pkrtz_f16_f32", 7, V + 5, 6),
    ]);
    let l = lift(&p);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("v1 = v0;"), "{b}");
    assert!(b.contains("v2 = asuint(asfloat(v0) + asfloat(v1));"), "{b}");
    assert!(b.contains("sir_mul_legacy(1.0f, asfloat(v2))"), "{b}");
    assert!(b.contains("mad(asfloat(v0), asfloat(v1), asfloat(1073741824u))"), "{b}");
    assert!(b.contains("saturate((mad(abs(asfloat(v0)), (-asfloat(v1)), asfloat(v2))) * 2.0f)"), "{b}");
    assert!(b.contains("v7 = sir_pkrtz(asfloat(v5), asfloat(v6));"), "{b}");
    assert!(!l.exec_narrowed);
    assert_eq!(l.vgprs.iter().copied().collect::<Vec<_>>(), (0..=7).collect::<Vec<u16>>());
}

#[test]
fn scalar_ops_set_scc_and_select() {
    let p = program(&[
        sop1("s_mov_b32", 4, 128 + 7),         // s4 = 7
        sop2("s_add_u32", 5, 4, 193),          // s5 = s4 + 0xffffffff, carry
        sopc("s_cmp_lt_u32", 5, 4),            // scc = s5 < s4
        sop2("s_cselect_b32", 6, 128 + 1, 128 + 2),
        vec![sop1("s_mov_b32", 7, LIT)[0], 0x1234],
    ]);
    let l = lift(&p);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("s4 = 7u;"), "{b}");
    assert!(b.contains("uint r = s4 + 4294967295u;") && b.contains("scc = (s4 + 4294967295u) < s4;"), "{b}");
    assert!(b.contains("scc = s5 < s4;"), "{b}");
    assert!(b.contains("uint r = scc ? 1u : 2u;"), "{b}");
    assert!(b.contains("s7 = 4660u;"), "{b}");
}

#[test]
fn compares_are_per_lane_masks_and_cmpx_predicates_later_writes() {
    let p = program(&[
        vopc("v_cmp_gt_f32", V + 0, 1),         // vcc = v0 > v1
        vop2("v_cndmask_b32", 2, V + 0, 1),     // v2 = vcc ? v1 : v0
        vopc("v_cmpx_lt_u32", 128 + 3, 2),      // ex = 3 < v2
        vop1("v_mov_b32", 3, 242),              // predicated
    ]);
    let l = lift(&p);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("vcc = (asfloat(v0) > asfloat(v1));"), "{b}");
    assert!(b.contains("v2 = vcc ? v1 : v0;"), "{b}");
    assert!(b.contains("ex = (3u < v2);"), "{b}");
    assert!(b.contains("if (ex) v3 = asuint(1.0f);"), "{b}");
    assert!(l.exec_narrowed);
}

#[test]
fn what_is_not_built_yet_is_named_not_guessed() {
    // (a buffer load from s[0:3] is no example any more: user data is a V# held as data, read through its window)
    let p = program(&[vop1("v_mov_b32", 1, V + 0), vop1("v_movrels_b32", 2, V + 0)]);
    let l = lift(&p);
    assert_eq!(l.unsupported.len(), 1);
    assert_eq!(l.unsupported[0].1, "v_movrels_b32");
    assert!(l.body.contains("// not translated: v_movrels_b32"));
}

fn dxc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("REBUILD_DXC") {
        return Some(p.into());
    }
    let root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut v: Vec<PathBuf> = std::fs::read_dir(root).ok()?.filter_map(|e| e.ok().map(|e| e.path().join("x64").join("dxc.exe"))).filter(|p| p.exists()).collect();
    v.sort();
    v.pop()
}

/// Every translated form, in one compute program, compiles with DXC (SM 6.6, wave32, 16-bit types).
#[test]
fn every_translated_form_compiles_with_dxc() {
    let Some(dxc) = dxc() else {
        eprintln!("skipped: no dxc.exe");
        return;
    };
    let parts = vec![
        vop1("v_mov_b32", 1, V + 0),
        vop2("v_add_f32", 2, V + 0, 1),
        vop2("v_mul_legacy_f32", 3, 242, 2),
        vec![vop2("v_madak_f32", 4, V + 0, 1)[0], 0x4000_0000],
        vop3("v_fma_f32", 5, [V + 0, V + 1, V + 2], 1, 2, 1, 1),
        vop3("v_med3_f32", 6, [V + 0, V + 1, V + 2], 0, 0, 0, 0),
        vop3("v_lshl_add_u32", 7, [V + 0, 128 + 2, V + 1], 0, 0, 0, 0),
        vop3("v_bfe_u32", 8, [V + 0, 128 + 4, 128 + 8], 0, 0, 0, 0),
        vop3("v_alignbit_b32", 9, [V + 0, V + 1, 128 + 8], 0, 0, 0, 0),
        vop1("v_cvt_i32_f32", 10, V + 3),
        vop1("v_cvt_f32_u32", 11, V + 10),
        vop1("v_sin_f32", 12, V + 11),
        vop1("v_fract_f32", 13, V + 12),
        vop2("v_cvt_pkrtz_f16_f32", 14, V + 5, 6),
        vop2("v_lshlrev_b32", 15, 128 + 3, 7),
        vop3("v_mul_hi_u32", 16, [V + 1, V + 2, 0], 0, 0, 0, 0),
        sop1("s_mov_b32", 4, 128 + 7),
        sop2("s_add_u32", 5, 4, 193),
        sopc("s_cmp_lt_u32", 5, 4),
        sop2("s_cselect_b32", 6, 128 + 1, 128 + 2),
        sop2("s_bfe_u32", 7, 6, LIT),
        vec![0x0010_0004],
        vopc("v_cmp_gt_f32", V + 0, 1),
        vop2("v_cndmask_b32", 17, V + 0, 1),
        vopc("v_cmpx_lt_u32", 128 + 3, 2),
        vop1("v_mov_b32", 18, 4),
    ];
    let l = lift(&program(&parts));
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    compile_cs(&dxc, &l, "lift");
}

/// Wraps a lifted body in a compute program (registers loaded from and stored to a buffer) and compiles it.
fn compile_cs(dxc: &std::path::Path, l: &Lifted, name: &str) {
    let mut src = String::from(PRELUDE);
    src.push_str("RWStructuredBuffer<uint> io : register(u0);\n[WaveSize(32)] [numthreads(32, 1, 1)]\nvoid main(uint t : SV_DispatchThreadID) {\n");
    src.push_str(&declarations(l));
    for r in &l.sgprs {
        src.push_str(&format!("    s{r} = io[{r}];\n"));
    }
    for r in &l.vgprs {
        src.push_str(&format!("    v{r} = io[1024 + t * 256 + {r}];\n"));
    }
    src.push_str(&l.body);
    for r in &l.vgprs {
        src.push_str(&format!("    io[1024 + t * 256 + {r}] = v{r};\n"));
    }
    src.push_str("}\n");
    let dir = std::env::temp_dir().join("rebuild-sir-lift");
    std::fs::create_dir_all(&dir).unwrap();
    let (hl, out) = (dir.join(format!("{name}.hlsl")), dir.join(format!("{name}.dxil")));
    std::fs::write(&hl, &src).unwrap();
    let r = Command::new(dxc)
        .args(["-T", "cs_6_6", "-E", "main", "-O3", "-enable-16bit-types", "-Fo"])
        .arg(&out)
        .arg(&hl)
        .output()
        .unwrap();
    assert!(r.status.success(), "dxc failed:\n{}\n{src}", String::from_utf8_lossy(&r.stderr));
}

#[path = "tests_cfg.rs"]
mod cfg_tests;

#[path = "tests_cov.rs"]
mod cov_tests;
