//! Build 3, second half: the console runtime — what the title's imports bind to.
//!
//! The title is native x86-64 code using the Windows calling convention. So most of what it imports
//! already exists on the host with the same meaning: the API-set DLLs (`api-ms-win-*`), `kernel32`,
//! the C runtime, sockets, HTTP, crypto. Those bind **straight to the host's own functions**: no
//! wrapper, no cost. Three kinds of import need us instead:
//!
//! ```text
//! override    the host has it, but the meaning differs here: memory (the title's memory lives in our
//!             watched regions and tiled windows), file paths (console drives through `vfs`), module
//!             lookups (the title's own DLLs, and every GetProcAddress miss is logged for discovery)
//! console     a DLL or function the host doesn't have (xmem, the audio and APU DLLs, D3D12.X, …)
//! host        everything else, bound to the host directly
//! ```

#[cfg(windows)]
pub mod budget;
pub mod limits;
pub mod device;
pub mod cpu;
pub mod drives;
pub mod dxgi;
pub mod frame;
pub mod framelog;
pub mod fence;
pub mod queue;
pub mod submit;
#[cfg(windows)]
pub mod exports;
#[cfg(windows)]
pub mod modules;
pub mod physical;
pub mod overlap;
pub mod pipeline;
pub mod registry;
pub mod present;
#[cfg(all(test, windows))]
mod present_card;
#[cfg(windows)]
pub mod title;
pub mod tls;
#[cfg(windows)]
pub mod xg;
pub mod xmem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// we implement it, although the host has it
    Override,
    /// bound to the host's own function
    Host,
    /// the host's DLL exists but lacks this function: a console-only addition to a host DLL
    ConsoleFunction,
    /// the whole DLL is console-only
    ConsoleDll,
}

/// Host functions whose meaning differs on the console, by function name (any DLL that exports it:
/// the same function is reached through several API-set names).
pub const OVERRIDES: &[(&str, &str)] = &[
    // the console's device type: the registry value OEM\ConsoleMode (registry.rs)
    ("RegOpenKeyW", "registry"),
    // traced only with REBUILD_TRACE_READS (winfiles::trace); otherwise the host's
    ("ReadFile", "files (trace)"),
    ("SetFilePointerEx", "files (trace)"),
    ("RegOpenKeyExW", "registry"),
    ("RegQueryValueExW", "registry"),
    ("RegGetValueW", "registry"),
    ("RegCloseKey", "registry"),
    // memory: the title's allocations must land in watched regions or tiled windows (Builds 1-2)
    ("VirtualAlloc", "memory"),
    ("VirtualAllocEx", "memory"),
    ("VirtualAlloc2", "memory"),
    ("VirtualFree", "memory"),
    ("VirtualFreeEx", "memory"),
    ("VirtualProtect", "memory"),
    ("VirtualQuery", "memory"),
    ("MapViewOfFile", "memory"),
    ("MapViewOfFile3", "memory"),
    ("UnmapViewOfFile", "memory"),
    ("CreateFileMappingW", "memory"),
    // files: console drive letters go through the overlay file system (`crates/vfs`)
    ("CreateFileW", "files"),
    ("CreateFile2", "files"),
    ("CreateFileA", "files"),
    ("DeleteFileW", "files"),
    ("DeleteFileA", "files"),
    ("CreateDirectoryW", "files"),
    ("CreateDirectoryA", "files"),
    ("RemoveDirectoryW", "files"),
    ("MoveFileExW", "files"),
    ("GetFileAttributesW", "files"),
    ("GetFileAttributesA", "files"),
    ("GetFileAttributesExW", "files"),
    ("GetFileAttributesExA", "files"),
    ("FindFirstFileW", "files"),
    ("FindFirstFileExW", "files"),
    ("FindFirstFileA", "files"),
    ("FindFirstFileExA", "files"),
    ("GetFullPathNameW", "files"),
    ("GetFullPathNameA", "files"),
    ("GetDiskFreeSpaceExW", "files"),
    ("GetCurrentDirectoryW", "files"),
    ("SetCurrentDirectoryW", "files"),
    // search handles are ours (directories span overlay, mods and base); the host's would get console paths
    ("FindNextFileW", "files"),
    ("FindNextFileA", "files"),
    ("FindClose", "files"),
    ("SetFileAttributesW", "files"),
    ("GetTempPathW", "files"),
    // F5: copies, links, handle changes and WER registrations name console paths too
    ("CopyFile2", "files"),
    ("CreateHardLinkW", "files"),
    ("CreateSymbolicLinkW", "files"),
    ("WerRegisterFile", "files"),
    ("SetFileInformationByHandle", "files"),
    // crt: the host ucrtbase's own fopen would call the host CreateFileW and miss the overlay; ours opens
    // through the files core and hands the handle back to the same ucrtbase (_open_osfhandle, _fdopen)
    ("fopen", "crt"),
    ("_wfopen", "crt"),
    ("_fsopen", "crt"),
    ("_wfsopen", "crt"),
    ("_open", "crt"),
    ("_wopen", "crt"),
    ("_wstat64", "crt"),
    ("remove", "crt"),
    ("_wremove", "crt"),
    ("_wrename", "crt"),
    ("_wrmdir", "crt"),
    ("_wchdir", "crt"),
    ("_wgetcwd", "crt"),
    // modules: the title's own DLLs, and every miss is a discovery entry
    ("GetProcAddress", "modules"),
    ("LoadLibraryExW", "modules"),
    // the GDK stub loads xgameruntime.dll with the A form: the host would hand it the PC runtime
    ("LoadLibraryExA", "modules"),
    ("LoadLibraryW", "modules"),
    ("LoadLibraryA", "modules"),
    ("LoadPackagedLibrary", "modules"),
    ("GetModuleHandleW", "modules"),
    ("GetModuleHandleA", "modules"),
    ("GetModuleHandleExW", "modules"),
    ("GetModuleFileNameW", "modules"),
    ("GetModuleFileNameA", "modules"),
    ("FreeLibrary", "modules"),
    // process identity: the console title must not see the host's command line or exe path
    ("GetCommandLineW", "process"),
    ("GetCommandLineA", "process"),
];

