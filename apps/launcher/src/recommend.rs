//! The settings recommended for this PC (owner request, 2026-11-28: "recommend a profile to run the game at
//! based on the specs of the machine automatically"). Design and the mapping table:
//! `docs/design/launcher.md`, "Recommended for this PC".
//!
//! - [`detect`] reads the machine (Windows): the hardware GPU with the most dedicated VRAM through DXGI
//!   (software and WARP adapters skipped, **no device created**), installed RAM, logical and physical
//!   cores, and the primary display's resolution.
//! - [`recommend`] is a pure function from [`Specs`] to a [`Recommendation`], unit-tested with a table of
//!   machines.
//! - A recommendation is **not a test profile**. It sets the same eight keys a built-in does
//!   ([`rebuild_config::profiles::PROFILE_KEYS`]) so nothing of a test profile is left behind, but every
//!   `limits.*` is 0: a limit only ever slows the game, so a real player never gets one.

use crate::model::Model;
use crate::store::Store;
use rebuild_config::profiles::PROFILE_KEYS;
use rebuild_config::{Config, Value};

/// `profile` in `rebuild.toml` once a recommendation is applied.
pub const PROFILE: &str = "recommended";
/// Top-level key `rebuild.toml` gets when the launcher applied the recommendation by itself, so it does so
/// once per game and never again (whatever the user does afterwards).
pub const FIRST_RUN_KEY: &str = "first_run_recommendation";

const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    pub name: String,
    pub vendor_id: u32,
    pub vram_bytes: u64,
}

/// What [`detect`] found. Anything it could not read is `None` / 0 and is left out of the decision.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Specs {
    /// the hardware adapter with the most dedicated VRAM; `None` when only a software adapter exists
    pub gpu: Option<Gpu>,
    /// installed RAM
    pub ram_bytes: u64,
    pub logical: u32,
    pub physical: u32,
    /// the primary display's current mode, in pixels
    pub display: Option<(u32, u32)>,
}

/// A GPU's class, lowest first. Each is the output and internal resolution it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// integrated GPUs and cards under ~6 GB: 720p, Performance (640 x 360 internal)
    Lowest,
    /// ~6 GB cards (RTX 2060/3060, RX 5600 XT/6600): 1080p, Quality (1280 x 720 internal)
    P1080,
    /// ~8 GB cards (RTX 3070/4060 Ti, RX 6800): 1440p, Quality (1707 x 960 internal)
    P1440,
    /// the minimum spec and above (RTX 4070 Ti, 12 GB): 4K, Quality (2560 x 1440 internal, the console's own)
    P2160,
}

impl Class {
    /// (output width, height, mode at that output)
    fn target(self) -> (u32, u32, &'static str) {
        match self {
            Class::Lowest => (1280, 720, "performance"),
            Class::P1080 => (1920, 1080, "quality"),
            Class::P1440 => (2560, 1440, "quality"),
            Class::P2160 => (3840, 2160, "quality"),
        }
    }

    /// The internal pixels a class carries: its output through its mode.
    fn budget(self) -> u64 {
        let (w, h, m) = self.target();
        let (iw, ih) = internal(w, h, m);
        iw as u64 * ih as u64
    }

    pub fn label(self) -> &'static str {
        match self {
            Class::Lowest => "integrated or entry-level GPU",
            Class::P1080 => "entry GPU (about 6 GB)",
            Class::P1440 => "mid-range GPU (about 8 GB)",
            Class::P2160 => "at or above the minimum spec (RTX 4070 Ti class, 12 GB)",
        }
    }

    fn down(self) -> Class {
        match self {
            Class::P2160 => Class::P1440,
            Class::P1440 => Class::P1080,
            _ => Class::Lowest,
        }
    }
}

/// The internal resolution an upscaler mode renders at for an output (DLSS's and FSR's published
/// ratios: DLAA / native AA 1.0, Quality 1.5, Balanced 1.7, Performance 2.0, Ultra Performance 3.0).
pub fn internal(w: u32, h: u32, mode: &str) -> (u32, u32) {
    let k = match mode {
        "dlaa" => 1.0,
        "quality" => 1.5,
        "balanced" => 1.7,
        "performance" => 2.0,
        _ => 3.0,
    };
    ((w as f64 / k).round() as u32, (h as f64 / k).round() as u32)
}

/// Modes from the most internal pixels to the fewest.
const MODES: [&str; 5] = ["dlaa", "quality", "balanced", "performance", "ultra_performance"];
/// 16:9 outputs, largest first.
const OUTPUTS: [(u32, u32); 5] = [(3840, 2160), (2560, 1440), (1920, 1080), (1600, 900), (1280, 720)];

