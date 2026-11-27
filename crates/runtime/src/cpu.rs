//! Console CPU instructions the host CPU may lack, patched where the title uses them.
//!
//! **`clwb`** (cache line write back, `66 [REX] 0F AE /6`, memory operand): the console's Zen 2 has it; many
//! desktop CPUs do not (it faults as an illegal instruction). The title uses it to push CPU writes out to memory
//! for the GPU. On the host it can go: the GPU reads system memory coherently over PCIe (it snoops the CPU's
//! caches), and our upload path sends changed memory explicitly. So the first time each `clwb` faults, the
//! handler checks the bytes really are that instruction, overwrites exactly its bytes with NOPs, and resumes:
//! every later pass over that spot costs nothing. Anything else stays an illegal instruction.

#![cfg(windows)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::EXCEPTION_ILLEGAL_INSTRUCTION;
use windows::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, FlushInstructionCache, EXCEPTION_POINTERS};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};
use windows::Win32::System::Threading::GetCurrentProcess;

static PATCHED: AtomicU32 = AtomicU32::new(0);

/// `clwb` sites patched so far.
pub fn patched() -> u32 {
    PATCHED.load(Ordering::Relaxed)
}

/// The length of the `clwb` at `code`, if the bytes are one: `66`, an optional REX, `0F AE`, a ModRM with reg 6 and
/// a memory operand, then SIB and displacement as the ModRM asks.
pub fn clwb_len(code: &[u8]) -> Option<usize> {
    let mut i = 0;
    if *code.first()? != 0x66 {
        return None;
    }
    i += 1;
    if (0x40..=0x4f).contains(code.get(i)?) {
        i += 1;
    }
    if code.get(i..i + 2)? != [0x0f, 0xae] {
        return None;
    }
    i += 2;
    let modrm = *code.get(i)?;
    i += 1;
    let (md, reg, rm) = (modrm >> 6, (modrm >> 3) & 7, modrm & 7);
    if reg != 6 || md == 3 {
        return None;
    }
    if rm == 4 {
        let sib = *code.get(i)?;
        i += 1;
        if md == 0 && sib & 7 == 5 {
            i += 4;
        }
    } else if md == 0 && rm == 5 {
        i += 4; // rip-relative
    }
    i += match md {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    (i <= code.len()).then_some(i)
}

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;

unsafe extern "system" fn handler(p: *mut EXCEPTION_POINTERS) -> i32 {
    let rec = unsafe { &*(*p).ExceptionRecord };
    if rec.ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let rip = rec.ExceptionAddress as usize;
    // SAFETY: the faulting instruction's bytes are mapped (the CPU just fetched them); 15 bytes is the x86 maximum
    let bytes = unsafe { std::slice::from_raw_parts(rip as *const u8, 15) };
    let Some(n) = clwb_len(bytes) else { return EXCEPTION_CONTINUE_SEARCH };
    let mut old = PAGE_PROTECTION_FLAGS(0);
    unsafe {
        if VirtualProtect(rip as *const c_void, n, PAGE_EXECUTE_READWRITE, &mut old).is_err() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        std::ptr::write_bytes(rip as *mut u8, 0x90, n);
        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(rip as *const c_void, n, old, &mut ignored);
        let _ = FlushInstructionCache(GetCurrentProcess(), Some(rip as *const c_void), n);
    }
    PATCHED.fetch_add(1, Ordering::Relaxed);
    // resume at the same address: now NOPs
    EXCEPTION_CONTINUE_EXECUTION
}

/// Install the handler (first in line, so it runs before crash reporting). Call before the title runs.
pub fn install() {
    unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clwb_forms_are_measured_and_others_refused() {
        assert_eq!(clwb_len(&[0x66, 0x41, 0x0f, 0xae, 0x31, 0x90]), Some(5), "clwb [r9], as the title has it");
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x30]), Some(4), "clwb [rax]");
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x74, 0x24, 0x08]), Some(6), "clwb [rsp+8]: SIB and disp8");
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x35, 1, 2, 3, 4]), Some(8), "clwb [rip+disp32]");
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x38]), None, "reg 7 is clflushopt: not ours to touch");
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0xf0]), None, "a register form is not clwb");
        assert_eq!(clwb_len(&[0x0f, 0xae, 0x30]), None, "no 66 prefix");
    }
}
