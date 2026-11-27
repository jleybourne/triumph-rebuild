//! The 12 module overrides: the title's modules are mapped by our loader, not the host's, so module
//! lookups go through this registry first. Console DLLs we implement get pseudo-handles; host DLLs
//! stay the host's. **Every miss is a discovery entry**: the GDK functions the title reaches by name
//! (`XGame*`, `XUser*`, …) are found here at run time, not in its import table.

#![cfg(windows)]
#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary as HostFreeLibrary, SetLastError, BOOL, ERROR_MOD_NOT_FOUND, ERROR_PROC_NOT_FOUND, HMODULE, WIN32_ERROR};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW as HostGetModuleFileNameW, GetModuleHandleW as HostGetModuleHandleW,
    GetProcAddress as HostGetProcAddress, LoadLibraryExW as HostLoadLibraryExW, LOAD_LIBRARY_FLAGS, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// A module the title can see.
#[derive(Debug, Clone)]
pub enum Module {
    /// mapped by our loader: base address and its exports (name → RVA, ordinal → RVA)
    Title { base: u64, path: String, names: HashMap<String, u32>, ordinals: HashMap<u16, u32> },
    /// a console-only DLL we implement (`exports::console_export`), behind a pseudo-handle
    Console { handle: u64, name: String },
}

#[derive(Default)]
struct Registry {
    /// lower-case DLL name (with `.dll`) → module
    by_name: HashMap<String, Module>,
    /// the title's exe (`GetModuleHandle(NULL)`)
    exe: Option<String>,
    /// every lookup that found nothing: (module, function) — the discovery list
    misses: Vec<(String, String)>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Registry::default))
}

/// Pseudo-handles for console DLLs: never valid host addresses (above the user address space).
const PSEUDO: u64 = 0xFFFF_8000_0000_0000;

fn key(name: &str) -> String {
    let n = name.rsplit(['\\', '/']).next().unwrap_or(name).to_ascii_lowercase();
    if n.contains('.') { n } else { n + ".dll" }
}

/// Register a module the loader mapped (`exe = true` for the title's executable).
pub fn register_title(name: &str, base: u64, path: &str, names: HashMap<String, u32>, ordinals: HashMap<u16, u32>, exe: bool) {
    with(|r| {
        let k = key(name);
        if exe {
            r.exe = Some(k.clone());
        }
        r.by_name.insert(k, Module::Title { base, path: path.into(), names, ordinals });
    });
}

/// Register a console-only DLL we implement, so the title can load it by name.
pub fn register_console(name: &str) -> u64 {
    with(|r| {
        let k = key(name);
        if let Some(Module::Console { handle, .. }) = r.by_name.get(&k) {
            return *handle;
        }
        let handle = PSEUDO + (r.by_name.len() as u64 + 1) * 0x1_0000;
        r.by_name.insert(k.clone(), Module::Console { handle, name: k });
        handle
    })
}

/// The discovery list so far: every module or function the title asked for and did not get.
pub fn misses() -> Vec<(String, String)> {
    with(|r| r.misses.clone())
}

fn find_by_handle(r: &Registry, h: u64) -> Option<Module> {
    r.by_name.values().find(|m| match m {
        Module::Title { base, .. } => *base == h,
        Module::Console { handle, .. } => *handle == h,
    }).cloned()
}

fn handle_of(m: &Module) -> u64 {
    match m {
        Module::Title { base, .. } => *base,
        Module::Console { handle, .. } => *handle,
    }
}

unsafe fn wide(p: PCWSTR) -> Option<String> {
    (!p.is_null()).then(|| p.to_string().unwrap_or_default())
}

unsafe fn narrow(p: PCSTR) -> Option<String> {
    (!p.is_null()).then(|| p.to_string().unwrap_or_default())
}

fn miss(module: &str, function: &str, err: WIN32_ERROR) {
    with(|r| r.misses.push((module.to_string(), function.to_string())));
    unsafe { SetLastError(err) };
}

// ---------------------------------------------------------------------------- the overrides