/// What the GDK runtime noted: interfaces, slots and exports the title asked for that we do not serve.
pub fn gdk_discovery() -> Vec<String> {
    rebuild_gdk::discovery::lines()
}

pub fn override_reason(function: &str) -> Option<&'static str> {
    OVERRIDES.iter().find(|(f, _)| *f == function).map(|(_, why)| *why)
}

/// Classify one import. `host` says whether the host can load the DLL, and whether it exports the
/// function (by name, or by ordinal for ordinal imports).
pub fn classify(function: &str, host_dll: bool, host_function: bool) -> Tier {
    if !host_dll {
        Tier::ConsoleDll
    } else if override_reason(function).is_some() {
        Tier::Override
    } else if host_function {
        Tier::Host
    } else {
        Tier::ConsoleFunction
    }
}

#[cfg(windows)]
pub mod host {
    //! Looking imports up on the host (the same lookups the binding resolver does).

    use std::collections::HashMap;
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::*;

    #[derive(Default)]
    pub struct Host {
        dlls: HashMap<String, Option<HMODULE>>,
    }

    impl Host {
        /// The host's DLL, from System32 only (never the title's folder or the current directory).
        pub fn dll(&mut self, name: &str) -> Option<HMODULE> {
            let key = name.to_ascii_lowercase();
            *self.dlls.entry(key).or_insert_with(|| {
                let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()
            })
        }

        /// The address of a function the host exports, by name or ordinal.
        pub fn function(&mut self, dll: &str, name: Option<&str>, ordinal: Option<u16>) -> Option<u64> {
            let m = self.dll(dll)?;
            unsafe {
                match (name, ordinal) {
                    (Some(n), _) => {
                        let c = std::ffi::CString::new(n).ok()?;
                        GetProcAddress(m, PCSTR(c.as_ptr() as *const u8)).map(|f| f as usize as u64)
                    }
                    (None, Some(o)) => GetProcAddress(m, PCSTR(o as usize as *const u8)).map(|f| f as usize as u64),
                    _ => None,
                }
            }
        }
    }

    /// The binding resolver for the loader: overrides → ours (not built yet: reported missing),
    /// host functions → the host, console-only → missing (the discovery list).
    pub struct HostResolver {
        pub host: Host,
        pub missing: Vec<(String, String, super::Tier)>,
    }

    impl rebuild_loader::Resolver for HostResolver {
        fn resolve(&mut self, dll: &str, symbol: &rebuild_loader::Symbol) -> Option<u64> {
            let (name, ordinal) = match symbol {
                rebuild_loader::Symbol::Name { name, .. } => (Some(name.as_str()), None),
                rebuild_loader::Symbol::Ordinal(o) => (None, Some(*o)),
            };
            let has_dll = self.host.dll(dll).is_some();
            let f = self.host.function(dll, name, ordinal);
            let tier = super::classify(name.unwrap_or(""), has_dll, f.is_some());
            if let (super::Tier::Override, Some(n)) = (tier, name) {
                if let Some(ours) = crate::modules::override_export(n) {
                    return Some(ours);
                }
                // listed as an override but none is built (or it is off, like the read tracer): the host's, not a
                // trap thunk (deepdive.md item 4)
                if f.is_some() {
                    return f;
                }
            }
            if let (super::Tier::ConsoleDll | super::Tier::ConsoleFunction, Some(n)) = (tier, name) {
                if let Some(ours) = crate::exports::console_export(dll, n) {
                    return Some(ours);
                }
            }
            match tier {
                super::Tier::Host => f,
                t => {
                    self.missing.push((dll.to_string(), symbol.to_string(), t));
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_files_and_modules_are_ours_even_though_the_host_has_them() {
        assert_eq!(classify("VirtualAlloc", true, true), Tier::Override);
        assert_eq!(classify("CreateFile2", true, true), Tier::Override);
        assert_eq!(classify("GetProcAddress", true, true), Tier::Override);
        assert_eq!(classify("WaitForSingleObject", true, true), Tier::Host);
        assert_eq!(classify("XMemAlloc", false, false), Tier::ConsoleDll);
        assert_eq!(classify("SomethingXboxOnly", true, false), Tier::ConsoleFunction);
    }

    #[cfg(windows)]
    #[test]
    fn host_lookups_find_real_functions_and_refuse_console_dlls() {
        let mut h = host::Host::default();
        assert!(h.function("api-ms-win-core-synch-l1-1-0.dll", Some("WaitForSingleObject"), None).is_some());
        assert!(h.function("kernel32.dll", Some("GetTickCount64"), None).is_some());
        assert!(h.dll("xmem.dll").is_none());
        assert!(h.function("kernel32.dll", Some("NoSuchFunctionAnywhere"), None).is_none());
    }
}