#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    pub class: Class,
    pub width: u32,
    pub height: u32,
    /// "dlss" (an NVIDIA RTX card) or "fsr" (everything else)
    pub backend: &'static str,
    pub mode: &'static str,
    pub internal: (u32, u32),
    /// the detected specs, one line
    pub specs_line: String,
    /// why this, one line
    pub why: String,
    /// things the recommendation can't fix (RAM, CPU), one line each
    pub notes: Vec<String>,
}

impl Recommendation {
    /// "4K · DLSS Quality", "1440p · DLSS DLAA", "1080p · FSR Quality".
    pub fn name(&self) -> String {
        let out = match (self.width, self.height) {
            (3840, 2160) => "4K".to_string(),
            (2560, 1440) => "1440p".into(),
            (1920, 1080) => "1080p".into(),
            (1600, 900) => "900p".into(),
            (1280, 720) => "720p".into(),
            (w, h) => format!("{w}×{h}"),
        };
        format!("{out} · {} {}", self.backend.to_uppercase(), mode_name(self.backend, self.mode))
    }

    /// The eight profile keys: output, upscaler, and every limit 0.
    pub fn values(&self) -> Vec<(&'static str, Value)> {
        PROFILE_KEYS
            .iter()
            .map(|k| {
                let v = match *k {
                    "display.width" => Value::Int(self.width as i64),
                    "display.height" => Value::Int(self.height as i64),
                    "upscaler.backend" => Value::Choice(self.backend.into()),
                    "upscaler.mode" => Value::Choice(self.mode.into()),
                    _ => Value::Int(0), // limits.*: a real player gets none
                };
                (*k, v)
            })
            .collect()
    }

    /// Set the eight keys (they become explicit) and `profile`; every other setting is kept. All or nothing.
    pub fn apply(&self, m: &mut Model) -> Result<(), String> {
        let mut next = m.config.clone();
        for (k, v) in self.values() {
            next.set(k, v)?;
        }
        next.profile = PROFILE.into();
        m.config = next;
        for k in PROFILE_KEYS {
            if let Some(s) = rebuild_config::find(k) {
                m.explicit.insert(s.key);
            }
        }
        Ok(())
    }

    /// Every value of the recommendation is in force.
    pub fn matches(&self, c: &Config) -> bool {
        self.values().iter().all(|(k, v)| c.get(k) == *v)
    }
}

/// An upscaler mode as players know it.
pub fn mode_name(backend: &str, mode: &str) -> &'static str {
    match mode {
        "dlaa" if backend == "dlss" => "DLAA",
        "dlaa" => "Native AA",
        "quality" => "Quality",
        "balanced" => "Balanced",
        "performance" => "Performance",
        _ => "Ultra Performance",
    }
}

