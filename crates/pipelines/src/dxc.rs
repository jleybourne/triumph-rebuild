//! Finding DXC, the shader compiler every translated program goes through (`translate::compile_file`).
//!
//! ```text
//! 1. REBUILD_DXC                         a dxc.exe named by hand (dev, tests)
//! 2. bundled, beside the running exe     <exe dir>\dxc\dxc.exe, else <exe dir>\dxc.exe, each with
//!                                        dxcompiler.dll and dxil.dll in the same folder
//! 3. the Windows SDK (dev machines)      the newest <Windows Kits>\10\bin\<version>\x64\dxc.exe
//! ```
//!
//! `dxc.exe` loads `dxcompiler.dll` (the compiler) and `dxil.dll` (the validator, which signs the DXIL; an
//! unsigned container is refused by the runtime) from its own folder. A bundled copy missing either is an
//! error that names the missing file, never a silent fall back to some other DXC. DXC is Microsoft's open
//! source compiler (github.com/microsoft/DirectXShaderCompiler): `dxc.exe` and `dxcompiler.dll` are under the
//! LLVM Release License (University of Illinois/NCSA), `dxil.dll` under Microsoft's redistributable licence
//! that ships with it (`docs/design/shader-library.md`). Nothing of it is in git: a release puts the three
//! files in `dxc\` beside the launcher (`pipelines bundle-dxc <folder>`).

use std::path::{Path, PathBuf};

/// The files a working DXC folder holds.
pub const FILES: [&str; 3] = ["dxc.exe", "dxcompiler.dll", "dxil.dll"];

/// Where a DXC came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Bundled,
    Sdk,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Env => "REBUILD_DXC",
            Source::Bundled => "bundled",
            Source::Sdk => "Windows SDK",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dxc {
    pub exe: PathBuf,
    pub source: Source,
}

/// The files of `FILES` missing beside `exe`.
pub fn missing_beside(exe: &Path) -> Vec<&'static str> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    FILES.iter().copied().filter(|f| !dir.join(f).is_file()).collect()
}

/// A bundled DXC in `dir` (the launcher's folder): `Ok(None)` when there is none at all, `Err` when one is
/// there but incomplete.
pub fn bundled_in(dir: &Path) -> Result<Option<PathBuf>, String> {
    for exe in [dir.join("dxc").join("dxc.exe"), dir.join("dxc.exe")] {
        let folder = exe.parent().unwrap();
        let present = FILES.iter().any(|f| folder.join(f).is_file());
        if !present {
            continue;
        }
        let missing = missing_beside(&exe);
        if !missing.is_empty() {
            return Err(format!("the bundled DXC in {} is incomplete: {} missing (it needs {})", folder.display(), missing.join(", "), FILES.join(", ")));
        }
        return Ok(Some(exe));
    }
    Ok(None)
}

/// The newest `dxc.exe` of the Windows SDK.
pub fn sdk() -> Option<PathBuf> {
    let base = std::env::var_os("ProgramFiles(x86)").map_or_else(|| PathBuf::from(r"C:\Program Files (x86)"), PathBuf::from);
    let root = base.join("Windows Kits").join("10").join("bin");
    let mut v: Vec<PathBuf> = std::fs::read_dir(root).ok()?.filter_map(|e| e.ok().map(|e| e.path().join("x64").join("dxc.exe"))).filter(|p| p.is_file()).collect();
    v.sort();
    v.pop()
}

/// The folder of the running exe.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// DXC by the order above, with `env` for `REBUILD_DXC` and `beside` for the exe's folder (tests pass their
/// own).
pub fn locate_with(env: Option<PathBuf>, beside: Option<&Path>, sdk_too: bool) -> Result<Dxc, String> {
    if let Some(p) = env {
        if !p.is_file() {
            return Err(format!("REBUILD_DXC names {}, which does not exist", p.display()));
        }
        return Ok(Dxc { exe: p, source: Source::Env });
    }
    if let Some(d) = beside {
        if let Some(exe) = bundled_in(d)? {
            return Ok(Dxc { exe, source: Source::Bundled });
        }
    }
    if sdk_too {
        if let Some(exe) = sdk() {
            return Ok(Dxc { exe, source: Source::Sdk });
        }
    }
    Err(match beside {
        Some(d) => format!("no DXC: put {} in {} (the release's dxc folder), install the Windows SDK, or set REBUILD_DXC", FILES.join(", "), d.join("dxc").display()),
        None => "no DXC: install the Windows SDK, or set REBUILD_DXC".into(),
    })
}

