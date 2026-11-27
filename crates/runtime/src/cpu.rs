//! Console CPU instructions the host CPU may lack, patched where the title uses them.
//!
//! **`clwb`** (cache line write back, `66 [REX] 0F AE /6`, memory operand): the console's Zen 2 has it; many
//! desktop CPUs do not (it faults as an illegal instruction). The title uses it to push CPU writes out to memory
//! for the GPU. On the host it can go: the GPU reads system memory coherently over PCIe (it snoops the CPU's
//! caches), and our upload path sends changed memory explicitly. So the first time each `clwb` faults, the
//! handler checks the bytes really are that instruction, overwrites exactly its bytes with NOPs, and resumes:
//! every later pass over that spot costs nothing.
//!
//! **`clzero`** (`0F 01 FC`, AMD only): zeroes the 64-byte line at `rAX`. It has to keep its effect, and the
//! title runs it in loops over whole buffers, so emulating each one in the handler (an exception per 64 bytes)
//! is too slow. The first fault at a site patches a detour instead: the 3 bytes become `jmp short` to the last 5
//! bytes of a run of `int3` padding nearby (a run of at least 6 that ends at a 16-byte function boundary, so a
//! real `int3` at its start stays), which jump to a stub that zeroes the line with flags and registers kept, then
//! returns to the next instruction. Nothing is relocated (`rAX` is implicit). With no padding in reach (the
//! title's unrolled clear routines), the site and the one instruction after it become a `jmp rel32` to a stub
//! that also runs a copy of that instruction; only register-only forms with no branch or RIP-relative operand
//! are moved (`add r64, imm8`, `cmp`/`test`/`mov` and the like between registers). Anything else: the handler
//! emulates that site.
//!
//! Anything else stays an illegal instruction.

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
    if bytes[..3] == CLZERO {
        let ctx = unsafe { &mut *(*p).ContextRecord };
        return match unsafe { patch_clzero(rip) } {
            Some(()) => EXCEPTION_CONTINUE_EXECUTION, // the site is a jump now (or another thread made it one)
            None => {
                // no padding in reach: this one time, do what the instruction does
                unsafe { std::ptr::write_bytes((ctx.Rax & !63) as *mut u8, 0, 64) };
                ctx.Rip += 3;
                CLZERO_EMULATED.fetch_add(1, Ordering::Relaxed);
                EXCEPTION_CONTINUE_EXECUTION
            }
        };
    }
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

const CLZERO: [u8; 3] = [0x0f, 0x01, 0xfc];
static CLZERO_PATCHED: AtomicU32 = AtomicU32::new(0);
static CLZERO_EMULATED: AtomicU32 = AtomicU32::new(0);
/// stub pages: (base, bytes used)
static PATCHING: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// `clzero` sites patched with a detour, and executions emulated (sites with no padding in reach).
pub fn clzero() -> (u32, u32) {
    (CLZERO_PATCHED.load(Ordering::Relaxed), CLZERO_EMULATED.load(Ordering::Relaxed))
}

/// Where in `window` (bytes around a site, at address `window_at`; the site's `jmp short` ends at `from`) a
/// 5-byte jump can go: the last 5 bytes of a run of at least 6 `int3`s ending at a 16-byte boundary.
pub fn find_cave(window: &[u8], window_at: usize, from: usize) -> Option<usize> {
    let mut i = 0;
    while i < window.len() {
        if window[i] != 0xcc {
            i += 1;
            continue;
        }
        let start = i;
        while i < window.len() && window[i] == 0xcc {
            i += 1;
        }
        let end = window_at + i; // exclusive
        // a run cut by the window's end may go on: only runs that end inside it, at a boundary
        if i < window.len() && i - start >= 6 && end % 16 == 0 {
            let cave = end - 5;
            let rel = cave as isize - from as isize;
            if (-128..=127).contains(&rel) {
                return Some(cave);
            }
        }
    }
    None
}