/// The GPU's name without the trademark marks, lower case, single-spaced.
fn normalise(name: &str) -> String {
    let n = name.to_lowercase().replace("(tm)", " ").replace("(r)", " ").replace(['™', '®'], " ");
    n.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Known models, most specific first (`"rtx 4070 ti"` before `"rtx 4070"`). A match caps the class the VRAM
/// gives, so a card with a lot of VRAM and a small chip (RTX 3060 12 GB, Arc A770 16 GB) isn't over-rated.
/// Relative to the minimum spec (RTX 4070 Ti): roughly ≥ 0.9× → 4K, ≥ 0.5× → 1440p, ≥ 0.3× → 1080p.
const MODELS: &[(&str, Class)] = &[
    // NVIDIA
    ("rtx 5090", Class::P2160),
    ("rtx 5080", Class::P2160),
    ("rtx 5070 ti", Class::P2160),
    ("rtx 4090", Class::P2160),
    ("rtx 4080", Class::P2160),
    ("rtx 4070 ti", Class::P2160),
    ("rtx 3090", Class::P2160),
    ("rtx 5070", Class::P1440),
    ("rtx 4070", Class::P1440),
    ("rtx 5060 ti", Class::P1440),
    ("rtx 4060 ti", Class::P1440),
    ("rtx 3080", Class::P1440),
    ("rtx 3070", Class::P1440),
    ("rtx 2080 ti", Class::P1440),
    ("rtx 5060", Class::P1080),
    ("rtx 4060", Class::P1080),
    ("rtx 3060", Class::P1080),
    ("rtx 2080", Class::P1080),
    ("rtx 2070", Class::P1080),
    ("rtx 2060", Class::P1080),
    ("gtx 1080", Class::P1080),
    ("gtx 1070", Class::P1080),
    ("rtx 3050", Class::Lowest),
    ("gtx 16", Class::Lowest),
    ("gtx 10", Class::Lowest),
    ("gtx 9", Class::Lowest),
    // AMD
    ("rx 9070 xt", Class::P2160),
    ("rx 7900 xtx", Class::P2160),
    ("rx 7900 xt", Class::P2160),
    ("rx 9070", Class::P1440),
    ("rx 7900", Class::P1440),
    ("rx 6950", Class::P1440),
    ("rx 6900", Class::P1440),
    ("rx 6800", Class::P1440),
    ("rx 7800", Class::P1440),
    ("rx 7700", Class::P1440),
    ("rx 9060 xt", Class::P1440),
    ("rx 6750", Class::P1440),
    ("rx 6700 xt", Class::P1440),
    ("rx 6700", Class::P1080),
    ("rx 9060", Class::P1080),
    ("rx 7600", Class::P1080),
    ("rx 6650", Class::P1080),
    ("rx 6600", Class::P1080),
    ("rx 5700", Class::P1080),
    ("rx 5600", Class::P1080),
    ("rx 6500", Class::Lowest),
    ("rx 6400", Class::Lowest),
    ("rx 5500", Class::Lowest),
    ("rx 5", Class::Lowest),
    ("vega", Class::Lowest),
    // Intel
    ("arc b580", Class::P1080),
    ("arc b570", Class::P1080),
    ("arc a770", Class::P1080),
    ("arc a750", Class::P1080),
    ("arc a580", Class::Lowest),
    ("arc a380", Class::Lowest),
    ("arc a310", Class::Lowest),
];

/// The class a GPU's dedicated VRAM allows (DXGI reports a 12 GB card as a little under 12 GiB, hence the
/// half-gigabyte slack). Under 2 GiB is an integrated GPU's carve-out.
pub fn vram_class(bytes: u64) -> Class {
    let gib = bytes as f64 / GIB as f64;
    if gib >= 11.5 {
        Class::P2160
    } else if gib >= 7.5 {
        Class::P1440
    } else if gib >= 5.5 {
        Class::P1080
    } else {
        Class::Lowest
    }
}

/// A known model's class, one lower for a laptop part.
pub fn model_class(name: &str) -> Option<Class> {
    let n = normalise(name);
    let c = MODELS.iter().find(|(m, _)| contains_model(&n, m)).map(|(_, c)| *c)?;
    let laptop = ["laptop", "mobile", "max-q", "max-p"].iter().any(|w| n.contains(w));
    Some(if laptop { c.down() } else { c })
}

/// `m` in `n` ending at a word boundary, so "rx 5" doesn't match "rx 5700" too early in the list
/// (it is listed after them) and "rtx 4070" never matches "rtx 40700".
fn contains_model(n: &str, m: &str) -> bool {
    let mut from = 0;
    while let Some(i) = n[from..].find(m) {
        let end = from + i + m.len();
        // "gtx 16" and "rx 5" are prefixes on purpose: a number that continues is still the family
        let family = m.ends_with(|c: char| c.is_ascii_digit()) && m.split(' ').next_back().is_some_and(|d| d.len() < 4);
        if family || n[end..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric()) {
            return true;
        }
        from = end;
    }
    false
}

/// An integrated GPU: almost no dedicated VRAM, or Intel's integrated names.
fn integrated(g: &Gpu) -> bool {
    let n = normalise(&g.name);
    g.vram_bytes < 2 * GIB || (g.vendor_id == 0x8086 && (n.contains("uhd") || n.contains("iris") || n.contains("hd graphics")))
}

/// DLSS needs an NVIDIA RTX card (tensor cores); a GTX gets FSR.
fn backend_for(g: Option<&Gpu>) -> &'static str {
    match g {
        Some(g) if g.vendor_id == 0x10de && normalise(&g.name).contains("rtx") => "dlss",
        _ => "fsr",
    }
}

/// The class of a GPU, and the reason in a few words.
pub fn gpu_class(g: Option<&Gpu>) -> (Class, String) {
    let Some(g) = g else { return (Class::Lowest, "no hardware GPU found".into()) };
    if integrated(g) {
        return (Class::Lowest, "integrated GPU".into());
    }
    let by_vram = vram_class(g.vram_bytes);
    match model_class(&g.name) {
        Some(m) if m < by_vram => (m, format!("{} by its model (its VRAM alone would allow more)", m.label())),
        _ => (by_vram, by_vram.label().to_string()),
    }
}

