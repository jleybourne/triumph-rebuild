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
    for k in 0..l.immediate_samplers.len() {
        src.push_str(&format!("SamplerState sir_imm{k} : register(s{k}, space{IMMEDIATE_SAMPLER_SPACE});\n"));
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
    assert!(b.contains("RWTexture2D<uint> t = ResourceDescriptorHeap[sir_vi(s8, 17u)]; { uint o; InterlockedMin(t[uint2((uint)v0, (uint)v1)], v4, o); v4 = (uint)o; }"), "{b}");
    assert!(b.contains("RWTexture2D<uint64_t> t = ResourceDescriptorHeap[sir_vi(s8, 17u)]; { uint64_t o; InterlockedMax(t[uint2((uint)v0, (uint)v1)], (uint64_t)(((uint64_t)v7 << 32) | (uint64_t)v6)); }"), "{b}");
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
    let w = "ResourceDescriptorHeap[sir_wi(sir_b, s5)]";
    assert!(b.contains(&format!("{{ uint sir_b = s4 + 16u; ByteAddressBuffer w = {w}; uint a = sir_wo(sir_b) & ~3u;")), "{b}");
    // the store through the same V#: the sub-window's raw UAV
    assert!(b.contains("RWByteAddressBuffer w = ResourceDescriptorHeap[sir_wu(sir_b, s5)]"), "{b}");
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

/// A descriptor from a table is indexed by the table's offset (a host one: the runtime maps it when it writes the
/// user data); one from a dynamic-resource heap base by an index from the title's own data, mapped through the
/// page table in the shadow buffer.
#[test]
fn a_dynamic_heap_index_goes_through_the_page_table_and_a_table_offset_does_not() {
    use crate::lift::{PAGE_TABLE_BYTE, PAGE_TABLE_PAGES, VIEW_PAGE_SHIFT};
    let (layout, used) = table();
    let l = lifted(&[smem("s_load_dwordx4", 8, 0, 64)], &layout, &used);
    assert!(l.body.contains("s8 = (uint)((int)s0 + 64) / 32u;"), "{}", l.body);
    assert!(!l.body.contains(&format!("{PAGE_TABLE_BYTE}u")));
    let heap = vec![RootParam { root: RootType::ResourceHeap, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 0, slot: 0 }];
    let l = lifted(&[smem("s_load_dwordx4", 8, 0, 64)], &heap, &[]);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let want = format!(
        "s8 = sir_shadow.Load({PAGE_TABLE_BYTE}u + (min((((uint)((int)s0 + 64) / 32u)) >> {VIEW_PAGE_SHIFT}u, {}u) << 2u)) + ((((uint)((int)s0 + 64) / 32u)) & {}u);",
        PAGE_TABLE_PAGES - 1,
        (1 << VIEW_PAGE_SHIFT) - 1
    );
    assert!(l.body.contains(&want), "{}", l.body);
    assert!(l.uses_shadow);
    compile(&l, "cov_page_table");
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
    assert!(b.contains("(sir_shadow.Load(sir_sh(s8)) << 16) >> (16u & 31)"), "{b}");
    assert!(b.contains("sir_bfe_u((sir_shadow.Load(sir_sh(s8)) << 16), 917520u & 31"), "{b}");
    assert!(b.contains("sir_shadow.Load(sir_sh(s8) + 4u) << (2u & 31)"), "{b}");
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
    // the typed view is the shadow entry's alternate
    assert!(l.body.contains("Buffer<float4> b = ResourceDescriptorHeap[sir_va(sir_shadow.Load(sir_sh(s8) + 8u), 2u)]; float4 r = b[0u];"), "{}", l.body);
    compile(&l, "cov_typed0");
}

#[test]
fn data_embedded_in_the_code_is_read_from_the_programs_own_words() {
    // s[10:11] = pc; s10 += 8 (two words on, the table); s12 = index * 4; s14 = s10 + s12; s_load_dwordx2 from s[14:15]
    let parts = [
        sop1("s_getpc_b64", 10, 0),
        sop2("s_add_u32", 10, 10, 128 + 16),
        sop2("s_lshl_b32", 12, 2, 128 + 2),
        sop2("s_add_u32", 14, 10, 12),
        smem("s_load_dwordx2", 16, 14, 4),
        vec![0xBF81_0000], // s_endpgm, then the table
        vec![0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444],
    ];
    let bytes: Vec<u8> = parts.iter().flatten().flat_map(|w| w.to_le_bytes()).collect();
    let insts = rebuild_shader::gfx10::decode(&bytes).unwrap();
    let bind = Bindings { hw: HwStage::Cs, layout: &[], used: &[] };
    // without the bytes (only the decoded, reachable instructions) the load is refused, not read as zeros
    assert_eq!(lift_program(&insts, Stage::None, Some(bind), true).unsupported.len(), 1);
    let l = lift_program_code(&insts, &bytes, Stage::None, Some(bind), true);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("s10 = 4u;"), "{b}");
    assert!(b.contains("s16 = sir_code[min(((uint)((int)s14 + 4) >> 2) + 0u,"), "{b}");
    assert!(l.code.contains(&0x3333_3333), "{:?}", l.code);
}

