//! Addresses in the title, as preferred VAs (`IMAGE_BASE` + RVA) of `game_scarlett_master-59e9b414471e3e6d`.
//! Rebase with [`rva`] when the image is mapped elsewhere. Each item names its confidence:
//! **high** = read directly from code with corroboration, **medium** = one consistent reading, needs a live
//! check, **low** = a candidate. Evidence: `docs/research/devmenu.md`.

/// The image's preferred base.
pub const IMAGE_BASE: u64 = 0x1_4000_0000;

/// `va - IMAGE_BASE`.
pub const fn rva(va: u64) -> u32 {
    (va - IMAGE_BASE) as u32
}

/// The script VM.
pub mod vm {
    /// Interpreter (high). `(thread_ctx*) -> state`; a 545-entry jump table at `0x146553900`; opcodes are
    /// obfuscated 16-bit values decoded with an xor/multiply.
    pub const INTERPRETER: u64 = 0x1_43de_2550;
    /// Thin entry that tail-calls the interpreter (high). Its one caller is [`THREAD_RUN`] at `0x143de204f`.
    pub const INTERPRETER_ENTRY: u64 = 0x1_43de_2540;
    /// The script thread's run virtual (high): vtable slot 17 of the four script-thread vtables
    /// (`0x14654fd20`, `0x14654fde0`, `0x1465604f0`, and one inside `0x1465603f0`). Around the interpreter
    /// call it stores the thread in the TLS block at [`tls::ACTIVE_THREAD`].
    pub const THREAD_RUN: u64 = 0x1_43de_1f10;
    /// The native opcode's handler inside the interpreter (high): builds a [`crate::native::Ctx`] at `rsp+0xa8`
    /// and calls the handler at `0x143de750e`.
    pub const NATIVE_OPCODE: u64 = 0x1_43de_7411;
    /// The anchor of the handler formula ([`crate::native::handler_address`]) (high).
    pub const HANDLER_ANCHOR: u64 = 0x1_41d1_09b0;
    /// Global holding `anchor - key`, set once from the first program (high).
    pub const HANDLER_BASE: u64 = 0x1_498c_1810;
    /// The recursive lock every native handler takes (high): acquire `0x140004b40`, release `0x144d9dab0`.
    pub const NATIVE_LOCK: u64 = 0x1_4aad_44e0;
    pub const LOCK_ACQUIRE: u64 = 0x1_4000_4b40;
    pub const LOCK_RELEASE: u64 = 0x1_44d9_dab0;
}

/// Offsets in the per-thread block at `*(*(gs:[0x58]) + 0)` (TLS slot 0).
pub mod tls {
    /// The running script thread (high; written at `0x143de2048`, restored at `0x143de2054`).
    pub const ACTIVE_THREAD: usize = 0x1478;
    /// The thread's small index used as the lock owner id (high; `0x140004b61`).
    pub const THREAD_INDEX: usize = 0x1454;
}

/// Script handle to object lookups (`(handle: *const Handle) -> *mut T`, null when absent or of another class).
pub mod cast {
    /// Handle → `fwExtensibleBase*`, through a reader-locked map (high).
    pub const HANDLE_TO_BASE: u64 = 0x1_4043_72d0;
    /// Handle → `CEntity*` (high).
    pub const ENTITY: u64 = 0x1_4035_0170;
    /// Handle → `CPed*` (high).
    pub const PED: u64 = 0x1_4526_e340;
    /// Handle → `CVehicle*` (high).
    pub const VEHICLE: u64 = 0x1_40d3_5050;
    /// Handle → `CDynamicEntity*` (high).
    pub const DYNAMIC_ENTITY: u64 = 0x1_407b_a630;
    /// Handle → `CObject*` (high).
    pub const OBJECT: u64 = 0x1_4133_bbd0;
}

/// Entities.
pub mod entity {
    /// Global `CPed*` of the local player (medium-high): read at 1,842 sites, which use `+0x9c` (530 times),
    /// `+0x70` (151) and `+0x30` (12), the entity fields below. Check live that `*(p + 0x30) == 3`.
    pub const LOCAL_PLAYER_PED: u64 = 0x1_4ab5_6e38;
    /// `u8` entity type (high for 3 = ped, from `0x140350215`; 2, 3, 4 are the physical types that get a
    /// physics sync after a matrix change).
    pub const TYPE: usize = 0x30;
    /// Matrix rows `right`, `forward`, `up` (16 bytes each, `w` unused) (medium).
    pub const MATRIX: usize = 0x40;
    /// Packed position ([`crate::world::Packed`]) (high).
    pub const POSITION: usize = 0x70;
    /// `u32` entity id; bits 17..21 are a type (`0x60000` for peds at `0x144da6540`) (high).
    pub const ID: usize = 0x9c;
    /// vtable slot of `SetMatrix(this, mat: *const [f32x4; 4], a: bool, b: bool, warp: u8)` (medium; called
    /// with `a = b = true` at `0x143c25e62` and `0x143c260aa`).
    pub const VT_SET_MATRIX: usize = 0x170;
    /// After `SetMatrix` the title syncs physics for types 2..4: `phys = 0x1450bb780(id)`, then
    /// `0x142080950(phys)` when non-null (medium).
    pub const PHYSICS_FOR_ID: u64 = 0x1_450b_b780;
    pub const PHYSICS_SYNC: u64 = 0x1_4208_0950;
    /// Packed position → world (high).
    pub const UNPACK_POSITION: u64 = 0x1_44dc_3790;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rva_of_preferred_va() {
        assert_eq!(rva(vm::INTERPRETER), 0x3de_2550);
        assert_eq!(rva(entity::LOCAL_PLAYER_PED), 0xab5_6e38);
    }
}
