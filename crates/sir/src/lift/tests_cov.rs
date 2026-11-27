//! Coverage forms (lane `shadercov`): bit scans and byte permutes, SDWA, image atomics, window addressing
//! through code-built descriptors, typed fetches with the format in the instruction, descriptor words served
//! from the shadow table. Each is checked by its lifted text and compiled with DXC.

use super::*;
use rebuild_shader::gfx10::enc::{self as e, Layout};
use rebuild_shader::native::{HwStage, ResourceType, RootParam, RootType, UsedResource};

/// One instruction from named fields of a layout (its tag bits set); `op` is split into `op`/`op_hi` as the
/// layout has them.
fn build(layout: &Layout, enc: Encoding, name: &str, fields: &[(&str, u32)]) -> Vec<u32> {
    let mut w = vec![0u32; layout.words as usize];
    let mut put = |word: u8, lo: u8, width: u8, v: u32| {
        let m = (((1u64 << width) - 1) << lo) as u32;
        w[word as usize] = (w[word as usize] & !m) | ((v << lo) & m);
    };
    for &(word, lo, width, v) in layout.tag {
        put(word, lo, width, v);
    }
    let o = op(enc, name);
    let field = |n: &str| layout.fields.iter().find(|f| f.name == n).copied();
    let opf = field("op").unwrap();
    put(opf.word, opf.lo, opf.width, o & ((1 << opf.width) - 1));
    if let Some(h) = field("op_hi") {
        put(h.word, h.lo, h.width, o >> opf.width);
    }
    for (n, v) in fields {
        let f = field(n).unwrap_or_else(|| panic!("no field {n} in {}", layout.name));
        put(f.word, f.lo, f.width, *v);
    }
    w
}

/// An SDWA word from named fields.
fn sdwa_word(layout: &Layout, fields: &[(&str, u32)]) -> u32 {
    let mut w = 0u32;
    for (n, v) in fields {
        let f = layout.fields.iter().find(|f| f.name == *n).unwrap();
        w |= v << f.lo;
    }
    w
}

/// A compute program around the lifted body (registers from and to a buffer, the shadow table when read),
/// compiled with DXC.
fn compile(l: &Lifted, name: &str) {
    let Some(dxc) = dxc() else {
        eprintln!("skipped: no dxc.exe");
        return;
    };
    let mut src = String::from(PRELUDE);
    if l.uses_shadow {
        src.push_str(&format!("ByteAddressBuffer sir_shadow : register(t0, space{SHADOW_SPACE});\n"));
    }
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
    for r in &l.sgprs {
        src.push_str(&format!("    io[512 + {r}] = s{r};\n"));
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

/// A compute program's bindings: one descriptor table at user data 0 (s0) holding a UAV and an SRV.
fn table() -> (Vec<RootParam>, Vec<UsedResource>) {
    let layout = vec![RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 0, slot: 0 }];
    let used = [ResourceType::Uav, ResourceType::Srv]
        .iter()
        .map(|&kind| UsedResource { kind, root: RootType::Table, table_offset_dwords: 0, lower_bound: 0, stride: 0, dwords: 8, user_data: 0, slot: 0 })
        .collect();
    (layout, used)
}

fn smem(name: &str, sdata: u32, sbase: u32, offset: u32) -> Vec<u32> {
    build(&e::SMEM, Encoding::Smem, name, &[("sbase", sbase / 2), ("sdata", sdata), ("offset", offset), ("soffset", 125)])
}

fn lifted(parts: &[Vec<u32>], layout: &[RootParam], used: &[UsedResource]) -> Lifted {
    let bind = Bindings { hw: HwStage::Cs, layout, used };
    lift_stage(&program(parts), Stage::None, Some(bind))
}

#[test]
fn bit_scans_and_byte_permutes() {
    let p = program(&[
        vop1("v_ffbl_b32", 1, V + 0),
        vop1("v_ffbh_u32", 2, V + 0),
        vop1("v_ffbh_i32", 3, V + 0),
        vop1("v_bfrev_b32", 4, V + 0),
        vop1("v_not_b32", 5, V + 0),
        vop3("v_perm_b32", 6, [V + 0, V + 1, LIT], 0, 0, 0, 0),
        vec![0x0c0d_0800],
    ]);
    let l = lift(&p);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("v1 = firstbitlow(v0);"), "{b}");
    assert!(b.contains("v2 = (v0 == 0u ? 0xffffffffu : 31u - firstbithigh(v0));"), "{b}");
    assert!(b.contains("31u - firstbithigh((int)v0)"), "{b}");
    assert!(b.contains("v4 = reversebits(v0);") && b.contains("v5 = ~v0;"), "{b}");
    assert!(b.contains("v6 = sir_perm(v0, v1, 202180608u);"), "{b}");
    compile(&l, "cov_bits");
}