pub extern "system" fn GetProcAddress(module: HMODULE, name: PCSTR) -> *mut c_void {
    let h = module.0 as u64;
    let ordinal = (name.0 as usize) < 0x1_0000;
    let label = if ordinal { format!("#{}", name.0 as usize) } else { unsafe { narrow(name) }.unwrap_or_default() };
    let found = with(|r| find_by_handle(r, h));
    match found {
        Some(Module::Title { base, names, ordinals, path }) => {
            let rva = if ordinal { ordinals.get(&(name.0 as usize as u16)).copied() } else { names.get(&label).copied() };
            match rva {
                Some(rva) => (base + rva as u64) as *mut c_void,
                None => {
                    miss(&key(&path), &label, ERROR_PROC_NOT_FOUND);
                    std::ptr::null_mut()
                }
            }
        }
        Some(Module::Console { name: dll, .. }) => match (!ordinal).then(|| crate::exports::console_export(&dll, &label)).flatten() {
            Some(f) => f as *mut c_void,
            None => {
                miss(&dll, &label, ERROR_PROC_NOT_FOUND);
                std::ptr::null_mut()
            }
        },
        None => match unsafe { HostGetProcAddress(module, name) } {
            Some(f) => f as *mut c_void,
            None => {
                let mut buf = [0u16; 260];
                let n = unsafe { HostGetModuleFileNameW(module, &mut buf) } as usize;
                miss(&key(&String::from_utf16_lossy(&buf[..n])), &label, ERROR_PROC_NOT_FOUND);
                std::ptr::null_mut()
            }
        },
    }
}

/// Title modules and our console DLLs first; then the host, from System32 only.
pub extern "system" fn LoadLibraryExW(name: PCWSTR, _file: *mut c_void, _flags: u32) -> HMODULE {
    let Some(n) = (unsafe { wide(name) }) else { return HMODULE::default() };
    let k = key(&n);
    if let Some(m) = with(|r| r.by_name.get(&k).cloned()) {
        return HMODULE(handle_of(&m) as *mut c_void);
    }
    if crate::exports::is_console_dll(&k) {
        return HMODULE(register_console(&k) as *mut c_void);
    }
    match unsafe { HostLoadLibraryExW(name, None, LOAD_LIBRARY_FLAGS(LOAD_LIBRARY_SEARCH_SYSTEM32.0)) } {
        Ok(h) => h,
        Err(_) => {
            miss(&k, "(the DLL)", ERROR_MOD_NOT_FOUND);
            HMODULE::default()
        }
    }
}

pub extern "system" fn LoadLibraryExA(name: PCSTR, file: *mut c_void, flags: u32) -> HMODULE {
    let Some(n) = (unsafe { narrow(name) }) else { return HMODULE::default() };
    let w: Vec<u16> = n.encode_utf16().chain(std::iter::once(0)).collect();
    LoadLibraryExW(PCWSTR(w.as_ptr()), file, flags)
}

pub extern "system" fn LoadLibraryW(name: PCWSTR) -> HMODULE {
    LoadLibraryExW(name, std::ptr::null_mut(), 0)
}

pub extern "system" fn LoadLibraryA(name: PCSTR) -> HMODULE {
    let Some(n) = (unsafe { narrow(name) }) else { return HMODULE::default() };
    let w: Vec<u16> = n.encode_utf16().chain(std::iter::once(0)).collect();
    LoadLibraryExW(PCWSTR(w.as_ptr()), std::ptr::null_mut(), 0)
}

pub extern "system" fn LoadPackagedLibrary(name: PCWSTR, _reserved: u32) -> HMODULE {
    LoadLibraryExW(name, std::ptr::null_mut(), 0)
}

