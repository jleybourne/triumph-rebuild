//! The script VM's natives: the call ABI and a scanner that lists every handler with its 64-bit hash.
//!
//! **ABI** (from the interpreter's native opcode, `exe+0x3de7411..0x3de7511`): a handler is
//! `extern "system" fn(ctx: *mut Ctx)`. The VM points `args` at the first argument slot on its stack and `ret` at
//! the stack top (null when the native returns nothing), calls the handler, then copies the results down over
//! the arguments. Slots are 8 bytes; a vector (`x, y, z, w` as `f32`) and an entity handle ([`Handle`]) each take
//! two slots and are read as one 16-byte load.
//!
//! **Hash probe**: every handler starts `cmp dword [rcx+0x10], 0; js probe`. The probe stores the native's 64-bit
//! hash into `args[0]` and then jumps back into the body, so the native still runs. The scanner reads those
//! hashes statically; a negative `argc` must never be passed at run time.
//!
//! The hashes are the title's own; none of the public lists for earlier titles on the engine match
//! (checked: twelve well-known hashes, zero hits anywhere in the image). Names are inferred per native in
//! `docs/research/devmenu.md`.

use crate::pe::Pe;

/// The context a handler receives (`rcx`). Only these four fields are read by handlers.
#[repr(C)]
#[derive(Debug)]
pub struct Ctx {
    /// First argument slot.
    pub args: *mut u64,
    /// Where results go; null when the native returns nothing.
    pub ret: *mut u64,
    /// Number of argument slots. Negative triggers the hash probe (see the module docs): never do that.
    pub argc: i32,
    /// Number of result slots.
    pub retc: u32,
}

/// A script handle to an entity: 16 bytes, two slots. `tag` is the handle kind (`4` seen for one kind at
/// `exe+0x3c313cc`); `value` is the key the handle map (`exe+0x4372d0`) looks up.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Handle {
    pub value: u64,
    pub index: u32,
    pub tag: u32,
}

impl Handle {
    pub fn is_null(&self) -> bool {
        self.value == 0
    }
}

/// A native handler found in the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Native {
    /// Handler RVA.
    pub rva: u32,
    /// The native's 64-bit hash, from its probe.
    pub hash: u64,
}

/// The handler a script's native opcode calls: `anchor - (rotr(key24, 12) ^ key10) + operand`, where `anchor` is
/// `exe+0x1d109b0`, `key24`/`key10` are the dwords at `+0x24`/`+0x10` of the first program the title loads (set
/// once, at `exe+0x1c18379..0x1c1839d`, into the global at `exe+0x98c1810`) and `operand` is the opcode's decoded
/// `u32`.
pub fn handler_address(anchor: u64, key24: u32, key10: u32, operand: u32) -> u64 {
    anchor.wrapping_sub((key24.rotate_right(12) ^ key10) as u64).wrapping_add(operand as u64)
}

/// The probe of a handler that starts at `code` (its first bytes, at least 0x40 long): the hash, if the
/// function is a native handler. `at(rel)` returns bytes at an offset relative to the function start
/// (the probe lies after the body).
fn probe(code: &[u8], at: &dyn Fn(i64, usize) -> Option<Vec<u8>>) -> Option<u64> {
    // `cmp dword ptr [rcx+0x10], 0` within the prologue
    let k = code.windows(4).take(0x30).position(|w| w == [0x83, 0x79, 0x10, 0x00])?;
    // then a `js` within the next few instructions (`mov rsi, rcx` usually sits between)
    for j in k + 4..(k + 14).min(code.len().saturating_sub(6)) {
        let target = match code[j] {
            0x78 => j as i64 + 2 + code[j + 1] as i8 as i64,
            0x0f if code[j + 1] == 0x88 => j as i64 + 6 + i32::from_le_bytes(code[j + 2..j + 6].try_into().ok()?) as i64,
            _ => continue,
        };
        let t = at(target, 24)?;
        // `mov rax, [rsi]`, then `movabs r64, imm64` and a store of that register
        for i in 0..t.len().saturating_sub(12) {
            if (t[i] == 0x48 || t[i] == 0x49) && (0xb8..=0xbf).contains(&t[i + 1]) && matches!(t[i + 10], 0x48 | 0x49 | 0x4c | 0x4d) && t[i + 11] == 0x89 {
                return Some(u64::from_le_bytes(t[i + 2..i + 10].try_into().ok()?));
            }
        }
        return None;
    }
    None
}

/// Every native handler in the image, with its hash, in RVA order.
pub fn scan(pe: &Pe) -> Vec<Native> {
    let mut out = Vec::new();
    let mut last = u32::MAX;
    for f in pe.functions() {
        if f.begin == last {
            continue;
        }
        last = f.begin;
        let Some(code) = pe.bytes(f.begin, 0x40) else { continue };
        let at = |rel: i64, len: usize| pe.bytes((f.begin as i64 + rel) as u32, len).map(|b| b.to_vec());
        if let Some(hash) = probe(code, &at) {
            out.push(Native { rva: f.begin, hash });
        }
    }
    out
}