#[test]
fn sdwa_selects_operands_and_merges_the_destination() {
    let s = &e::SDWA;
    // v3.word1 = (v1.byte1 & sext(v2.word1)), the other half of v3 kept
    let and = vec![
        vop2("v_and_b32", 3, e::SRC_SDWA, 2)[0],
        sdwa_word(s, &[("src0", 1), ("src0_sel", 1), ("src1_sel", 5), ("src1_sext", 1), ("dst_sel", 5), ("dst_unused", 2)]),
    ];
    // v4 = float(v0.word1), bytes 1-3 zero (dst byte 0, padded)
    let cvt = vec![vop1("v_cvt_f32_u32", 4, e::SRC_SDWA)[0], sdwa_word(s, &[("src0", 0), ("src0_sel", 5), ("dst_sel", 0), ("dst_unused", 0)])];
    // v5 = s6 + v1 (src0 an SGPR), whole dword
    let add = vec![vop2("v_add_nc_u32", 5, e::SRC_SDWA, 1)[0], sdwa_word(s, &[("src0", 6), ("s0", 1), ("src0_sel", 6), ("src1_sel", 6), ("dst_sel", 6)])];
    // s[10:11] = v1.byte0 < v2.byte2 (VOPC, a scalar destination)
    let cmp = vec![
        vopc("v_cmp_lt_u32", e::SRC_SDWA, 2)[0],
        sdwa_word(&e::SDWAB, &[("src0", 1), ("src0_sel", 0), ("src1_sel", 2), ("sdst", 10), ("sd", 1)]),
    ];
    // v7 = sign-extended byte 2 of the result
    let sext = vec![vop1("v_mov_b32", 7, e::SRC_SDWA)[0], sdwa_word(s, &[("src0", 0), ("src0_sel", 6), ("dst_sel", 2), ("dst_unused", 1)])];
    let l = lift(&program(&[and, cvt, add, cmp, sext, vop2("v_cndmask_b32", 8, V + 1, 2)]));
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("{ uint sir_a = ((v1 >> 8u) & 0xffu), sir_b = (uint)(((int)v2 << 0) >> 16), sir_r = 0u;"), "{b}");
    assert!(b.contains("sir_r = sir_a & sir_b;") && b.contains("v3 = ((v3 & 65535u) | ((sir_r & 65535u) << 16u));"), "{b}");
    assert!(b.contains("sir_a = ((v0 >> 16u) & 0xffffu)") && b.contains("sir_r = asuint((float)sir_a);") && b.contains("v4 = ((sir_r & 255u) << 0u);"), "{b}");
    assert!(b.contains("sir_a = s6, sir_b = v1") && b.contains("v5 = sir_r;"), "{b}");
    assert!(b.contains("m10 = (sir_a < sir_b);"), "{b}");
    assert!(b.contains("v7 = (((sir_r & 255u) << 16u) | (((sir_r >> 7u) & 1u) != 0u ? 4278190080u : 0u));"), "{b}");
    compile(&l, "cov_sdwa");
}

#[test]
fn sdwa_float_operands_take_abs_and_neg_and_the_result_clamps() {
    let w = sdwa_word(&e::SDWA, &[("src0", 1), ("src0_sel", 6), ("src0_neg", 1), ("src1_sel", 6), ("src1_abs", 1), ("dst_sel", 6), ("clamp", 1)]);
    let l = lift(&program(&[vec![vop2("v_mul_f32", 3, e::SRC_SDWA, 2)[0], w]]));
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    assert!(l.body.contains("sir_r = asuint(saturate((-asfloat(sir_a)) * abs(asfloat(sir_b))));"), "{}", l.body);
    // abs/neg on an integer op is refused, not ignored
    let w = sdwa_word(&e::SDWA, &[("src0", 1), ("src0_sel", 6), ("src0_neg", 1), ("src1_sel", 6), ("dst_sel", 6)]);
    let l = lift(&program(&[vec![vop2("v_and_b32", 3, e::SRC_SDWA, 2)[0], w]]));
    assert_eq!(l.unsupported.len(), 1);
    assert!(l.unsupported[0].1.contains("SDWA abs/neg"), "{:?}", l.unsupported);
}

