//! One console program translated to host DXIL: the `translate` binary's path (`v1`), as a library function the
//! runtime also calls for programs the prebuilt library does not hold (the BVH builder's, embedded in
//! `xboxbvhbuilder.dll`, which no census of `shaders.rpf` sees).
//!
//! ```text
//! container ─ native program (XDAT) ─ decode ─ lift (rebuild_sir::lift, control flow) ─ HLSL ─ dxc (-O3) ─ DXIL
//!                                                                                   └ sampler words, top dword
//! DXIL bindings (PSV0) + sampler words ─ StageLayout ─ the root signature (crate::layout)
//! ```
//!
//! It lives here, not in `crates/sir`, because what it returns is what this crate consumes (a [`StageLayout`]
//! and the sidecar's [`Samplers`]), and because it shells out to `dxc.exe`, which the IR crate should not.
//! The binary and the runtime call the same [`lift_hlsl`] and [`compile_file`], so the runtime's DXIL is
//! byte-identical to what `translate v1` writes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rebuild_shader::features::{self, Findings, WaveClass};
use rebuild_shader::native::{HwStage, NativeProgram, StageIo};
use rebuild_shader::{gfx10, signature, Container, FourCC};

use crate::layout::{StageLayout, USER_DATA_DWORDS};
use crate::sidecar::Samplers;
use crate::Stage;

/// The DXC arguments every translated program is compiled with (after `-T <stage>_6_6`).
pub const DXC_ARGS: [&str; 5] = ["-E", "main", "-O3", "-enable-16bit-types", "-Fo"];

/// `dxc.exe`: `REBUILD_DXC`, else the newest in the Windows SDK.
pub fn dxc_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("REBUILD_DXC") {
        return Some(p.into());
    }
    let root = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut v: Vec<_> = std::fs::read_dir(root).ok()?.filter_map(|e| e.ok().map(|e| e.path().join("x64").join("dxc.exe"))).filter(|p| p.exists()).collect();
    v.sort();
    v.pop()
}

/// The program hash of a console container: the first 8 bytes (big-endian) of the BLAKE3 of the container
/// (its declared size; bytes past it do not count), as the census and the program library name it.
pub use crate::stream::program_hash;

/// The container's own bytes (its declared size).
pub fn container_bytes(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 0x20 || &bytes[..4] != b"DXBC" {
        return None;
    }
    let total = u32::from_le_bytes(bytes[0x18..0x1c].try_into().ok()?) as usize;
    bytes.get(..total)
}

/// The host stage a native program translates to (`None`: not one this translator makes).
pub fn host_stage(p: &NativeProgram) -> Option<Stage> {
    match (&p.io, p.stage) {
        (StageIo::Cs(_), _) => Some(Stage::Cs),
        (StageIo::Ps { .. }, _) => Some(Stage::Ps),
        (StageIo::Vs { .. }, HwStage::Vs) => Some(Stage::Vs),
        _ => None,
    }
}

/// A program lifted and assembled, before DXC.
#[derive(Debug, Clone)]
pub struct Hlsl {
    pub stage: Stage,
    pub src: String,
    /// The immediate sampler words and comparison mask (`top_dword` left `None`: the sidecar's optional word
    /// is not written yet, so `translate`'s sidecars stay as they were).
    pub samplers: Samplers,
    /// The highest user-data dword the program reads (heap bases excluded), if any.
    pub top_dword: Option<u32>,
    /// Every descriptor read and its view (`rebuild_sir::lift::Lifted::desc_uses`): what `execcheck` builds a
    /// heap from.
    pub uses: Vec<rebuild_sir::lift::res::DescUse>,
}

/// Why a program was not assembled; `body` is the lifted text so far (for `REBUILD_TRANSLATE_DUMP`).
#[derive(Debug, Clone)]
pub struct Refused {
    pub reason: String,
    pub body: String,
}

impl Refused {
    fn new(reason: impl Into<String>) -> Refused {
        Refused { reason: reason.into(), body: String::new() }
    }
}