#[test]
fn a_wave32_mask_survives_a_mask_written_to_the_next_sgpr() {
    // wave32: masks in s1 and s2 are separate registers, not a pair
    let p = program(&[
        vop3("v_cmp_lt_f32", 1, [V + 0, V + 1, 0], 0, 0, 0, 0),
        vop3("v_cmp_gt_f32", 2, [V + 0, V + 1, 0], 0, 0, 0, 0),
        sop2("s_or_b32", 3, 1, 2),
    ]);
    let l = lift_program(&p, Stage::None, None, false);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    assert!(l.body.contains("m3 = m1 || m2;"), "{}", l.body);
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
    assert!(b.contains("uint4 sh = sir_shadow.Load4(sir_sh(s8)); RWByteAddressBuffer w = ResourceDescriptorHeap[sir_va(sh.z, 1u)];"), "{b}");
    assert!(b.contains("uint a = sh.w + sir_i * 0u + sir_o + 0u;") && b.contains("w.InterlockedAdd(a & ~3u, v4, o);"), "{b}");
    assert!(b.contains("int o = 0; if (true) w.InterlockedMax(a & ~3u, (int)v5, o);"), "{b}");
    assert!(b.contains("w.InterlockedCompareExchange(a & ~3u, v7, v6, o);") && b.contains("v6 = (uint)o;"), "{b}");
    assert!(b.contains("RWByteAddressBuffer w = ResourceDescriptorHeap[sir_wu(sir_b, s5)]") && b.contains("w.InterlockedMin(a & ~3u, v8, o);"), "{b}");
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

#[test]
fn global_memory_small_lds_access_and_image_forms() {
    let (layout, used) = table();
    let flat = |name: &str, fields: &[(&str, u32)]| build(&e::FLAT, Encoding::Global, name, fields);
    let img = |name: &str, fields: &[(&str, u32)]| {
        let mut f = vec![("dim", 1), ("srsrc", 2), ("ssamp", 4)];
        f.extend_from_slice(fields);
        build(&e::MIMG, Encoding::Mimg, name, &f)
    };
    let l = lifted(
        &[
            smem("s_load_dwordx8", 8, 0, 0),
            // an immediate sampler in s[16:19]
            sop1("s_mov_b32", 16, 128),
            sop1("s_mov_b32", 17, 128),
            sop1("s_mov_b32", 18, 128),
            sop1("s_mov_b32", 19, 128),
            // global_load_dwordx2 v[4:5], v0, s[2:3] offset -4; global_store_dword v[0:1] (64-bit address), v6
            flat("global_load_dwordx2", &[("seg", 2), ("addr", 0), ("saddr", 2), ("vdst", 4), ("offset", 0xffc)]),
            flat("global_store_dword", &[("seg", 2), ("addr", 0), ("saddr", 0x7d), ("data", 6)]),
            img("image_sample", &[("dmask", 0b0111), ("d16", 1), ("vaddr", 0), ("vdata", 10)]),
            img("image_gather4_lz_o", &[("dmask", 1), ("vaddr", 0), ("vdata", 12)]),
            img("image_load_mip", &[("dmask", 1), ("vaddr", 0), ("vdata", 14)]),
        ],
        &layout,
        &used,
    );
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("uint sir_b = s2; ByteAddressBuffer w = ResourceDescriptorHeap[sir_wi(sir_b, s3)]; uint a = (sir_wo(sir_b) + v0 + (uint)-4) & ~3u;"), "{b}");
    assert!(b.contains("RWByteAddressBuffer w = ResourceDescriptorHeap[NonUniformResourceIndex(sir_wu(sir_b, v1))]") && b.contains("w.Store(a + 0u, v6);"), "{b}");
    assert!(b.contains("v10 = sir_f16(r.x) | (sir_f16(r.y) << 16);") && b.contains("v11 = sir_f16(r.z) | 0u;"), "{b}");
    assert!(b.contains("t.GatherRed(sir_imm0, float2(asfloat(v1), asfloat(v2)), int2(((int)v0 << 26) >> 26, ((int)v0 << 18) >> 26));"), "{b}");
    assert!(b.contains("t.Load(int3((int)v0, (int)v1, (int)v2))"), "{b}");
    compile(&l, "cov_global_image");

    let io = rebuild_shader::native::ComputeIo { rsrc2: 0, threads: [64, 1, 1] };
    let ds = |name: &str| build(&e::DS, Encoding::Ds, name, &[("addr", 0), ("data0", 1), ("vdst", 3), ("offset0", 2)]);
    let l = lift_stage(&program(&[ds("ds_read_i16"), ds("ds_read_u8"), ds("ds_write_b16")]), Stage::Cs(io), None);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("v3 = (uint)(((int)(sir_lds[(v0 + 2u) >> 2] >> (((v0 + 2u) & 3u) * 8u)) << 16) >> 16);"), "{b}");
    assert!(b.contains("InterlockedAnd(sir_lds[sir_b >> 2], ~(65535u << sir_sh)); InterlockedOr(sir_lds[sir_b >> 2], (v1 & 65535u) << sir_sh);"), "{b}");
}