/// DXC for this process.
pub fn locate() -> Result<Dxc, String> {
    locate_with(std::env::var_os("REBUILD_DXC").map(PathBuf::from), exe_dir().as_deref(), true)
}

/// What identifies a DXC build: the BLAKE3 over its three files (a missing one counts as empty). Programs
/// compiled by another DXC are compiled again when the library is next rebuilt.
pub fn identity(exe: &Path) -> [u8; 32] {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let mut h = rebuild_shader::blake3::Hasher::new();
    for f in FILES {
        let p = if f == "dxc.exe" { exe.to_path_buf() } else { dir.join(f) };
        h.update(f.as_bytes());
        h.update(&std::fs::read(p).unwrap_or_default());
    }
    *h.finalize().as_bytes()
}

/// Copies the DXC at `from` (its three files) into `to\dxc\`, with a note on where it came from and its
/// licences. For packaging a release; never into git.
pub fn bundle(from: &Path, to: &Path) -> Result<PathBuf, String> {
    let missing = missing_beside(from);
    if !missing.is_empty() {
        return Err(format!("{} lacks {}", from.parent().unwrap_or(Path::new(".")).display(), missing.join(", ")));
    }
    let dir = to.join("dxc");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let src = from.parent().unwrap();
    for f in FILES {
        std::fs::copy(src.join(f), dir.join(f)).map_err(|e| format!("{f}: {e}"))?;
    }
    for lic in ["LICENSE-LLVM.txt", "LICENSE-MIT.txt", "LICENSE-MS.txt"] {
        for cand in [src.join(lic), src.parent().and_then(Path::parent).map(|p| p.join(lic)).unwrap_or_default()] {
            if cand.is_file() {
                let _ = std::fs::copy(&cand, dir.join(lic));
                break;
            }
        }
    }
    let note = format!(
        "DirectX Shader Compiler (DXC), copied from {}\n\n\
         Used by the launcher to build the shader library from your own copy of the game, and by the layer to\n\
         translate programs on demand. Source: https://github.com/microsoft/DirectXShaderCompiler\n\
         dxc.exe and dxcompiler.dll: LLVM Release License (University of Illinois/NCSA Open Source License).\n\
         dxil.dll: Microsoft's DXC redistributable licence (LICENSE-MS.txt in a DXC release).\n",
        src.display()
    );
    std::fs::write(dir.join("README.txt"), note).map_err(|e| e.to_string())?;
    Ok(dir.join("dxc.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rebuild-dxc-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_bundled_copy_beside_the_exe_is_found_and_an_incomplete_one_is_named() {
        let d = temp("bundled");
        assert_eq!(locate_with(None, Some(&d), false).unwrap_err().contains("no DXC: put dxc.exe, dxcompiler.dll, dxil.dll"), true);
        let sub = d.join("dxc");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("dxc.exe"), b"x").unwrap();
        std::fs::write(sub.join("dxcompiler.dll"), b"x").unwrap();
        let e = locate_with(None, Some(&d), true).unwrap_err();
        assert!(e.contains("incomplete") && e.contains("dxil.dll missing"), "{e}");
        std::fs::write(sub.join("dxil.dll"), b"x").unwrap();
        let got = locate_with(None, Some(&d), true).unwrap();
        assert_eq!(got, Dxc { exe: sub.join("dxc.exe"), source: Source::Bundled });
        // REBUILD_DXC wins, and must exist
        assert_eq!(locate_with(Some(sub.join("dxil.dll")), Some(&d), true).unwrap().source, Source::Env);
        assert!(locate_with(Some(d.join("nope.exe")), Some(&d), true).is_err());
        // the identity follows the files
        let a = identity(&sub.join("dxc.exe"));
        std::fs::write(sub.join("dxil.dll"), b"y").unwrap();
        assert_ne!(a, identity(&sub.join("dxc.exe")));
        // bundling copies all three and a note
        let out = temp("bundle-out");
        let exe = bundle(&sub.join("dxc.exe"), &out).unwrap();
        assert_eq!(exe, out.join("dxc").join("dxc.exe"));
        assert!(missing_beside(&exe).is_empty() && out.join("dxc").join("README.txt").is_file());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&out);
    }
}