#[test]
fn image_atomics_are_interlocked_on_32_and_64_bit_texels() {
    let (layout, used) = table();
    let img = |name: &str, dmask: u32, glc: u32, vdata: u32| {
        build(&e::MIMG, Encoding::Mimg, name, &[("dim", 1), ("dmask", dmask), ("glc", glc), ("vaddr", 0), ("vdata", vdata), ("srsrc", 2)])
    };
    let l = lifted(
        &[
            smem("s_load_dwordx8", 8, 0, 0),
            img("image_atomic_umin", 1, 1, 4),
            img("image_atomic_umax", 3, 0, 6),
            img("image_atomic_smax", 1, 0, 8),
            img("image_atomic_cmpswap", 3, 1, 10),
            img("image_atomic_sub", 1, 0, 12),
        ],
        &layout,
        &used,
    );
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("RWTexture2D<uint> t = ResourceDescriptorHeap[s8]; { uint o; InterlockedMin(t[uint2((uint)v0, (uint)v1)], v4, o); v4 = (uint)o; }"), "{b}");
    assert!(b.contains("RWTexture2D<uint64_t> t = ResourceDescriptorHeap[s8]; { uint64_t o; InterlockedMax(t[uint2((uint)v0, (uint)v1)], (uint64_t)(((uint64_t)v7 << 32) | (uint64_t)v6)); }"), "{b}");
    assert!(b.contains("RWTexture2D<int> t") && b.contains("InterlockedMax(t[uint2((uint)v0, (uint)v1)], (int)v8);"), "{b}");
    assert!(b.contains("InterlockedCompareExchange(t[uint2((uint)v0, (uint)v1)], v11, v10, o); v10 = (uint)o;"), "{b}");
    assert!(b.contains("InterlockedAdd(t[uint2((uint)v0, (uint)v1)], (uint)0 - v12);"), "{b}");
    compile(&l, "cov_image_atomics");
    // wrapping increment has no host equivalent: refused with its name
    let l = lifted(&[smem("s_load_dwordx8", 8, 0, 0), img("image_atomic_inc", 1, 0, 4)], &layout, &used);
    assert!(l.unsupported[0].1.contains("image atomic inc"), "{:?}", l.unsupported);
}

#[test]
fn a_descriptor_built_from_an_address_is_read_through_its_window() {
    // s[4:7] = { address lo (user data), address hi | stride 16 << 16, records, word 3 }, as vertex shaders build it
    let parts = [
        sop1("s_mov_b32", 4, 0),
        vec![sop2("s_or_b32", 5, 1, LIT)[0], 16 << 16],
        sop1("s_mov_b32", 6, 193),
        vec![sop1("s_mov_b32", 7, LIT)[0], 0x3 << 28 | 0xFAC],
        smem("s_buffer_load_dwordx2", 20, 4, 0x10),
        // buffer_load_dwordx2 v[4:5], v0 (index), s[4:7], offset 8
        build(&e::MUBUF, Encoding::Mubuf, "buffer_load_dwordx2", &[("idxen", 1), ("offset", 8), ("vaddr", 0), ("vdata", 4), ("srsrc", 1), ("soffset", 128)]),
        // tbuffer_load_format_xyzw v[8:11], v0 (index), s[4:7], 8_8_8_8 unorm
        build(&e::MTBUF, Encoding::Mtbuf, "tbuffer_load_format_xyzw", &[("idxen", 1), ("format", 56), ("vaddr", 0), ("vdata", 8), ("srsrc", 1), ("soffset", 128)]),
        build(&e::MUBUF, Encoding::Mubuf, "buffer_store_dword", &[("offen", 1), ("vaddr", 1), ("vdata", 2), ("srsrc", 1), ("soffset", 128)]),
    ];
    let l = lifted(&parts, &[], &[]);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    let w = format!("ResourceDescriptorHeap[{WINDOW_HEAP_BASE}u + (s5 & 32767u)]");
    assert!(b.contains(&format!("{{ RWByteAddressBuffer w = {w}; uint a = (s4 + 16u) & ~3u;")), "{b}");
    assert!(b.contains("s20 = w.Load(a + 0u);") && b.contains("s21 = w.Load(a + 4u);"), "{b}");
    // word 3 is a known constant: OOB_SELECT 3 (no check) is folded
    assert!(b.contains("sir_i = v0, sir_o = 8u;") && b.contains("v4 = true ? w.Load(a + 0u) : 0u;"), "{b}");
    assert!(b.contains("uint sir_e[1] = { sir_unaligned(w.Load((a & ~3u) + 0u), w.Load((a & ~3u) + 4u), a) };"), "{b}");
    assert!(b.contains("asuint((float)((sir_e[0] >> 24u) & 255u) / 255.0f)"), "{b}");
    assert!(b.contains("v11 = sir_dst_sel(sir_c, (sir_ds >> 9u) & 7u, 0x3f800000u);"), "{b}");
    assert!(b.contains("if (true) w.Store(a + 0u, v2);"), "{b}");
    assert!(l.live_in.contains(&0) && l.live_in.contains(&1));
    compile(&l, "cov_window");
}