#[test]
fn buffer_views_are_the_heaps_alternates_and_raw_reads_apply_the_lead() {
    // d3d12x: the console index holds a raw SRV from the 16-byte aligned address `lead` bytes below the buffer;
    // the CBV, UAV and typed views are at the shadow entry's alternate (`(stride, records, alternate, lead)`)
    let layout = vec![RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 0, slot: 0 }];
    let cbv = vec![UsedResource { kind: ResourceType::Cbv, root: RootType::Table, table_offset_dwords: 0, lower_bound: 0, stride: 0, dwords: 4, user_data: 0, slot: 0 }];
    // a CBV range of a table: read as the raw SRV at the console index all the same (never a CBV from the heap)
    let l = lifted(&[smem("s_load_dwordx4", 8, 0, 0), smem("s_buffer_load_dwordx2", 12, 8, 16)], &layout, &cbv);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    assert!(l.body.contains("ByteAddressBuffer c = ResourceDescriptorHeap[sir_vi(s8, 0u)]; uint sir_ld = sir_shadow.Load(sir_sh(s8) + 12u);"), "{}", l.body);
    assert!(l.body.contains("s12 = c.Load(sir_ld + 16u);") && l.body.contains("s13 = c.Load(sir_ld + 20u);"), "{}", l.body);
    assert!(!l.body.contains("ConstantBuffer") && !l.uses_cb, "{}", l.body);
    compile(&l, "cov_alt_cbv");

    let (layout, used) = table();
    let mubuf = |name: &str, vdata: u32| build(&e::MUBUF, Encoding::Mubuf, name, &[("offen", 1), ("vaddr", 0), ("vdata", vdata), ("srsrc", 2), ("soffset", 128)]);
    let l = lifted(
        &[
            smem("s_load_dwordx4", 8, 0, 0),
            smem("s_buffer_load_dword", 12, 8, 4),
            mubuf("buffer_load_dwordx2", 4),
            mubuf("buffer_store_dword", 6),
            build(&e::MTBUF, Encoding::Mtbuf, "tbuffer_load_format_x", &[("offen", 1), ("format", 22), ("vaddr", 0), ("vdata", 7), ("srsrc", 2), ("soffset", 128)]),
        ],
        &layout,
        &used,
    );
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(b.contains("ByteAddressBuffer c = ResourceDescriptorHeap[sir_vi(s8, 0u)]; uint sir_ld = sir_shadow.Load(sir_sh(s8) + 12u);") && b.contains("s12 = c.Load(sir_ld + 4u);"), "{b}");
    assert!(b.contains("ByteAddressBuffer b = ResourceDescriptorHeap[sir_vi(s8, 0u)]; uint4 sh = sir_shadow.Load4(sir_sh(s8)); uint2 r = b.Load2(sh.w + v0 + 0u + 0u);"), "{b}");
    assert!(b.contains("uint4 sh = sir_shadow.Load4(sir_sh(s8)); RWByteAddressBuffer b = ResourceDescriptorHeap[sir_va(sh.z, 1u)]; if (true) b.Store(sh.w + v0 + 0u + 0u, v6);"), "{b}");
    assert!(b.contains("uint a = sh.w + 0u + sir_i * sir_st + sir_o;"), "{b}");
    compile(&l, "cov_alt_raw");
}