/// Natives named so far, by hash. Confidence and evidence for each are in `docs/research/devmenu.md`.
pub mod known {
    /// `(entity: Handle, alive: bool) -> Vector`: unpacks `entity + 0x70` (handler `exe+0x3c09d00`).
    pub const GET_ENTITY_COORDS: u64 = 0x6c66_0bb2_5792_a0f9;
    /// `() -> Handle`: the local player's ped (handler `exe+0x4461c00`, impl `exe+0x447f600`).
    pub const PLAYER_PED_ID: u64 = 0x96b4_308c_db2a_fe46;
    /// `(entity: Handle, rotation: Vector, order: int, flag: bool)`: rebuilds the matrix through the
    /// entity's `SetMatrix` virtual (handler `exe+0x3c14d00`).
    pub const SET_ENTITY_ROTATION: u64 = 0x0121_bbe1_6337_ceb2;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_and_handle_layout() {
        assert_eq!(std::mem::size_of::<Ctx>(), 0x18);
        assert_eq!(std::mem::offset_of!(Ctx, argc), 0x10);
        assert_eq!(std::mem::offset_of!(Ctx, retc), 0x14);
        assert_eq!(std::mem::size_of::<Handle>(), 16);
    }

    #[test]
    fn handler_formula() {
        // rorx 12 is a right rotate
        assert_eq!(handler_address(0x1_41d1_09b0, 0x0000_1000, 0, 0x10), 0x1_41d1_09b0 - 1 + 0x10);
        assert_eq!(handler_address(0x1_41d1_09b0, 0, 0, 0), 0x1_41d1_09b0);
    }

    /// The handler at `exe+0x4d027b0`, byte for byte (its body is not needed: only the prologue and probe).
    #[test]
    fn probe_reads_the_hash() {
        let mut code = vec![0xcc; 0x80];
        // push rsi; push rdi; sub rsp,0x68; cmp dword [rcx+0x10],0; mov rsi,rcx; js +0x4d
        let head = [0x56, 0x57, 0x48, 0x83, 0xec, 0x68, 0x83, 0x79, 0x10, 0x00, 0x48, 0x8b, 0xf1, 0x78, 0x4d];
        code[..head.len()].copy_from_slice(&head);
        // at 0x5c: mov rax,[rsi]; movabs rcx, 0x7e150fa21adc5e1f; mov [rax],rcx; jmp back
        let probe_bytes = [0x48, 0x8b, 0x06, 0x48, 0xb9, 0x1f, 0x5e, 0xdc, 0x1a, 0xa2, 0x0f, 0x15, 0x7e, 0x48, 0x89, 0x08, 0xeb, 0xa1];
        code[0x5c..0x5c + probe_bytes.len()].copy_from_slice(&probe_bytes);
        let at = |rel: i64, len: usize| code.get(rel as usize..rel as usize + len).map(|b| b.to_vec());
        assert_eq!(probe(&code[..0x40], &at), Some(0x7e15_0fa2_1adc_5e1f));
        // a function without the probe
        let plain = [0x48, 0x83, 0xec, 0x28, 0x33, 0xc0, 0x48, 0x83, 0xc4, 0x28, 0xc3];
        let mut p = vec![0xcc; 0x40];
        p[..plain.len()].copy_from_slice(&plain);
        let at2 = |rel: i64, len: usize| p.get(rel as usize..rel as usize + len).map(|b| b.to_vec());
        assert_eq!(probe(&p, &at2), None);
    }

    /// Against the image copy, when it is on this machine: every handler and the natives named so far.
    #[test]
    fn scan_the_image_copy() {
        let Some(path) = rebuild_storage::find_cached_image("game_scarlett_master-59e9b414471e3e6d.dll") else { return };
        let Ok(data) = std::fs::read(&path) else { return };
        let pe = Pe::parse(&data, false).expect("a PE32+ image");
        assert_eq!(pe.image_base, crate::addr::IMAGE_BASE);
        let natives = scan(&pe);
        assert_eq!(natives.len(), 6818, "the static count");
        let mut hashes: Vec<u64> = natives.iter().map(|n| n.hash).collect();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(hashes.len(), natives.len(), "hashes are unique");
        let find = |h: u64| natives.iter().find(|n| n.hash == h).map(|n| n.rva);
        assert_eq!(find(known::GET_ENTITY_COORDS), Some(0x3c0_9d00));
        assert_eq!(find(known::PLAYER_PED_ID), Some(0x446_1c00));
        assert_eq!(find(known::SET_ENTITY_ROTATION), Some(0x3c1_4d00));
    }
}