/// Lifts and assembles one native program (`v1`: with control flow, through `lift_program`; else the v0
/// straight-line lifter). `isg1`/`osg1` are the container's signature parts (unused for compute). The caller
/// decides which programs to try (`translate` keeps its own filters; [`translate_native`] refuses non-agnostic
/// ones).
pub fn lift_hlsl(p: &NativeProgram, isg1: &[u8], osg1: &[u8], v1: bool) -> Result<Hlsl, Refused> {
    use rebuild_sir::lift;
    let stage = host_stage(p).ok_or_else(|| Refused::new(format!("a {:?} program is not translated", p.stage)))?;
    let bind = lift::Bindings { hw: p.stage, layout: &p.layout, used: &p.used };
    let insts = gfx10::decode(&p.patched_code()).map_err(|_| Refused::new("patched code does not decode"))?;
    let wave64 = p.wave64;
    // how console lanes map to host lanes, by the program's wave class (agnostic programs: as before)
    let wave = wave_of(p, &insts);
    let run = |st| if v1 { lift::lift_program_wave(&insts, st, Some(bind), wave64, wave) } else { lift::lift_stage(&insts, st, Some(bind)) };
    // compute programs have no signatures
    let sig = |b: &[u8]| if stage == Stage::Cs { Ok(Vec::new()) } else { signature::parse(b) };
    let (Ok(isg1), Ok(osg1)) = (sig(isg1), sig(osg1)) else { return Err(Refused::new("signature unreadable")) };
    let (l, result) = match &p.io {
        StageIo::Ps { inputs, input_addr, .. } => {
            let st = || lift::PsStage { inputs, input_addr: *input_addr };
            let l = run(lift::Stage::Ps(st()));
            let r = lift::ps_program(&l, &st(), Some(&bind), &isg1, &osg1);
            (l, r)
        }
        StageIo::Vs { inputs, outputs } => {
            let st = || lift::VsStage { inputs, outputs };
            let l = run(lift::Stage::Vs(st()));
            let r = lift::vs_program(&l, &st(), Some(&bind), &isg1, &osg1);
            (l, r)
        }
        StageIo::Cs(io) => {
            let l = run(lift::Stage::Cs(*io));
            let r = lift::cs_program(&l, io, Some(&bind));
            (l, r)
        }
        _ => unreachable!("host_stage checked the stage"),
    };
    let src = result.map_err(|reason| Refused { reason, body: l.body.clone() })?;
    let top_dword = l.live_in.iter().filter(|s| !bind.is_heap_base(**s)).filter_map(|s| bind.user_dword(*s)).max().map(u32::from);
    let compare = l.immediate_compare.iter().filter(|k| **k < 32).fold(0u32, |m, k| m | 1 << k);
    Ok(Hlsl { stage, src, samplers: Samplers { words: l.immediate_samplers, compare, top_dword: None }, top_dword, uses: l.desc_uses })
}

/// DXC processes this process runs at once, at most (the owner's rule: the machine stays responsive).
pub const MAX_DXC: usize = 4;

static DXC_RUNNING: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static DXC_FREE: std::sync::Condvar = std::sync::Condvar::new();

/// One of the [`MAX_DXC`] slots, given back on drop.
struct DxcSlot;

impl DxcSlot {
    fn take() -> DxcSlot {
        let mut n = DXC_RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= MAX_DXC {
            n = DXC_FREE.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        DxcSlot
    }
}

impl Drop for DxcSlot {
    fn drop(&mut self) {
        *DXC_RUNNING.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        DXC_FREE.notify_one();
    }
}

/// Compiles `hlsl` (a file) to `out` with DXC, as every translated program is compiled. `Err`: DXC's first
/// error line. A failure with no error line (DXC itself died: 6 of 7,402 in one corpus run under load, each
/// compiling clean when run again) is tried once more.
pub fn compile_file(dxc: &Path, stage: Stage, hlsl: &Path, out: &Path) -> Result<(), String> {
    let _slot = DxcSlot::take();
    let mut last = String::new();
    for _ in 0..2 {
        let r = std::process::Command::new(dxc)
            .args(["-T", &format!("{}_6_6", stage.name())])
            .args(DXC_ARGS)
            .arg(out)
            .arg(hlsl)
            .output()
            .map_err(|e| format!("run {}: {e}", dxc.display()))?;
        if r.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&r.stderr);
        if let Some(l) = err.lines().find(|l| l.contains("error")) {
            return Err(l.to_string());
        }
        last = format!("dxc failed (exit {:?}, no error line)", r.status.code());
    }
    Err(last)
}

/// Where the time went.
#[derive(Debug, Clone, Copy, Default)]
pub struct Times {
    /// Container parse, native program, wave class.
    pub parse: Duration,
    /// Decode, lift and HLSL assembly.
    pub lift: Duration,
    /// DXC (process start, compile, sign), and reading its output.
    pub dxc: Duration,
}