/// NULL is the title's exe, not ours.
pub extern "system" fn GetModuleHandleW(name: PCWSTR) -> HMODULE {
    let found = match unsafe { wide(name) } {
        None => with(|r| r.exe.clone().and_then(|e| r.by_name.get(&e).cloned())),
        Some(n) => with(|r| r.by_name.get(&key(&n)).cloned()),
    };
    match found {
        Some(m) => HMODULE(handle_of(&m) as *mut c_void),
        None if name.is_null() => unsafe { HostGetModuleHandleW(None) }.unwrap_or_default(),
        None => unsafe { HostGetModuleHandleW(name) }.unwrap_or_default(),
    }
}

pub extern "system" fn GetModuleHandleA(name: PCSTR) -> HMODULE {
    match unsafe { narrow(name) } {
        None => GetModuleHandleW(PCWSTR::null()),
        Some(n) => {
            let w: Vec<u16> = n.encode_utf16().chain(std::iter::once(0)).collect();
            GetModuleHandleW(PCWSTR(w.as_ptr()))
        }
    }
}

pub extern "system" fn GetModuleHandleExW(_flags: u32, name: PCWSTR, out: *mut HMODULE) -> BOOL {
    if out.is_null() {
        return BOOL(0);
    }
    let h = GetModuleHandleW(name);
    unsafe { *out = h };
    BOOL(!h.is_invalid() as i32)
}

/// The title sees its modules at console paths (`G:\name`), never the host's.
pub extern "system" fn GetModuleFileNameW(module: HMODULE, buf: *mut u16, size: u32) -> u32 {
    let h = if module.is_invalid() { GetModuleHandleW(PCWSTR::null()).0 as u64 } else { module.0 as u64 };
    let path = with(|r| find_by_handle(r, h)).map(|m| match m {
        Module::Title { path, .. } => path,
        Module::Console { name, .. } => format!("C:\\Windows\\System32\\{name}"),
    });
    match path {
        Some(p) => {
            let w: Vec<u16> = p.encode_utf16().collect();
            let n = w.len().min(size.saturating_sub(1) as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(w.as_ptr(), buf, n);
                buf.add(n).write(0);
            }
            n as u32
        }
        None => unsafe { HostGetModuleFileNameW(module, std::slice::from_raw_parts_mut(buf, size as usize)) },
    }
}

pub extern "system" fn GetModuleFileNameA(module: HMODULE, buf: *mut u8, size: u32) -> u32 {
    let mut w = vec![0u16; size as usize + 1];
    let n = GetModuleFileNameW(module, w.as_mut_ptr(), size) as usize;
    let s = String::from_utf16_lossy(&w[..n]);
    let b = s.as_bytes();
    let n = b.len().min(size.saturating_sub(1) as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(b.as_ptr(), buf, n);
        buf.add(n).write(0);
    }
    n as u32
}

/// Our modules stay for the life of the title; host modules are the host's to count.
pub extern "system" fn FreeLibrary(module: HMODULE) -> BOOL {
    if with(|r| find_by_handle(r, module.0 as u64)).is_some() {
        return BOOL(1);
    }
    BOOL(unsafe { HostFreeLibrary(module) }.is_ok() as i32)
}

