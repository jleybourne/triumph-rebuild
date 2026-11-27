//! The root seam (`docs/design/root-seam.md`): how a translated program's user data is made from the title's
//! root arguments, read from the console program's own root layout (its `RootParam` table: which root
//! parameter the hardware loads into which user SGPRs).
//!
//! The lifter reads user SGPR `s` as user-data dword `k = user_data + (s - first SGPR of the parameter)`
//! (`rebuild_sir::lift::res::Bindings::user_dword`), at `b0` (`sir_ud[k / 4][k % 4]`). So dword `k` holds what
//! the console's hardware would have put in that SGPR:
//!
//! ```text
//! root type              user-data dwords                      value
//! Constants              sgprs (one per constant)              constant j of the parameter
//! Table                  1                                     byte offset of the table's first descriptor from
//!                                                              the start of its heap (console sizes: 32, 16)
//! Descriptor (root view) 2                                     the console GPU address, low then high
//! ResourceHeap, SamplerHeap, ImmediateSampler, other           0 (the lifter does not read them as data)
//! ```
//!
//! Each program is mapped once (per hash); the list writes the words at the next draw or dispatch after a
//! change (`rebuild_xlist::console::UserMap`).

use rebuild_shader::native::{NativeProgram, RootType};
use rebuild_xlist::console::{StageUserData, UserMap, UserSource, USER_DWORDS};

use crate::layout::{param, USER_DATA_DWORDS};

const _: () = assert!(USER_DWORDS == USER_DATA_DWORDS as usize);

/// The bytes a table's descriptors span, as the program reads them statically (each used range's offset plus
/// its descriptor size); one descriptor when the program names none.
fn table_bytes(p: &NativeProgram, user_data: u8) -> u32 {
    p.used
        .iter()
        .filter(|u| u.user_data == user_data && u.root == RootType::Table)
        .map(|u| u.table_offset_dwords.saturating_add(u32::from(u.dwords).max(4)).saturating_mul(4))
        .max()
        .unwrap_or(0)
}

/// One stage's user data, written at the library root parameter `param`. Dwords past the root constants
/// cannot be read by a program the library holds (`translate::layout_of` refuses those), so they are left out.
pub fn stage(p: &NativeProgram, param: u32) -> Result<StageUserData, String> {
    let mut s = StageUserData { param, ..Default::default() };
    for rp in &p.layout {
        if rp.slot >= 64 {
            return Err(format!("root parameter {} (past the 64 the list keeps)", rp.slot));
        }
        let slot = rp.slot;
        for j in 0..rp.sgprs.max(1) {
            let k = usize::from(rp.user_data) + j as usize;
            if k >= USER_DWORDS {
                break;
            }
            let src = match rp.root {
                RootType::Constants => UserSource::Constant { slot, dword: j.min(63) as u8 },
                RootType::Table if j == 0 => UserSource::Table { slot, bytes: table_bytes(p, rp.user_data) },
                RootType::Descriptor if j == 0 => UserSource::AddressLo { slot },
                RootType::Descriptor if j == 1 => UserSource::AddressHi { slot },
                _ => UserSource::Zero,
            };
            if src != UserSource::Zero {
                s.src[k] = src;
                s.dwords = s.dwords.max(k as u32 + 1);
            }
        }
    }
    Ok(s)
}

/// A compute pipeline's map: the CS's user data at `CS_USER_DATA`, the shadow table at `CS_SHADOW`.
pub fn compute(cs: &NativeProgram) -> Result<UserMap, String> {
    Ok(UserMap::new(&[stage(cs, param::CS_USER_DATA)?], param::CS_SHADOW))
}