pub fn gb(bytes: u64) -> String {
    let g = bytes as f64 / GIB as f64;
    if g >= 1.0 {
        format!("{} GB", g.round() as u64)
    } else {
        format!("{} MB", (bytes >> 20).max(1))
    }
}

/// The detected specs, one line: "NVIDIA GeForce RTX 4070 Ti · 12 GB VRAM · 64 GB RAM · 8 cores / 16
/// threads · 3840×2160 display".
pub fn specs_line(s: &Specs) -> String {
    let mut v = Vec::new();
    match &s.gpu {
        Some(g) => {
            v.push(g.name.clone());
            v.push(format!("{} VRAM", gb(g.vram_bytes)));
        }
        None => v.push("no hardware GPU".into()),
    }
    if s.ram_bytes > 0 {
        v.push(format!("{} RAM", gb(s.ram_bytes)));
    }
    match (s.physical, s.logical) {
        (0, 0) => {}
        (0, l) => v.push(format!("{l} threads")),
        (p, l) if p == l => v.push(format!("{p} cores")),
        (p, l) => v.push(format!("{p} cores / {l} threads")),
    }
    if let Some((w, h)) = s.display {
        v.push(format!("{w}×{h} display"));
    }
    v.join(" · ")
}

/// What to run this machine at. Pure: the same specs always give the same answer.
///
/// 1. The GPU's [`Class`]: dedicated VRAM, capped by the model when it is a known one (one class lower for
///    a laptop part); integrated GPUs and machines with no hardware GPU are [`Class::Lowest`].
/// 2. The output: the class's output, or the largest 16:9 size that fits the display when the display is
///    smaller (never above the display).
/// 3. The mode: the one with the most internal pixels within what the class carries, so a strong GPU on a
///    small display renders sharper (DLAA at 1440p for a 4070 Ti) rather than idling.
/// 4. The backend: DLSS on an NVIDIA RTX card, FSR otherwise. Limits are all 0.
pub fn recommend(s: &Specs) -> Recommendation {
    let (class, class_why) = gpu_class(s.gpu.as_ref());
    let (tw, th, _) = class.target();
    let (width, height) = match s.display {
        Some((dw, dh)) if dw < tw || dh < th => OUTPUTS.iter().copied().find(|&(w, h)| w <= tw && w <= dw && h <= dh).unwrap_or_else(|| fit_16_9(dw, dh)),
        _ => (tw, th),
    };
    let budget = class.budget() as f64 * 1.01;
    let mode = MODES
        .iter()
        .copied()
        .find(|m| {
            let (iw, ih) = internal(width, height, m);
            (iw as u64 * ih as u64) as f64 <= budget
        })
        .unwrap_or("ultra_performance");
    let backend = backend_for(s.gpu.as_ref());
    let internal = internal(width, height, mode);
    let capped = (width, height) != (tw, th);
    let mut why = format!("{}{class_why}: ", if class < Class::P2160 { "below the minimum spec, " } else { "" });
    why += &format!("{width}×{height} with {} {} ({}×{} internal)", backend.to_uppercase(), mode_name(backend, mode), internal.0, internal.1);
    if capped {
        why += ", the display's size";
    } else if class == Class::P2160 && mode == "quality" {
        why += ", the console's own internal resolution";
    }
    why = why[..1].to_uppercase() + &why[1..];
    let mut notes = Vec::new();
    if s.gpu.as_ref().is_some_and(|g| g.vendor_id == 0x10de) && backend == "fsr" {
        notes.push("This NVIDIA card has no tensor cores, so FSR rather than DLSS.".to_string());
    }
    if s.ram_bytes > 0 && s.ram_bytes < 15 * GIB {
        notes.push(format!("{} of RAM: the title alone is given 13.5 GiB on the console; expect paging.", gb(s.ram_bytes)));
    } else if s.ram_bytes > 0 && s.ram_bytes < 31 * GIB {
        notes.push(format!("{} of RAM (the reference machine has 64 GB): the title alone is given up to 13.5 GiB.", gb(s.ram_bytes)));
    }
    if s.logical > 0 && s.logical < 12 {
        notes.push(format!("{} threads: the title was written for 14 (7 console cores × 2); frames may be CPU-bound at any resolution.", s.logical));
    }
    Recommendation { class, width, height, backend, mode, internal, specs_line: specs_line(s), why, notes }
}

/// The largest even 16:9 size inside a display smaller than every listed output, at least 640 x 360 (the
/// registry's floor).
fn fit_16_9(dw: u32, dh: u32) -> (u32, u32) {
    let w = dw.min(dh * 16 / 9).max(640) / 16 * 16;
    (w, w * 9 / 16)
}