/// Our override of a host module function, by name (the resolver binds these instead of the host's).
pub fn override_export(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "GetProcAddress" => GetProcAddress as *const (),
        "LoadLibraryExW" => LoadLibraryExW as *const (),
        "LoadLibraryExA" => LoadLibraryExA as *const (),
        "LoadLibraryW" => LoadLibraryW as *const (),
        "LoadLibraryA" => LoadLibraryA as *const (),
        "LoadPackagedLibrary" => LoadPackagedLibrary as *const (),
        "GetModuleHandleW" => GetModuleHandleW as *const (),
        "GetModuleHandleA" => GetModuleHandleA as *const (),
        "GetModuleHandleExW" => GetModuleHandleExW as *const (),
        "GetModuleFileNameW" => GetModuleFileNameW as *const (),
        "GetModuleFileNameA" => GetModuleFileNameA as *const (),
        "FreeLibrary" => FreeLibrary as *const (),
        // network: ours only while the offline online-mode sandbox is on, else None (the host's)
        other => return crate::sandbox::override_export(other).or_else(|| crate::exports::override_export(other)),
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cstr(s: &str) -> std::ffi::CString {
        std::ffi::CString::new(s).unwrap()
    }
    fn wstr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn title_modules_console_dlls_and_the_host_resolve_and_misses_are_logged() {
        // a title module mapped by our loader, at a made-up base
        let mut names = HashMap::new();
        names.insert("TitleEntry".to_string(), 0x1234u32);
        let mut ords = HashMap::new();
        ords.insert(7u16, 0x40u32);
        register_title("title_test.exe", 0x7000_0000_0000, "G:\\title_test.exe", names, ords, true);

        let exe = GetModuleHandleW(PCWSTR::null());
        assert_eq!(exe.0 as u64, 0x7000_0000_0000, "NULL is the title's exe");
        let n = cstr("TitleEntry");
        assert_eq!(GetProcAddress(exe, PCSTR(n.as_ptr() as *const u8)) as u64, 0x7000_0000_1234);
        assert_eq!(GetProcAddress(exe, PCSTR(7 as *const u8)) as u64, 0x7000_0000_0040, "by ordinal");

        // a console DLL we implement: a pseudo-handle, and its functions are ours
        let xm = wstr("xmem.dll");
        let h = LoadLibraryExW(PCWSTR(xm.as_ptr()), std::ptr::null_mut(), 0);
        assert!(h.0 as u64 >= PSEUDO);
        let f = cstr("XMemAlloc");
        assert_eq!(GetProcAddress(h, PCSTR(f.as_ptr() as *const u8)) as u64, crate::exports::console_export("xmem.dll", "XMemAlloc").unwrap());

        // the host still answers for host DLLs
        let k32 = wstr("kernel32.dll");
        let hk = LoadLibraryW(PCWSTR(k32.as_ptr()));
        let t = cstr("GetTickCount64");
        assert!(!GetProcAddress(hk, PCSTR(t.as_ptr() as *const u8)).is_null());

        // misses: a GDK function by name, and a console DLL we don't have
        let g = cstr("XGameGetXboxTitleId");
        assert!(GetProcAddress(h, PCSTR(g.as_ptr() as *const u8)).is_null());
        let gr = wstr("xgameruntime.dll.not-real");
        assert!(LoadLibraryW(PCWSTR(gr.as_ptr())).is_invalid());
        let m = misses();
        assert!(m.contains(&("xmem.dll".into(), "XGameGetXboxTitleId".into())), "{m:?}");
        assert!(m.iter().any(|(d, _)| d == "xgameruntime.dll.not-real"), "{m:?}");

        // the title sees its exe at a console path
        let mut buf = [0u16; 64];
        let n = GetModuleFileNameW(HMODULE::default(), buf.as_mut_ptr(), 64) as usize;
        assert_eq!(String::from_utf16_lossy(&buf[..n]), "G:\\title_test.exe");
        assert_eq!(FreeLibrary(exe).0, 1);
    }

    #[test]
    fn every_module_override_has_an_implementation() {
        for (f, why) in crate::OVERRIDES.iter().filter(|(_, w)| *w == "modules" || *w == "process") {
            assert!(override_export(f).is_some(), "{f} ({why})");
        }
    }

    #[test]
    fn xgameruntime_by_the_a_form_is_ours_not_the_pcs() {
        let n = cstr("xgameruntime.dll");
        let h = LoadLibraryExA(PCSTR(n.as_ptr() as *const u8), std::ptr::null_mut(), 0);
        assert!(h.0 as u64 >= PSEUDO, "a console pseudo-handle, not the host's PC runtime: {:?}", h);
        let q = cstr("QueryApiImpl");
        let f = GetProcAddress(h, PCSTR(q.as_ptr() as *const u8));
        assert_eq!(f as usize as u64, rebuild_gdk::runtime::get_proc("QueryApiImpl").unwrap() as usize as u64);
        let w = wstr("XGameRuntime.dll");
        assert_eq!(LoadLibraryExW(PCWSTR(w.as_ptr()), std::ptr::null_mut(), 0), h, "one handle, any case or form");
    }

}
