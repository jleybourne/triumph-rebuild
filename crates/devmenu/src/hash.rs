//! The title's string hash: Jenkins one-at-a-time over the lower-cased bytes (`atStringHash` in the engine).
//!
//! Evidence: the entity class checks pass `joaat` of lower-cased class names to the class-id virtual
//! (`CPed` = `0x94821659` at `exe+0x526e355`, `CEntity` = `0xb1389614` at `exe+0x350185`).

/// Jenkins one-at-a-time hash of `s`, ASCII lower-cased first.
pub const fn joaat(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut h: u32 = 0;
    let mut i = 0;
    while i < b.len() {
        h = h.wrapping_add(b[i].to_ascii_lowercase() as u32);
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        i += 1;
    }
    h = h.wrapping_add(h << 3);
    h ^= h >> 11;
    h.wrapping_add(h << 15)
}

/// Class ids the title checks through the entity's class-id virtual (`vtable[1]`, `(this, id) -> bool`).
pub mod class {
    use super::joaat;
    pub const FW_ENTITY: u32 = joaat("fwEntity");
    pub const ENTITY: u32 = joaat("CEntity");
    pub const DYNAMIC_ENTITY: u32 = joaat("CDynamicEntity");
    pub const PED: u32 = joaat("CPed");
    pub const VEHICLE: u32 = joaat("CVehicle");
    pub const OBJECT: u32 = joaat("CObject");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_ids_match_the_image() {
        // the immediates the title passes to the class-id virtual (see `addr::cast`)
        assert_eq!(class::ENTITY, 0xb138_9614);
        assert_eq!(class::PED, 0x9482_1659);
        assert_eq!(class::DYNAMIC_ENTITY, 0x291d_d2f1);
        assert_eq!(class::VEHICLE, 0xf820_aaa9);
        assert_eq!(class::OBJECT, 0x9cec_50b6);
        assert_eq!(class::FW_ENTITY, 0x9423_cddb);
    }

    #[test]
    fn case_does_not_matter() {
        assert_eq!(joaat("CPED"), joaat("cped"));
        assert_eq!(joaat(""), 0);
    }
}