/// The length of the instruction at `code` if it may be moved into a stub as is: register-only, no branch, no
/// memory operand (so nothing RIP-relative). `REX 83 /r ib` (add, or, adc, sbb, and, sub, xor, cmp with an imm8)
/// and `REX op /r` for add, or, and, sub, xor, cmp, test, mov between registers.
pub fn movable_len(code: &[u8]) -> Option<usize> {
    let rex = *code.first()?;
    if !(0x40..=0x4f).contains(&rex) {
        return None;
    }
    let op = *code.get(1)?;
    let modrm = *code.get(2)?;
    if modrm >> 6 != 3 {
        return None;
    }
    match op {
        0x83 => Some(4),
        0x01 | 0x03 | 0x09 | 0x0b | 0x21 | 0x23 | 0x29 | 0x2b | 0x31 | 0x33 | 0x39 | 0x3b | 0x85 | 0x89 | 0x8b => Some(3),
        _ => None,
    }
}

/// The stub at `at`: zero the 64-byte line at rax, flags and registers kept, then `jmp back`.
pub fn clzero_stub(at: usize, back: usize) -> Vec<u8> {
    clzero_stub_with(at, &[], back)
}

/// As [`clzero_stub`], running `moved` (instructions taken from after the site) before jumping back.
pub fn clzero_stub_with(at: usize, moved: &[u8], back: usize) -> Vec<u8> {
    // pushfq; push rdx; mov rdx, rax; and rdx, -64
    let mut b = vec![0x9c, 0x52, 0x48, 0x89, 0xc2, 0x48, 0x83, 0xe2, 0xc0];
    b.extend([0x48, 0xc7, 0x02, 0, 0, 0, 0]); // mov qword [rdx], 0
    for d in (8..64).step_by(8) {
        b.extend([0x48, 0xc7, 0x42, d as u8, 0, 0, 0, 0]); // mov qword [rdx+d], 0
    }
    b.extend([0x5a, 0x9d]); // pop rdx; popfq
    b.extend_from_slice(moved);
    b.push(0xe9); // jmp rel32
    let rel = back as i64 - (at + b.len() + 4) as i64;
    b.extend((rel as i32).to_le_bytes());
    b
}