/// One translated program, with what the program library would hold for it.
#[derive(Debug, Clone)]
pub struct Translated {
    pub hash: u64,
    pub stage: Stage,
    pub hlsl: Hlsl,
    pub dxil: Vec<u8>,
    /// Its root layout (from the DXIL's bindings and the sampler words): `RootDesc::compute(&layout, 16)` is
    /// its root signature.
    pub layout: StageLayout,
    pub times: Times,
}

/// The first native program of a container this translator makes, and the container's signature parts.
pub fn native_of(bytes: &[u8]) -> Result<(NativeProgram, Vec<u8>, Vec<u8>), String> {
    let bytes = container_bytes(bytes).ok_or("not a DXBC container")?;
    let c = Container::parse(bytes).map_err(|e| e.to_string())?;
    let dxil = c.part(FourCC::DXIL).ok_or("no DXIL part")?;
    let list = rebuild_shader::native_programs(dxil.data).map_err(|e| e.to_string())?;
    let p = list.into_iter().find(|p| host_stage(p).is_some()).ok_or("no CS, PS or hardware VS program in the container")?;
    let part = |f| c.part(f).map(|p| p.data.to_vec()).unwrap_or_default();
    Ok((p, part(FourCC::ISG1), part(FourCC::OSG1)))
}

/// The wave mapping of a program (`rebuild_sir::lift::Wave::choose`): agnostic programs stay one console lane per
/// host lane; wave-wide ones run native wave32, as a partly filled wave64 (graphics) or doubled (compute).
pub fn wave_of(p: &NativeProgram, insts: &[gfx10::Instruction]) -> rebuild_sir::lift::Wave {
    let class = features::analyse(p, &mut Findings::default()).class();
    rebuild_sir::lift::Wave::choose(class, p.wave64, matches!(p.io, StageIo::Cs(_)), insts)
}

/// Refuses what `translate v1` refuses before lifting: undecodable code. Every wave class is lifted (wave-wide
/// programs through `rebuild_sir::lift::wave`); a program the lifter cannot finish is refused there.
pub fn check_kind(p: &NativeProgram) -> Result<(), String> {
    let row = features::analyse(p, &mut Findings::default());
    if let Some(e) = &row.decode_error {
        return Err(format!("does not decode: {e}"));
    }
    let _: WaveClass = row.class();
    Ok(())
}

/// The layout of a compiled program, refusing user-data reads past the root constants.
pub fn layout_of(dxil: &[u8], h: &Hlsl) -> Result<StageLayout, String> {
    let c = crate::bindings::read(dxil)?;
    let l = StageLayout::from_bindings(&c.bindings, &h.samplers.words, h.samplers.compare)?;
    if let Some(d) = h.top_dword.filter(|d| *d >= USER_DATA_DWORDS) {
        if l.user_data {
            return Err(format!("reads user-data dword {d}, past the {USER_DATA_DWORDS} root constants"));
        }
    }
    Ok(l)
}

/// Translates one console container: the `translate v1` path, compiled with `dxc` in `work` (a folder for the
/// HLSL and DXIL files, which are removed after).
pub fn translate_container_with(bytes: &[u8], dxc: &Path, work: &Path) -> Result<Translated, String> {
    let t = Instant::now();
    let hash = program_hash(bytes).ok_or("not a DXBC container")?;
    let (p, isg1, osg1) = native_of(bytes)?;
    check_kind(&p)?;
    let parse = t.elapsed();
    let t = Instant::now();
    let hlsl = lift_hlsl(&p, &isg1, &osg1, true).map_err(|r| r.reason)?;
    let lift = t.elapsed();
    let t = Instant::now();
    let dxil = compile_hlsl(dxc, work, hash, &hlsl)?;
    let dxc = t.elapsed();
    let layout = layout_of(&dxil, &hlsl)?;
    Ok(Translated { hash, stage: hlsl.stage, hlsl, dxil, layout, times: Times { parse, lift, dxc } })
}

/// [`translate_container_with`] with [`dxc_path`] and a folder in the temp directory.
pub fn translate_container(bytes: &[u8]) -> Result<Translated, String> {
    let dxc = dxc_path().ok_or("no dxc.exe (Windows SDK, or REBUILD_DXC)")?;
    translate_container_with(bytes, &dxc, &std::env::temp_dir().join(format!("rebuild-translate-{}", std::process::id())))
}