#[test]
fn a_descriptor_word_3_read_at_run_time_picks_the_range_check() {
    // word 3 loaded from memory: OOB_SELECT is read from it, per access
    let parts = [
        smem("s_load_dwordx4", 8, 0, 0),
        smem("s_buffer_load_dwordx4", 12, 8, 0),
        build(&e::MUBUF, Encoding::Mubuf, "buffer_load_dword", &[("offen", 1), ("vaddr", 0), ("vdata", 4), ("srsrc", 3), ("soffset", 128)]),
    ];
    let (layout, used) = table();
    let l = lifted(&parts, &layout, &used);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("uint sir_ob = (s15 >> 28) & 3u;"), "{b}");
    assert!(b.contains("(sir_ob == 3u || (sir_ob == 2u ? sir_o + 0u < sir_nr : (sir_ob == 1u ? sir_i < sir_nr : (sir_i < sir_nr && sir_o + 0u < sir_st))))"), "{b}");
    compile(&l, "cov_window_oob");
}

#[test]
fn a_buffer_descriptors_stride_and_records_come_from_the_shadow_table() {
    let (layout, used) = table();
    let parts = [
        smem("s_load_dwordx4", 8, 0, 0),
        sop2("s_lshr_b32", 20, 9, 128 + 16),
        vec![sop2("s_bfe_u32", 21, 9, LIT)[0], 16 | 14 << 16],
        sop2("s_lshl_b32", 22, 10, 128 + 2),
    ];
    let l = lifted(&parts, &layout, &used);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("(sir_shadow.Load(s8 * 16u) << 16) >> (16u & 31)"), "{b}");
    assert!(b.contains("sir_bfe_u((sir_shadow.Load(s8 * 16u) << 16), 917520u & 31"), "{b}");
    assert!(b.contains("sir_shadow.Load(s8 * 16u + 4u) << (2u & 31)"), "{b}");
    assert!(l.uses_shadow);
    compile(&l, "cov_shadow_words");
    // the address bits of word 1, and word 3, are not in the shadow table
    for p in [sop2("s_and_b32", 20, 9, LIT), vec![sop2("s_bfe_u32", 21, 11, LIT)[0], 12 | 4 << 16]] {
        let mut p = p;
        if p.len() == 1 {
            p.push(0xffff);
        }
        let l = lifted(&[smem("s_load_dwordx4", 8, 0, 0), p], &layout, &used);
        assert!(l.unsupported[0].1.contains("read as data"), "{:?}", l.unsupported);
    }
}

#[test]
fn a_typed_access_with_no_index_and_no_offset_is_element_0() {
    let (layout, used) = table();
    let parts = [
        smem("s_load_dwordx4", 8, 0, 0),
        build(&e::MUBUF, Encoding::Mubuf, "buffer_load_format_xyz", &[("vaddr", 0), ("vdata", 4), ("srsrc", 2), ("soffset", 128)]),
    ];
    let l = lifted(&parts, &layout, &used);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    assert!(l.body.contains("Buffer<float4> b = ResourceDescriptorHeap[s8]; float4 r = b[0u];"), "{}", l.body);
    compile(&l, "cov_typed0");
}