/// A stub slot of `len` bytes within reach of `near` (64 KiB executable pages, found by walking free regions).
unsafe fn stub_space(pages: &mut Vec<(usize, usize)>, near: usize, len: usize) -> Option<usize> {
    use windows::Win32::System::Memory::{VirtualAlloc, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE};
    const PAGE: usize = 0x10000;
    const REACH: usize = 0x7000_0000;
    let len = (len + 15) & !15;
    if let Some(p) = pages.iter_mut().find(|(b, used)| b.abs_diff(near) < REACH && used + len <= PAGE) {
        let at = p.0 + p.1;
        p.1 += len;
        return Some(at);
    }
    // free regions below the site first (the image's own neighbourhood), then above
    for down in [true, false] {
        let mut a = if down { (near & !(PAGE - 1)).checked_sub(PAGE)? } else { (near & !(PAGE - 1)) + PAGE };
        while a.abs_diff(near) < REACH && a > PAGE {
            let mut m = MEMORY_BASIC_INFORMATION::default();
            if unsafe { VirtualQuery(Some(a as *const c_void), &mut m, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
                break;
            }
            let (rb, rs) = (m.BaseAddress as usize, m.RegionSize);
            if m.State.0 == 0x10000 && rs >= PAGE {
                let base = (rb + PAGE - 1) & !(PAGE - 1);
                if base + PAGE <= rb + rs {
                    let got = unsafe { VirtualAlloc(Some(base as *const c_void), PAGE, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) };
                    if !got.is_null() {
                        pages.push((got as usize, len));
                        return Some(got as usize);
                    }
                }
            }
            a = if down {
                match rb.checked_sub(1) {
                    Some(x) => x & !(PAGE - 1),
                    None => break,
                }
            } else {
                rb + rs
            };
        }
    }
    None
}

unsafe fn write_code(at: usize, bytes: &[u8]) -> bool {
    let mut old = PAGE_PROTECTION_FLAGS(0);
    unsafe {
        if VirtualProtect(at as *const c_void, bytes.len(), PAGE_EXECUTE_READWRITE, &mut old).is_err() {
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at as *mut u8, bytes.len());
        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(at as *const c_void, bytes.len(), old, &mut ignored);
        let _ = FlushInstructionCache(GetCurrentProcess(), Some(at as *const c_void), bytes.len());
    }
    true
}

/// Detour the `clzero` at `site`. `Some`: the site jumps to a stub now (or already did). `None`: no padding in
/// reach or no stub space; the caller emulates.
///
/// # Safety
/// `site` is executable code holding a `clzero`, with 126 readable bytes before it and 140 after.
pub unsafe fn patch_clzero(site: usize) -> Option<()> {
    let mut pages = PATCHING.lock().unwrap_or_else(|e| e.into_inner());
    // another thread may have patched it while this one waited
    if unsafe { std::slice::from_raw_parts(site as *const u8, 3) } != CLZERO {
        return Some(());
    }
    let from = site + 2;
    let window_at = site.saturating_sub(126);
    let window = unsafe { std::slice::from_raw_parts(window_at as *const u8, site - window_at + 140) };
    let Some(cave) = find_cave(window, window_at, from) else {
        // no padding in reach: move the next instruction into the stub, jump there directly
        let after = unsafe { std::slice::from_raw_parts((site + 3) as *const u8, 15) };
        let n = movable_len(after)?;
        let moved = after[..n].to_vec();
        let stub = unsafe { stub_space(&mut pages, site, 128) }?;
        let code = clzero_stub_with(stub, &moved, site + 3 + n);
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len()) };
        let _ = unsafe { FlushInstructionCache(GetCurrentProcess(), Some(stub as *const c_void), code.len()) };
        let mut patch = vec![0xe9];
        patch.extend(((stub as i64 - (site + 5) as i64) as i32).to_le_bytes());
        patch.resize(3 + n, 0x90); // never run: the jump leaves first
        if !unsafe { write_code(site, &patch) } {
            return None;
        }
        CLZERO_PATCHED.fetch_add(1, Ordering::Relaxed);
        return Some(());
    };
    let stub = unsafe { stub_space(&mut pages, cave, 128) }?;
    let code = clzero_stub(stub, site + 3);
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len()) };
    let _ = unsafe { FlushInstructionCache(GetCurrentProcess(), Some(stub as *const c_void), code.len()) };
    let mut jmp = vec![0xe9];
    jmp.extend(((stub as i64 - (cave + 5) as i64) as i32).to_le_bytes());
    // the cave first, then the site (one 3-byte write: jmp short, nop)
    if !unsafe { write_code(cave, &jmp) } {
        return None;
    }
    let site_code = [0xeb, (cave as isize - from as isize) as i8 as u8, 0x90];
    if !unsafe { write_code(site, &site_code) } {
        return None;
    }
    CLZERO_PATCHED.fetch_add(1, Ordering::Relaxed);
    Some(())
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

    #[test]
    fn a_clzero_site_is_detoured_and_zeroes_its_line() {
        use windows::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE};
        // a function: mov rax, rcx; clzero; ret; int3 padding to the next 16-byte boundary, where a neighbour starts
        let page = unsafe { VirtualAlloc(None, 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) } as usize;
        assert_ne!(page, 0);
        let f = page + 0x100;
        unsafe {
            std::ptr::write_bytes(page as *mut u8, 0xcc, 0x1000);
            std::ptr::copy_nonoverlapping([0x48u8, 0x89, 0xc8, 0x0f, 0x01, 0xfc, 0xc3].as_ptr(), f as *mut u8, 7);
            *((f + 0x10) as *mut u8) = 0xc3;
        }
        assert_eq!(unsafe { patch_clzero(f + 3) }, Some(()));
        assert_eq!(unsafe { *((f + 3) as *const u8) }, 0xeb, "the site jumps now");
        assert_eq!(unsafe { patch_clzero(f + 3) }, Some(()), "a second fault finds it patched");
        let buf = vec![0xffu8; 256];
        let line = (buf.as_ptr() as usize + 64) & !63;
        let run: unsafe extern "system" fn(usize) = unsafe { std::mem::transmute(f) };
        unsafe { run(line + 17) };
        let off = line - buf.as_ptr() as usize;
        assert!(buf[off..off + 64].iter().all(|&b| b == 0), "the line is zero");
        assert!(buf[..off].iter().chain(&buf[off + 64..]).all(|&b| b == 0xff), "nothing else");
        assert!(clzero().0 >= 1);
        unsafe {
            let _ = VirtualFree(page as *mut c_void, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn with_no_padding_in_reach_the_next_instruction_moves_into_the_stub() {
        use windows::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE};
        // mov rax, rcx; clzero; add rax, 40h; mov [rdx], rax; ret -- in a page of NOPs (no int3 anywhere)
        let page = unsafe { VirtualAlloc(None, 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) } as usize;
        let f = page + 0x400;
        unsafe {
            std::ptr::write_bytes(page as *mut u8, 0x90, 0x1000);
            let code = [0x48u8, 0x89, 0xc8, 0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x89, 0x02, 0xc3];
            std::ptr::copy_nonoverlapping(code.as_ptr(), f as *mut u8, code.len());
        }
        assert_eq!(unsafe { patch_clzero(f + 3) }, Some(()));
        assert_eq!(unsafe { *((f + 3) as *const u8) }, 0xe9, "a direct jump");
        let buf = vec![0xffu8; 256];
        let line = (buf.as_ptr() as usize + 64) & !63;
        let mut after = 0usize;
        let run: unsafe extern "system" fn(usize, *mut usize) = unsafe { std::mem::transmute(f) };
        unsafe { run(line + 8, &mut after) };
        let off = line - buf.as_ptr() as usize;
        assert!(buf[off..off + 64].iter().all(|&b| b == 0), "the line is zero");
        assert!(buf[..off].iter().chain(&buf[off + 64..]).all(|&b| b == 0xff), "nothing else");
        assert_eq!(after, line + 8 + 0x40, "the moved add ran once");
        unsafe {
            let _ = VirtualFree(page as *mut c_void, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn only_register_forms_are_moved() {
        assert_eq!(movable_len(&[0x48, 0x83, 0xc0, 0x40]), Some(4), "add rax, 40h");
        assert_eq!(movable_len(&[0x4d, 0x39, 0xda]), Some(3), "cmp r10, r11");
        assert_eq!(movable_len(&[0x48, 0x83, 0x40, 0x08, 0x01]), None, "a memory operand");
        assert_eq!(movable_len(&[0x72, 0xf1]), None, "a branch");
        assert_eq!(movable_len(&[0x83, 0xc0, 0x40]), None, "no REX: not one of the forms seen");
    }

    #[test]
    fn a_cave_needs_a_run_of_six_at_a_boundary_in_short_reach() {
        let mut w = vec![0x90u8; 64];
        w[40..48].fill(0xcc); // ends at 48: a boundary when the window is at 0x1000
        assert_eq!(find_cave(&w, 0x1000, 0x1000 + 10), Some(0x1000 + 43));
        assert_eq!(find_cave(&w, 0x1008, 0x1008 + 10), None, "not at a boundary");
        w[40..43].fill(0x90);
        assert_eq!(find_cave(&w, 0x1000, 0x1000 + 10), None, "five is not enough (a real int3 may lead it)");
    }
}