/// A graphics pipeline's map: the vertex and pixel programs' user data at `VS_USER_DATA` / `PS_USER_DATA`,
/// the shadow table at `GFX_SHADOW`. A pipeline without a pixel program writes no pixel user data.
pub fn graphics(vs: &NativeProgram, ps: Option<&NativeProgram>) -> Result<UserMap, String> {
    let v = stage(vs, param::VS_USER_DATA)?;
    let p = match ps {
        Some(ps) => stage(ps, param::PS_USER_DATA)?,
        None => StageUserData { param: param::PS_USER_DATA, ..Default::default() },
    };
    Ok(UserMap::new(&[v, p], param::GFX_SHADOW))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_shader::native::{HwStage, ResourceType, RootParam, StageIo, UsedResource};

    fn rp(root: RootType, sgprs: u32, user_data: u8, slot: u8) -> RootParam {
        RootParam { root, table_offset_dwords: 0, sgprs, base_register: 0, user_data, slot }
    }

    fn program(layout: Vec<RootParam>, used: Vec<UsedResource>) -> NativeProgram {
        NativeProgram { stage: HwStage::Cs, wave64: false, vgprs: 4, sgprs: 16, lds_bytes: 0, layout, used, io: StageIo::None, code: Vec::new() }
    }

    #[test]
    fn each_root_type_becomes_its_user_data() {
        // s0-s1: a heap base and a sampler heap base (the lifter reads 0), s2-s3: a root CBV, s4: a table,
        // s5-s7: three root constants, s8: an immediate sampler
        let p = program(
            vec![
                rp(RootType::ResourceHeap, 1, 0, 9),
                rp(RootType::SamplerHeap, 1, 1, 10),
                rp(RootType::Descriptor, 2, 2, 2),
                rp(RootType::Table, 1, 4, 0),
                rp(RootType::Constants, 3, 5, 1),
                rp(RootType::ImmediateSampler, 4, 8, 11),
            ],
            vec![
                UsedResource { kind: ResourceType::Uav, root: RootType::Table, table_offset_dwords: 16, lower_bound: 0, stride: 0, dwords: 8, user_data: 4, slot: 0 },
                UsedResource { kind: ResourceType::Srv, root: RootType::Table, table_offset_dwords: 0, lower_bound: 0, stride: 0, dwords: 8, user_data: 4, slot: 0 },
            ],
        );
        let m = compute(&p).unwrap();
        assert_eq!((m.stage_count, m.shadow_param, m.stages[0].param), (1, 1, 0));
        let s = &m.stages[0];
        assert_eq!(s.dwords, 8, "up to the last constant");
        use UserSource::*;
        assert_eq!(
            &s.src[..8],
            &[Zero, Zero, AddressLo { slot: 2 }, AddressHi { slot: 2 }, Table { slot: 0, bytes: 96 }, Constant { slot: 1, dword: 0 }, Constant { slot: 1, dword: 1 }, Constant { slot: 1, dword: 2 }]
        );
        assert_eq!(m.slots, 0b111, "slots 0-2 are read; the heaps and the immediate sampler are not");
    }

    #[test]
    fn graphics_maps_both_stages_and_drops_dwords_past_the_root_constants() {
        let vs = program(vec![rp(RootType::Constants, 2, 0, 3), rp(RootType::Constants, 4, 14, 5)], vec![]);
        let ps = program(vec![rp(RootType::Table, 1, 2, 4)], vec![]);
        let m = graphics(&vs, Some(&ps)).unwrap();
        assert_eq!((m.stage_count, m.shadow_param, m.stages[0].param, m.stages[1].param), (2, 2, 0, 1));
        assert_eq!(m.stages[0].dwords, 16, "dwords 14 and 15 kept, 16 and 17 dropped");
        assert_eq!(m.stages[0].src[15], UserSource::Constant { slot: 5, dword: 1 });
        assert_eq!((m.stages[1].dwords, m.stages[1].src[2]), (3, UserSource::Table { slot: 4, bytes: 0 }));
        assert_eq!(graphics(&vs, None).unwrap().stages[1].dwords, 0);
        assert!(stage(&program(vec![rp(RootType::Table, 1, 0, 64)], vec![]), 0).is_err());
    }
}