#[test]
fn heap_buffer_loads_are_never_cbvs() {
    // GBV on WARP: "Descriptor Type in Heap: SRV, Register Type: CBV". Console descriptors are untyped, so a slot
    // the title's root signature calls a CBV may hold a buffer SRV: every heap-sourced buffer load is a raw read,
    // with a register offset and through MUBUF alike
    let layout = vec![RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 0, slot: 0 }];
    let cbv = vec![UsedResource { kind: ResourceType::Cbv, root: RootType::Table, table_offset_dwords: 0, lower_bound: 0, stride: 0, dwords: 4, user_data: 0, slot: 0 }];
    let reg = build(&e::SMEM, Encoding::Smem, "s_buffer_load_dwordx4", &[("sbase", 4), ("sdata", 12), ("offset", 8), ("soffset", 20)]);
    let mubuf = build(&e::MUBUF, Encoding::Mubuf, "buffer_load_dwordx2", &[("offen", 1), ("vaddr", 0), ("vdata", 4), ("srsrc", 2), ("soffset", 128)]);
    let l = lifted(&[sop1("s_mov_b32", 20, 160), smem("s_load_dwordx4", 8, 0, 0), smem("s_buffer_load_dword", 16, 8, 4), reg, mubuf], &layout, &cbv);
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    assert!(!b.contains("ConstantBuffer") && !l.uses_cb, "{b}");
    assert!(l.desc_uses.iter().all(|u| u.view != res::View::Cbv), "{:?}", l.desc_uses);
    assert!(b.contains("s16 = c.Load(sir_ld + 4u);"), "{b}");
    assert!(b.contains("uint b = (s20 + 8u) >> 2;") && b.contains("s15 = c.Load(sir_ld + ((b + 3u) << 2));"), "{b}");
    assert!(b.contains("ByteAddressBuffer b = ResourceDescriptorHeap[sir_vi(s8, 0u)]; uint4 sh = sir_shadow.Load4(sir_sh(s8)); uint2 r = b.Load2(sh.w + v0 + 0u + 0u);"), "{b}");
    assert!(!b.contains("NonUniformResourceIndex"), "uniform heap indices stay uniform: {b}");
    compile(&l, "cov_never_cbv");
}