/// The launcher applies the recommendation by itself only to a game whose settings have never chosen an
/// output or upscaler: no profile key is explicit, the profile is the default one, and it hasn't done so
/// for this game before. Anything the user set is never overwritten.
pub fn first_run(store: &Store) -> bool {
    let c = &store.model.config;
    store.top_str(FIRST_RUN_KEY).is_none()
        && (c.profile.is_empty() || c.profile == "default")
        && !PROFILE_KEYS.iter().any(|k| store.model.explicit.contains(k))
}

/// Apply on first run: the recommendation, and the marker that it was done.
pub fn apply_first_run(store: &mut Store, r: &Recommendation) -> Result<(), String> {
    r.apply(&mut store.model)?;
    store.set_top_str(FIRST_RUN_KEY, &r.name());
    Ok(())
}

/// The machine, as Windows reports it. Creates no D3D12 device (DXGI only), so it is safe while a
/// measurement runs.
pub fn detect() -> Specs {
    #[cfg(windows)]
    {
        win::specs()
    }
    #[cfg(not(windows))]
    {
        Specs { logical: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0), ..Default::default() }
    }
}

#[cfg(windows)]
mod win {
    use super::{Gpu, Specs};
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    use windows::Win32::System::SystemInformation::*;

    pub fn specs() -> Specs {
        let (physical, logical) = cores();
        Specs { gpu: gpu(), ram_bytes: ram(), logical, physical, display: display() }
    }