/// Compiles assembled HLSL through files in `work` (named by the program, so parallel calls never collide).
pub fn compile_hlsl(dxc: &Path, work: &Path, hash: u64, h: &Hlsl) -> Result<Vec<u8>, String> {
    std::fs::create_dir_all(work).map_err(|e| format!("{}: {e}", work.display()))?;
    let name = format!("{hash:016x}.{}", h.stage.name());
    let (hl, obj) = (work.join(format!("{name}.hlsl")), work.join(format!("{name}.dxil")));
    std::fs::write(&hl, &h.src).map_err(|e| format!("{}: {e}", hl.display()))?;
    let r = compile_file(dxc, h.stage, &hl, &obj).and_then(|_| std::fs::read(&obj).map_err(|e| format!("{}: {e}", obj.display())));
    let _ = std::fs::remove_file(&hl);
    let _ = std::fs::remove_file(&obj);
    r
}

/// Compute containers in an archive (a copy of `shaders.rpf`, never the game folder), in file order, each once:
/// `(hash, container)`, until `want` accepts `limit` of them. For tests and benches.
pub fn compute_containers(rpf: &[u8], limit: usize, want: impl Fn(u64) -> bool) -> Vec<(u64, Vec<u8>)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rebuild_shader::sgd5::records(rpf) {
        if out.len() >= limit {
            break;
        }
        let Some(bytes) = r.unpack() else { continue };
        let Some(h) = program_hash(&bytes) else { continue };
        if !seen.insert(h) || !want(h) {
            continue;
        }
        if native_of(&bytes).is_ok_and(|(p, _, _)| host_stage(&p) == Some(Stage::Cs)) {
            out.push((h, container_bytes(&bytes).unwrap().to_vec()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus copy (never the game folder) and the `translate v1` output to compare with.
    fn corpus() -> Option<(Vec<u8>, PathBuf)> {
        let rpf = std::env::var_os("REBUILD_SHADERS").map_or_else(|| PathBuf::from(r"D:\victory-data\shaders.rpf"), PathBuf::from);
        let out = std::env::var_os("REBUILD_TRANSLATE_OUT").map_or_else(|| PathBuf::from(r"D:\victory-data\translate"), PathBuf::from);
        match std::fs::read(&rpf) {
            Ok(b) => Some((b, out)),
            Err(e) => {
                eprintln!("skipped: no corpus copy at {}: {e}", rpf.display());
                None
            }
        }
    }

    #[test]
    fn hashes_and_refusals() {
        let mut c = vec![0u8; 0x40];
        c[..4].copy_from_slice(b"DXBC");
        c[0x18..0x1c].copy_from_slice(&0x20u32.to_le_bytes());
        let want = u64::from_be_bytes(rebuild_shader::blake3::hash(&c[..0x20]).as_bytes()[..8].try_into().unwrap());
        assert_eq!(program_hash(&c), Some(want), "bytes past the container's size do not count");
        assert_eq!(program_hash(b"DXIL"), None);
        assert!(native_of(b"nope").unwrap_err().contains("not a DXBC"));
    }

    /// Compute containers from the corpus: each translated by [`translate_container`] gives the bytes
    /// `translate v1` wrote for it (`<hash>.cs.dxil`), and a layout the library would accept.
    #[test]
    fn a_container_translates_to_what_translate_v1_writes() {
        if Path::new(r"D:\Developer\triumph-rebuild\agents\MEASURING").exists() {
            eprintln!("skipped: the measurement lock is present");
            return;
        }
        let Some((rpf, out)) = corpus() else { return };
        let Some(dxc) = dxc_path() else {
            eprintln!("skipped: no dxc.exe");
            return;
        };
        let work = std::env::temp_dir().join(format!("rebuild-translate-test-{}", std::process::id()));
        let mut compared = 0;
        // the first compute programs translate v1 compiled, in file order
        for (h, bytes) in compute_containers(&rpf, 4, |h| out.join(format!("{h:016x}.cs.dxil")).exists()) {
            let want = std::fs::read(out.join(format!("{h:016x}.cs.dxil"))).unwrap();
            let t = translate_container_with(&bytes, &dxc, &work).unwrap();
            assert_eq!((t.hash, t.stage), (h, Stage::Cs));
            assert!(t.dxil == want, "{h:016x}: the DXIL differs from translate v1's ({} vs {} bytes)", t.dxil.len(), want.len());
            let side = std::fs::read(out.join(format!("{h:016x}.cs.samplers"))).unwrap();
            assert_eq!(crate::sidecar::encode(&t.hlsl.samplers), side, "{h:016x}: the sampler sidecar");
            crate::layout::RootDesc::compute(&t.layout, USER_DATA_DWORDS).check().unwrap();
            compared += 1;
        }
        let _ = std::fs::remove_dir_all(&work);
        if compared == 0 {
            eprintln!("skipped: no translate v1 compute output in {}", out.display());
        }
    }
}