/// The title's heap is larger than the host's: a console index past `CONSOLE_VIEWS` must never reach the host
/// heap's alternate or window ranges (another view type there). Every heap read goes through `sir_vi` (console
/// index) or `sir_va` (alternate), and every shadow read through `sir_sh`.
#[test]
fn console_view_indices_never_reach_the_alternates() {
    use crate::lift::{NullKind, CONSOLE_VIEWS, NULL_BANK, WINDOW_HEAP_BASE};
    // the null bank: one slot per kind, inside the bank, below the windows
    let slots: Vec<u32> = NullKind::all().iter().map(|k| k.slot()).collect();
    let mut sorted = slots.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), slots.len(), "one slot per kind");
    assert!(slots.iter().all(|&s| s < NULL_BANK));
    assert!(CONSOLE_VIEWS + NULL_BANK < WINDOW_HEAP_BASE);
    // the prelude's literals are the constants
    assert!(PRELUDE.contains(&format!("i < {CONSOLE_VIEWS}u ? i : {CONSOLE_VIEWS}u + k")));
    assert!(PRELUDE.contains(&format!("z >= {CONSOLE_VIEWS}u && z < {}u", CONSOLE_VIEWS + NULL_BANK)));
    assert!(PRELUDE.contains(&format!("min(i, {CONSOLE_VIEWS}u) * 16u")));

    let (layout, used) = table();
    let mubuf = |name: &str, vdata: u32| build(&e::MUBUF, Encoding::Mubuf, name, &[("offen", 1), ("vaddr", 0), ("vdata", vdata), ("srsrc", 2), ("soffset", 128)]);
    let l = lifted(
        &[
            smem("s_load_dwordx4", 8, 0, 0),
            smem("s_buffer_load_dword", 12, 8, 4),
            mubuf("buffer_load_dwordx2", 4),
            mubuf("buffer_store_dword", 6),
            build(&e::MTBUF, Encoding::Mtbuf, "tbuffer_load_format_x", &[("offen", 1), ("format", 22), ("vaddr", 0), ("vdata", 7), ("srsrc", 2), ("soffset", 128)]),
        ],
        &layout,
        &used,
    );
    assert!(l.unsupported.is_empty(), "{:?}", l.unsupported);
    let b = &l.body;
    for (at, _) in b.match_indices("DescriptorHeap[") {
        let rest = &b[at + "DescriptorHeap[".len()..];
        assert!(rest.starts_with("sir_vi(") || rest.starts_with("sir_va("), "an unguarded heap index: {}", &rest[..rest.len().min(60)]);
    }
    for (at, _) in b.match_indices("sir_shadow.Load") {
        let rest = &b[at..];
        assert!(rest.starts_with("sir_shadow.Load(sir_sh(") || rest.starts_with("sir_shadow.Load4(sir_sh("), "an unguarded shadow read: {}", &rest[..rest.len().min(60)]);
    }
    compile(&l, "cov_view_guard");
}

/// The window contract with the memory model: 512 MiB sub-windows, view index from address bits 29-43, offset
/// from bits 0-28; the prelude computes the same as `window_slot`.
#[test]
fn window_views_are_512_mib_sub_windows() {
    use crate::lift::{window_slot, SUB_WINDOW, WINDOW_COUNT, WINDOW_HEAP_BASE};
    assert_eq!(SUB_WINDOW, 1 << 29);
    assert_eq!(window_slot(0), (WINDOW_HEAP_BASE, 0));
    assert_eq!(window_slot(0x1_5cd7_0000), (WINDOW_HEAP_BASE + 10, 0x1cd7_0000), "draw 1's vertex data");
    assert_eq!(window_slot((1 << 44) + 4), (WINDOW_HEAP_BASE, 4), "44 bits of address");
    assert_eq!(window_slot((1 << 44) - 1), (WINDOW_HEAP_BASE + WINDOW_COUNT - 1, (SUB_WINDOW - 1) as u32));
    assert!(PRELUDE.contains(&format!("uint sir_wi(uint lo, uint hi) {{ return {WINDOW_HEAP_BASE}u + (((hi << 3) | (lo >> 29)) & {}u); }}", WINDOW_COUNT - 1)));
    assert!(PRELUDE.contains(&format!("uint sir_wo(uint lo) {{ return lo & {:#x}u; }}", SUB_WINDOW - 1)));
    // loads read the raw SRV at WINDOW_HEAP_BASE + k, stores the raw UAV at WINDOW_UAV_BASE + k (the memory model's
    // layout: rebuild_d3d12x::heap::WINDOW_UAV_BASE)
    assert_eq!(crate::lift::WINDOW_UAV_BASE, 934_464);
    assert!(PRELUDE.contains(&format!("uint sir_wu(uint lo, uint hi) {{ return {}u + (((hi << 3) | (lo >> 29)) & {}u); }}", crate::lift::WINDOW_UAV_BASE, WINDOW_COUNT - 1)));
}