#[test]
fn sixteen_bit_ops_keep_the_other_half_and_packed_ops_take_each_half() {
    let p = program(&[
        vopc("v_cmp_lt_f16", V + 0, 1),
        vop2("v_add_f16", 2, 242, 1), // 1.0 (f16 0x3c00) + v1
        build(&e::VOP3A, Encoding::Vop3, "v_mul_f16", &[("vdst", 3), ("src0", V), ("src1", V + 1), ("opsel", 0b1001)]),
        vop1("v_rcp_f16", 4, V + 2),
        build(&e::VOP3P, Encoding::Vop3p, "v_pk_add_f16", &[("vdst", 5), ("src0", V), ("src1", 240), ("opsel_hi", 3), ("opsel_hi2", 1)]),
        build(&e::VOP3P, Encoding::Vop3p, "v_pk_mul_lo_u16", &[("vdst", 6), ("src0", V), ("src1", V + 1), ("opsel", 0b10), ("opsel_hi", 1)]),
        vop3("v_lshlrev_b64", 8, [V + 2, V, 0], 0, 0, 0, 0),
    ]);
    let l = lift(&p);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("vcc = (f16tof32(v0) < f16tof32(v1));"), "{b}");
    assert!(b.contains("v2 = (v2 & 0xffff0000u) | sir_f16(f16tof32(15360u) + f16tof32(v1));"), "{b}");
    assert!(b.contains("v3 = (v3 & 0xffffu) | (sir_f16(f16tof32((v0 >> 16)) * f16tof32(v1)) << 16);"), "{b}");
    // the inline 0.5 is f16 0x3800 in the low half: the high half reads 0
    assert!(b.contains("v5 = sir_f16(f16tof32(v0) + f16tof32(14336u)) | (sir_f16(f16tof32((v0 >> 16)) + f16tof32((14336u >> 16))) << 16);"), "{b}");
    assert!(b.contains("(((v0 & 0xffffu) * ((v1 >> 16) & 0xffffu)) & 0xffffu) | (((((v0 >> 16) & 0xffffu) * (v1 & 0xffffu)) & 0xffffu) << 16)"), "{b}");
    assert!(b.contains("{ uint64_t sir_s = (((uint64_t)v1 << 32) | (uint64_t)v0) << (v2 & 63u);"), "{b}");
    compile(&l, "cov_16bit");
}

#[test]
fn carries_64_bit_compares_and_byte_packing() {
    let p = program(&[
        build(&e::VOP3B, Encoding::Vop3, "v_add_co_u32", &[("vdst", 2), ("sdst", 10), ("src0", V), ("src1", V + 1)]),
        build(&e::VOP3B, Encoding::Vop3, "v_add_co_ci_u32", &[("vdst", 3), ("sdst", 106), ("src0", V), ("src1", 128), ("src2", 10)]),
        build(&e::VOP3A, Encoding::Vop3, "v_cmp_ne_u64", &[("vdst", 12), ("src0", V), ("src1", 193)]),
        vop3("v_cvt_pk_u8_f32", 4, [V + 0, 128 + 3, V + 1], 0, 0, 0, 0),
        vop3("v_mul_hi_i32", 5, [V + 0, V + 1, 0], 0, 0, 0, 0),
    ]);
    let l = lift(&p);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("{ uint64_t sir_s = (uint64_t)v0 + (uint64_t)v1 + (uint64_t)0u;") && b.contains("m10 = (sir_s >> 32) != 0u;"), "{b}");
    assert!(b.contains("(uint64_t)(m10 ? 1u : 0u);") && b.contains("vcc = (sir_s >> 32) != 0u;"), "{b}");
    assert!(b.contains("m12 = ((((uint64_t)v1 << 32) | (uint64_t)v0) != (((uint64_t)0xffffffffu << 32) | (uint64_t)4294967295u));"), "{b}");
    assert!(b.contains("(v1 & ~(0xffu << ((3u & 3u) * 8u))) | ((uint)clamp(round(asfloat(v0)), 0.0f, 255.0f) << ((3u & 3u) * 8u))"), "{b}");
    compile(&l, "cov_carry");
}