    /// Every hardware adapter (software and the Basic Render Driver skipped); the one with the most VRAM.
    fn gpu() -> Option<Gpu> {
        unsafe {
            let f: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut best: Option<Gpu> = None;
            for i in 0..16 {
                let Ok(a) = f.EnumAdapters1(i) else { break };
                let Ok(d) = a.GetDesc1() else { continue };
                let software = (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 || (d.VendorId == 0x1414 && d.DeviceId == 0x8c);
                if software {
                    continue;
                }
                let len = d.Description.iter().position(|c| *c == 0).unwrap_or(d.Description.len());
                let g = Gpu { name: String::from_utf16_lossy(&d.Description[..len]).trim().to_string(), vendor_id: d.VendorId, vram_bytes: d.DedicatedVideoMemory as u64 };
                if best.as_ref().is_none_or(|b| g.vram_bytes > b.vram_bytes) {
                    best = Some(g);
                }
            }
            best
        }
    }

    /// Installed RAM (what the machine has, not what Windows has left); the usable total as a fallback.
    fn ram() -> u64 {
        unsafe {
            let mut kb = 0u64;
            if GetPhysicallyInstalledSystemMemory(&mut kb).is_ok() && kb > 0 {
                return kb * 1024;
            }
            let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
            if GlobalMemoryStatusEx(&mut m).is_ok() {
                return m.ullTotalPhys;
            }
            0
        }
    }

    /// (physical cores, logical processors), from the processor-core records.
    fn cores() -> (u32, u32) {
        let fallback = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(0);
        unsafe {
            let mut len = 0u32;
            let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len);
            if len == 0 {
                return (0, fallback);
            }
            // u64 words keep every record 8-byte aligned
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetLogicalProcessorInformationEx(RelationProcessorCore, Some(buf.as_mut_ptr() as *mut _), &mut len).is_err() {
                return (0, fallback);
            }
            let base = buf.as_ptr() as *const u8;
            let (mut at, mut physical, mut logical) = (0usize, 0u32, 0u32);
            while at + 8 <= len as usize {
                let rec = &*(base.add(at) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
                if rec.Size == 0 {
                    break;
                }
                if rec.Relationship == RelationProcessorCore {
                    physical += 1;
                    let p = &rec.Anonymous.Processor;
                    let masks = std::slice::from_raw_parts(p.GroupMask.as_ptr(), p.GroupCount.max(1) as usize);
                    logical += masks.iter().map(|g| g.Mask.count_ones()).sum::<u32>();
                }
                at += rec.Size as usize;
            }
            if physical == 0 {
                (0, fallback)
            } else {
                (physical, logical.max(physical))
            }
        }
    }

    /// The primary display's current mode in physical pixels (unaffected by DPI scaling).
    fn display() -> Option<(u32, u32)> {
        unsafe {
            let mut m = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            if EnumDisplaySettingsW(windows::core::PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut m).as_bool() && m.dmPelsWidth > 0 && m.dmPelsHeight > 0 {
                Some((m.dmPelsWidth, m.dmPelsHeight))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, vendor: u32, gib: f64) -> Option<Gpu> {
        Some(Gpu { name: name.into(), vendor_id: vendor, vram_bytes: (gib * GIB as f64) as u64 })
    }
    fn machine(g: Option<Gpu>, ram_gib: u64, cores: (u32, u32), display: (u32, u32)) -> Specs {
        Specs { gpu: g, ram_bytes: ram_gib * GIB, physical: cores.0, logical: cores.1, display: Some(display) }
    }
    const NV: u32 = 0x10de;
    const AMD: u32 = 0x1002;
    const INTEL: u32 = 0x8086;
    const UHD4K: (u32, u32) = (3840, 2160);

    /// The mapping table (also in `docs/design/launcher.md`): machine → output, backend, mode.
    #[test]
    fn the_machine_table() {
        // DXGI reports the 4070 Ti's 12 GB as 12,282 MiB
        let ti = || gpu("NVIDIA GeForce RTX 4070 Ti", NV, 12_282.0 / 1024.0);
        let rows: Vec<(&str, Specs, (u32, u32), &str, &str)> = vec![
            ("the reference: 4070 Ti, 64 GB, 4K", machine(ti(), 64, (8, 16), UHD4K), (3840, 2160), "dlss", "quality"),
            ("4070 Ti on a 1440p display", machine(ti(), 64, (8, 16), (2560, 1440)), (2560, 1440), "dlss", "dlaa"),
            ("4070 Ti on a 1080p display", machine(ti(), 64, (8, 16), (1920, 1080)), (1920, 1080), "dlss", "dlaa"),
            ("4070 Ti on an ultrawide 3440x1440", machine(ti(), 64, (8, 16), (3440, 1440)), (2560, 1440), "dlss", "dlaa"),
            ("RTX 4090, 24 GB", machine(gpu("NVIDIA GeForce RTX 4090", NV, 24.0), 64, (16, 32), UHD4K), (3840, 2160), "dlss", "quality"),
            ("8 GB card, 16 GB RAM (RTX 3070)", machine(gpu("NVIDIA GeForce RTX 3070", NV, 8.0), 16, (6, 12), UHD4K), (2560, 1440), "dlss", "quality"),
            ("8 GB unknown card", machine(gpu("Some Future GPU", NV, 8.0), 16, (6, 12), UHD4K), (2560, 1440), "fsr", "quality"),
            ("8 GB card on a 1080p display", machine(gpu("NVIDIA GeForce RTX 3070", NV, 8.0), 16, (6, 12), (1920, 1080)), (1920, 1080), "dlss", "quality"),
            ("6 GB AMD (RX 5600 XT)", machine(gpu("AMD Radeon RX 5600 XT", AMD, 6.0), 16, (6, 12), (2560, 1440)), (1920, 1080), "fsr", "quality"),
            ("6 GB AMD, unknown model", machine(gpu("AMD Radeon Pro W5500X", AMD, 6.0), 16, (6, 12), (1920, 1080)), (1920, 1080), "fsr", "quality"),
            ("RX 7900 XTX, 24 GB", machine(gpu("AMD Radeon RX 7900 XTX", AMD, 24.0), 32, (8, 16), UHD4K), (3840, 2160), "fsr", "quality"),
            ("RTX 3060 12 GB is capped by its model", machine(gpu("NVIDIA GeForce RTX 3060", NV, 12.0), 32, (6, 12), UHD4K), (1920, 1080), "dlss", "quality"),
            ("Arc A770 16 GB is capped by its model", machine(gpu("Intel(R) Arc(TM) A770 Graphics", INTEL, 16.0), 32, (8, 16), UHD4K), (1920, 1080), "fsr", "quality"),
            ("a laptop 4090 is one class down", machine(gpu("NVIDIA GeForce RTX 4090 Laptop GPU", NV, 16.0), 32, (16, 24), UHD4K), (2560, 1440), "dlss", "quality"),
            ("GTX 1660: NVIDIA without RTX gets FSR", machine(gpu("NVIDIA GeForce GTX 1660 SUPER", NV, 6.0), 16, (6, 12), (1920, 1080)), (1280, 720), "fsr", "performance"),
            ("Intel iGPU: the lowest", machine(gpu("Intel(R) UHD Graphics 770", INTEL, 128.0 / 1024.0), 16, (6, 12), (1920, 1080)), (1280, 720), "fsr", "performance"),
            ("Intel Iris Xe, a laptop", machine(gpu("Intel(R) Iris(R) Xe Graphics", INTEL, 128.0 / 1024.0), 16, (4, 8), (1920, 1200)), (1280, 720), "fsr", "performance"),
            ("AMD APU (512 MB carve-out)", machine(gpu("AMD Radeon(TM) Graphics", AMD, 0.5), 16, (8, 16), (2560, 1440)), (1280, 720), "fsr", "performance"),
            ("no hardware GPU", machine(None, 16, (4, 8), (1920, 1080)), (1280, 720), "fsr", "performance"),
            ("a display smaller than 720p", machine(gpu("Intel(R) UHD Graphics", INTEL, 0.125), 8, (2, 4), (1024, 600)), (1024, 576), "fsr", "balanced"),
            ("16:10 at 1920x1200 gets 1080p", machine(gpu("NVIDIA GeForce RTX 3070", NV, 8.0), 16, (6, 12), (1920, 1200)), (1920, 1080), "dlss", "quality"),
            ("1680x1050 gets 900p, DLAA within the 8 GB budget", machine(gpu("NVIDIA GeForce RTX 3070", NV, 8.0), 16, (6, 12), (1680, 1050)), (1600, 900), "dlss", "dlaa"),
        ];
        for (what, s, out, backend, mode) in rows {
            let r = recommend(&s);
            assert_eq!(((r.width, r.height), r.backend, r.mode), (out, backend, mode), "{what}: {r:#?}");
            // never above the display
            let (dw, dh) = s.display.unwrap();
            assert!(r.width <= dw && r.height <= dh, "{what}");
            // the values are registry-valid and every limit is 0
            let mut c = Config::parse("").unwrap();
            for (k, v) in r.values() {
                let reg = rebuild_config::find(k).unwrap_or_else(|| panic!("{k}"));
                reg.check(&v).unwrap_or_else(|e| panic!("{what}: {e}"));
                if k.starts_with("limits.") {
                    assert_eq!(v, Value::Int(0), "{what}: {k}");
                }
                c.set(k, v).unwrap();
            }
            assert!(rebuild_config::profiles::limits_in_force(&c).is_empty(), "{what}: no limits");
            assert!(!r.why.is_empty() && !r.why.contains('\n') && !r.specs_line.contains('\n'), "{what}: one line");
        }
    }

    #[test]
    fn the_reference_reads_as_the_minimum_spec() {
        let s = machine(gpu("NVIDIA GeForce RTX 4070 Ti", NV, 11.99), 64, (8, 16), UHD4K);
        let r = recommend(&s);
        assert_eq!(r.name(), "4K · DLSS Quality");
        assert_eq!(r.internal, (2560, 1440));
        assert_eq!(r.class, Class::P2160);
        assert_eq!(r.specs_line, "NVIDIA GeForce RTX 4070 Ti · 12 GB VRAM · 64 GB RAM · 8 cores / 16 threads · 3840×2160 display");
        assert!(r.why.contains("minimum spec") && r.why.contains("console's own"), "{}", r.why);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        let low = recommend(&machine(gpu("AMD Radeon RX 6600", AMD, 8.0), 16, (4, 8), (1920, 1080)));
        assert!(low.why.starts_with("Below the minimum spec"), "{}", low.why);
        assert_eq!(low.notes.len(), 2, "RAM and threads: {:?}", low.notes);
    }

    #[test]
    fn models_match_whole_names_most_specific_first() {
        assert_eq!(model_class("NVIDIA GeForce RTX 4070 Ti SUPER"), Some(Class::P2160));
        assert_eq!(model_class("NVIDIA GeForce RTX 4070 SUPER"), Some(Class::P1440));
        assert_eq!(model_class("NVIDIA GeForce RTX 4060 Ti"), Some(Class::P1440));
        assert_eq!(model_class("NVIDIA GeForce RTX 4060"), Some(Class::P1080));
        assert_eq!(model_class("AMD Radeon RX 7900 GRE"), Some(Class::P1440));
        assert_eq!(model_class("AMD Radeon RX 7900 XT"), Some(Class::P2160));
        assert_eq!(model_class("AMD Radeon RX 580 2048SP"), Some(Class::Lowest));
        assert_eq!(model_class("AMD Radeon RX 5700 XT"), Some(Class::P1080), "not the 'rx 5' family entry");
        assert_eq!(model_class("NVIDIA GeForce GTX 1650"), Some(Class::Lowest));
        assert_eq!(model_class("NVIDIA GeForce GTX 1080 Ti"), Some(Class::P1080));
        assert_eq!(model_class("NVIDIA GeForce RTX 3080 Ti Laptop GPU"), Some(Class::P1080));
        assert_eq!(model_class("Microsoft Basic Render Driver"), None);
        assert_eq!(model_class("NVIDIA GeForce RTX 40700"), None, "a word boundary");
    }

    #[test]
    fn vram_classes_have_slack_for_dxgi_rounding() {
        assert_eq!(vram_class(12_282 << 20), Class::P2160);
        assert_eq!(vram_class(8_031 << 20), Class::P1440);
        assert_eq!(vram_class(6_052 << 20), Class::P1080);
        assert_eq!(vram_class(4 << 30), Class::Lowest);
    }

    #[test]
    fn the_largest_gpu_wins_and_an_unknown_display_is_no_cap() {
        let s = Specs { gpu: gpu("NVIDIA GeForce RTX 4080", NV, 16.0), ..Default::default() };
        let r = recommend(&s);
        assert_eq!((r.width, r.height, r.mode), (3840, 2160, "quality"));
        assert_eq!(specs_line(&s), "NVIDIA GeForce RTX 4080 · 16 GB VRAM");
    }

    #[test]
    fn applying_sets_the_eight_keys_clears_limits_and_keeps_the_rest() {
        let mut m = Model::new(Config::parse("[settings]\n\"display.vsync\" = true\n").unwrap());
        rebuild_config::builtin("series-s").unwrap().apply(&mut m.config).unwrap();
        assert!(!rebuild_config::profiles::limits_in_force(&m.config).is_empty());
        let r = recommend(&machine(gpu("NVIDIA GeForce RTX 3070", NV, 8.0), 16, (6, 12), UHD4K));
        r.apply(&mut m).unwrap();
        assert!(r.matches(&m.config));
        assert_eq!(m.config.profile, PROFILE);
        assert!(rebuild_config::profiles::limits_in_force(&m.config).is_empty(), "the test profile's limits are gone");
        assert!(m.config.bool("display.vsync"), "other settings kept");
        assert!(PROFILE_KEYS.iter().all(|k| m.explicit.contains(k)), "the eight keys are explicit");
        m.config.set("display.width", Value::Int(1920)).unwrap();
        assert!(!r.matches(&m.config), "an edit is no longer the recommendation");
    }

    #[test]
    fn first_run_is_only_a_game_that_never_chose_an_output() {
        let d = crate::temp_dir("recommend-first");
        let p = d.join("rebuild.toml");
        let r = recommend(&machine(gpu("AMD Radeon RX 5600 XT", AMD, 6.0), 16, (6, 12), UHD4K));
        // a new game: only game.dir set (what Library::add writes)
        let mut s = Store::load(&p).unwrap();
        s.model.apply(rebuild_config::find("game.dir").unwrap(), crate::model::Edit::Text(r"D:\x".into())).unwrap();
        assert!(first_run(&s));
        apply_first_run(&mut s, &r).unwrap();
        s.save(&p).unwrap();
        let back = Store::load(&p).unwrap();
        assert!(r.matches(&back.model.config));
        assert_eq!(back.top_str(FIRST_RUN_KEY).as_deref(), Some("1080p · FSR Quality"));
        assert!(!first_run(&back), "once per game");
        // even after the user resets every key, it is not applied again
        let mut reset = back.clone();
        for k in PROFILE_KEYS {
            reset.model.reset(rebuild_config::find(k).unwrap());
        }
        reset.model.config.profile = "default".into();
        assert!(!first_run(&reset));
        // a game where the user chose an output is never first run
        let mut user = Store::load(&d.join("other.toml")).unwrap();
        user.model.apply(rebuild_config::find("display.width").unwrap(), crate::model::Edit::Slide(1600)).unwrap();
        assert!(!first_run(&user));
        // nor one on a profile
        let mut prof = Store::load(&d.join("third.toml")).unwrap();
        prof.model.config.profile = "mine".into();
        assert!(!first_run(&prof));
    }

    #[test]
    fn detection_reads_this_machine_without_a_device() {
        // DXGI enumeration only; no D3D12 device, so it is safe while a measurement runs
        let s = detect();
        assert!(s.logical >= 1, "{s:?}");
        #[cfg(windows)]
        {
            assert!(s.ram_bytes >= GIB, "{s:?}");
            assert!(s.physical >= 1 && s.physical <= s.logical, "{s:?}");
            assert!(s.display.is_some_and(|(w, h)| w >= 640 && h >= 360), "{s:?}");
        }
        let r = recommend(&s);
        assert!(r.width >= 640 && r.height >= 360);
    }
}