#[test]
fn scalar_bit_ops_carries_and_vcc_as_data() {
    let p = program(&[
        sop2("s_bfm_b32", 4, 128 + 5, 128 + 8),
        sop2("s_add_u32", 5, 0, 1),
        sop2("s_addc_u32", 6, 2, 3),
        sop1("s_bcnt1_i32_b32", 7, 0),
        sop1("s_flbit_i32_b32", 8, 0),
        vec![0xB000_0000 | op(Encoding::Sopk, "s_movk_i32") << 23 | 9 << 16 | 3],
        sop1("s_bitset1_b32", 9, 128 + 4),
        sop2("s_add_u32", 106, 0, 128 + 1), // data in VCC
        sop1("s_not_b32", 10, 106),
    ]);
    let l = lift(&p);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("s4 = ((1u << (5u & 31u)) - 1u) << (8u & 31u);"), "{b}");
    assert!(b.contains("{ uint64_t sir_s = (uint64_t)s2 + (uint64_t)s3 + (scc ? 1u : 0u);") && b.contains("scc = (sir_s >> 32) != 0;"), "{b}");
    assert!(b.contains("uint r = countbits(s0);") && b.contains("31u - firstbithigh(s0)"), "{b}");
    assert!(b.contains("s9 = s9 | (1u << (4u & 31u));"), "{b}");
    assert!(b.contains("s106 = r;") && b.contains("uint r0 = ~s106"), "{b}");
    compile(&l, "cov_salu");
    // VCC holding a lane mask, read as data: refused
    let l = lift(&program(&[vopc("v_cmp_lt_u32", V + 0, 1), sop1("s_not_b32", 10, 106)]));
    assert!(l.unsupported[0].1.contains("lane mask read as data"), "{:?}", l.unsupported);
}

#[test]
fn buffer_and_lds_atomics() {
    let (layout, used) = table();
    let atomic = |name: &str, glc: u32, vdata: u32, srsrc: u32| {
        build(&e::MUBUF, Encoding::Mubuf, name, &[("offen", 1), ("glc", glc), ("vaddr", 0), ("vdata", vdata), ("srsrc", srsrc), ("soffset", 128)])
    };
    let l = lifted(
        &[
            smem("s_load_dwordx4", 8, 0, 0),
            atomic("buffer_atomic_add", 1, 4, 2),
            atomic("buffer_atomic_smax", 0, 5, 2),
            atomic("buffer_atomic_cmpswap", 1, 6, 2),
            atomic("buffer_atomic_umin", 0, 8, 1), // through the window of s[4:7] (data)
        ],
        &layout,
        &used,
    );
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("RWByteAddressBuffer w = ResourceDescriptorHeap[s8];") && b.contains("w.InterlockedAdd(a & ~3u, v4, o);"), "{b}");
    assert!(b.contains("int o = 0; if (true) w.InterlockedMax(a & ~3u, (int)v5, o);"), "{b}");
    assert!(b.contains("w.InterlockedCompareExchange(a & ~3u, v7, v6, o);") && b.contains("v6 = (uint)o;"), "{b}");
    assert!(b.contains(&format!("ResourceDescriptorHeap[{WINDOW_HEAP_BASE}u + (s5 & 32767u)]")) && b.contains("w.InterlockedMin(a & ~3u, v8, o);"), "{b}");
    compile(&l, "cov_buffer_atomics");

    let io = rebuild_shader::native::ComputeIo { rsrc2: 0, threads: [64, 1, 1] };
    let ds = |name: &str| build(&e::DS, Encoding::Ds, name, &[("addr", 0), ("data0", 1), ("data1", 2), ("vdst", 3), ("offset0", 8)]);
    let p = program(&[ds("ds_max_u32"), ds("ds_or_rtn_b32"), ds("ds_cmpst_rtn_b32"), ds("ds_sub_u32")]);
    let l = lift_stage(&p, Stage::Cs(io), None);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("InterlockedMax(sir_lds[(v0 + 8u) >> 2], v1, o);"), "{b}");
    assert!(b.contains("InterlockedOr(sir_lds[(v0 + 8u) >> 2], v1, o);") && b.contains("v3 = (uint)o;"), "{b}");
    assert!(b.contains("InterlockedCompareExchange(sir_lds[(v0 + 8u) >> 2], v1, v2, o);"), "{b}");
    assert!(b.contains("InterlockedAdd(sir_lds[(v0 + 8u) >> 2], 0u - v1, o);"), "{b}");
    // signed LDS min/max would compile unsigned on the uint array: refused
    let l = lift_stage(&program(&[ds("ds_max_i32")]), Stage::Cs(io), None);
    assert_eq!(l.unsupported.len(), 1);
}
