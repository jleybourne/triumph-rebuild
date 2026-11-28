//! U2: the first-run system check. Each check reports pass, fail or skipped, with a one-line fix.
//!
//! The checks read facts through [`Probe`], so they are unit-tested with fakes. The real probe
//! ([`SystemProbe`]) queries the adapter through DXGI, and creates a D3D12 device for the tiled tier,
//! shader model and upload-heap (ReBAR) checks **only when the measurement lock is absent**.

use std::path::PathBuf;

/// The adapter, as DXGI reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Adapter {
    pub name: String,
    pub vendor_id: u32,
    /// the user-mode driver version, four 16-bit parts
    pub driver: Option<[u16; 4]>,
    pub vram_bytes: u64,
    pub software: bool,
}

/// What a D3D12 device says it supports.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceCaps {
    Caps {
        tiled_tier: u32,
        /// (major, minor), e.g. (6, 6)
        shader_model: (u32, u32),
        /// D3D12 GPU upload heaps: CPU-visible VRAM, which needs Resizable BAR
        gpu_upload_heap: bool,
    },
    /// not created, and why (the measurement lock, or no adapter)
    Skipped(String),
    Failed(String),
}

pub trait Probe {
    /// `game.dir` from the settings ("" when unset)
    fn game_dir(&self) -> PathBuf;
    fn adapter(&self) -> Option<Adapter>;
    fn device(&self) -> DeviceCaps;
    fn dxc(&self) -> Option<PathBuf>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// one line: what to do when it fails (empty on a pass)
    pub fix: String,
}

/// Provisional VRAM floor until the owner sets the minimum spec in `plan.md` (an owner-only item).
pub const MIN_VRAM_GIB: u64 = 8;
pub const MIN_TILED_TIER: u32 = 2;
pub const MIN_SHADER_MODEL: (u32, u32) = (6, 6);

pub fn vendor(id: u32) -> &'static str {
    match id {
        0x10de => "NVIDIA",
        0x1002 => "AMD",
        0x8086 => "Intel",
        0x1414 => "Microsoft",
        _ => "unknown vendor",
    }
}

/// The driver version as the vendor writes it (NVIDIA: 566.14 from 32.0.15.6614).
pub fn driver_text(vendor_id: u32, v: [u16; 4]) -> String {
    if vendor_id == 0x10de {
        let n = (v[2] as u32 % 10) * 10000 + v[3] as u32;
        format!("{}.{:02} ({}.{}.{}.{})", n / 100, n % 100, v[0], v[1], v[2], v[3])
    } else {
        format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3])
    }
}

fn check(name: &'static str, ok: bool, detail: String, fix: &str) -> Check {
    Check { name, status: if ok { Status::Pass } else { Status::Fail }, detail, fix: if ok { String::new() } else { fix.into() } }
}

pub fn run(p: &dyn Probe) -> Vec<Check> {
    let mut out = Vec::new();

    let dir = p.game_dir();
    let game = if dir.as_os_str().is_empty() {
        check("Game folder", false, "not set".into(), "Pick the title's folder on the Play tab.")
    } else if !dir.is_dir() {
        check("Game folder", false, format!("{} is not a folder", dir.display()), "Pick the title's folder on the Play tab.")
    } else {
        let n = std::fs::read_dir(&dir).map(|r| r.count()).unwrap_or(0);
        check("Game folder", n > 0, format!("{} ({n} entries)", dir.display()), "The folder is empty: pick the folder the title was installed to.")
    };
    out.push(game);

    let adapter = p.adapter();
    out.push(match &adapter {
        None => check("GPU", false, "no DXGI adapter".into(), "Install the GPU vendor's driver; a D3D12 GPU is required."),
        Some(a) => check(
            "GPU",
            !a.software,
            format!("{} ({})", a.name, vendor(a.vendor_id)),
            "Only a software adapter was found: install the GPU vendor's driver.",
        ),
    });
    out.push(match adapter.as_ref().and_then(|a| a.driver.map(|d| (a.vendor_id, d))) {
        Some((v, d)) => check("Driver", true, driver_text(v, d), ""),
        None => check("Driver", false, "version not readable".into(), "Install the latest driver from the GPU vendor's site."),
    });
    out.push(match &adapter {
        Some(a) => {
            let gib = a.vram_bytes as f64 / (1u64 << 30) as f64;
            check("VRAM", a.vram_bytes >= MIN_VRAM_GIB << 30, format!("{gib:.1} GiB dedicated"), "Under 8 GiB of VRAM: the title's memory may not fit (minimum spec not final).")
        }
        None => check("VRAM", false, "no adapter".into(), "Install the GPU vendor's driver."),
    });

    let skipped = |name: &'static str, why: &str| Check {
        name,
        status: Status::Skipped,
        detail: why.into(),
        fix: "Run the check again once the device can be created.".into(),
    };
    match p.device() {
        DeviceCaps::Caps { tiled_tier, shader_model, gpu_upload_heap } => {
            out.push(check(
                "Tiled resources",
                tiled_tier >= MIN_TILED_TIER,
                format!("tier {tiled_tier}"),
                "Tier 2 or higher is needed for console addresses: update the driver, or use a newer GPU.",
            ));
            out.push(check(
                "Shader model",
                shader_model >= MIN_SHADER_MODEL,
                format!("{}.{}", shader_model.0, shader_model.1),
                "Shader model 6.6 is needed: update the GPU driver and Windows.",
            ));
            out.push(check(
                "Resizable BAR",
                gpu_upload_heap,
                if gpu_upload_heap { "GPU upload heaps supported".into() } else { "GPU upload heaps not supported".into() },
                "Enable Resizable BAR (and Above 4G decoding) in the firmware, then update the driver.",
            ));
        }
        DeviceCaps::Skipped(why) => {
            for n in ["Tiled resources", "Shader model", "Resizable BAR"] {
                out.push(skipped(n, &why));
            }
        }
        DeviceCaps::Failed(why) => {
            for n in ["Tiled resources", "Shader model", "Resizable BAR"] {
                out.push(check(n, false, format!("D3D12 device: {why}"), "Update the GPU driver: D3D12 feature level 12.0 is needed."));
            }
        }
    }

    out.push(match p.dxc() {
        Some(d) => check("DXC", true, d.display().to_string(), ""),
        None => check("DXC", false, "no DXC found".into(), "Put dxc.exe, dxcompiler.dll and dxil.dll in the dxc folder beside the launcher (a release has them), install the Windows SDK, or set REBUILD_DXC to dxc.exe."),
    });
    out
}

/// DXC (`rebuild_pipelines::dxc::locate`): `REBUILD_DXC`, the bundled copy beside the launcher
/// (`dxc\dxc.exe` with its two DLLs), or the newest `dxc.exe` in the Windows SDK.
pub fn find_dxc() -> Option<PathBuf> {
    rebuild_pipelines::dxc::locate().ok().map(|d| d.exe)
}

/// The real machine. `lock` is the measurement lock file: while it exists no device is created.
pub struct SystemProbe {
    pub game_dir: PathBuf,
    pub lock: Option<PathBuf>,
}

impl Probe for SystemProbe {
    fn game_dir(&self) -> PathBuf {
        self.game_dir.clone()
    }
    fn adapter(&self) -> Option<Adapter> {
        #[cfg(windows)]
        {
            win::adapter().map(|(a, _)| a)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
    fn device(&self) -> DeviceCaps {
        if let Some(l) = &self.lock {
            if l.exists() {
                return DeviceCaps::Skipped(format!("a measurement is running ({} exists)", l.display()));
            }
        }
        #[cfg(windows)]
        {
            win::device()
        }
        #[cfg(not(windows))]
        {
            DeviceCaps::Skipped("not Windows".into())
        }
    }
    fn dxc(&self) -> Option<PathBuf> {
        find_dxc()
    }
}

#[cfg(windows)]
mod win {
    use super::{Adapter, DeviceCaps};
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;

    /// The high-performance adapter, through DXGI only (no device).
    pub fn adapter() -> Option<(Adapter, IDXGIAdapter1)> {
        unsafe {
            let f: IDXGIFactory6 = CreateDXGIFactory1().ok()?;
            let a: IDXGIAdapter1 = f.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE).ok()?;
            let d = a.GetDesc1().ok()?;
            let len = d.Description.iter().position(|c| *c == 0).unwrap_or(d.Description.len());
            let driver = a.CheckInterfaceSupport(&IDXGIDevice::IID).ok().map(|v| {
                let v = v as u64;
                [(v >> 48) as u16, (v >> 32) as u16, (v >> 16) as u16, v as u16]
            });
            Some((
                Adapter {
                    name: String::from_utf16_lossy(&d.Description[..len]),
                    vendor_id: d.VendorId,
                    driver,
                    vram_bytes: d.DedicatedVideoMemory as u64,
                    software: (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
                },
                a,
            ))
        }
    }

    pub fn device() -> DeviceCaps {
        let Some((_, a)) = adapter() else { return DeviceCaps::Skipped("no adapter".into()) };
        unsafe {
            let mut dev: Option<ID3D12Device> = None;
            if let Err(e) = D3D12CreateDevice(&a, D3D_FEATURE_LEVEL_12_0, &mut dev) {
                return DeviceCaps::Failed(e.to_string());
            }
            let Some(dev) = dev else { return DeviceCaps::Failed("no device".into()) };
            let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
            let tiled = dev
                .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut _, std::mem::size_of_val(&o) as u32)
                .map(|_| o.TiledResourcesTier.0 as u32)
                .unwrap_or(0);
            // ask for the highest model this header knows; the runtime lowers it, or refuses one it
            // does not know, so step down until it answers
            let mut sm = (0, 0);
            for want in (0x60..=0x69).rev() {
                let mut m = D3D12_FEATURE_DATA_SHADER_MODEL { HighestShaderModel: D3D_SHADER_MODEL(want) };
                if dev
                    .CheckFeatureSupport(D3D12_FEATURE_SHADER_MODEL, &mut m as *mut _ as *mut _, std::mem::size_of_val(&m) as u32)
                    .is_ok()
                {
                    let v = m.HighestShaderModel.0 as u32;
                    sm = (v >> 4, v & 0xf);
                    break;
                }
            }
            let mut o16 = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
            let upload = dev
                .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS16, &mut o16 as *mut _ as *mut _, std::mem::size_of_val(&o16) as u32)
                .map(|_| o16.GPUUploadHeapSupported.as_bool())
                .unwrap_or(false);
            DeviceCaps::Caps { tiled_tier: tiled, shader_model: sm, gpu_upload_heap: upload }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        game: PathBuf,
        adapter: Option<Adapter>,
        device: DeviceCaps,
        dxc: Option<PathBuf>,
    }
    impl Probe for Fake {
        fn game_dir(&self) -> PathBuf {
            self.game.clone()
        }
        fn adapter(&self) -> Option<Adapter> {
            self.adapter.clone()
        }
        fn device(&self) -> DeviceCaps {
            self.device.clone()
        }
        fn dxc(&self) -> Option<PathBuf> {
            self.dxc.clone()
        }
    }

    fn good() -> Fake {
        let game = crate::temp_dir("syscheck");
        std::fs::write(game.join("a-file"), b"x").unwrap();
        Fake {
            game,
            adapter: Some(Adapter {
                name: "Fake GPU".into(),
                vendor_id: 0x10de,
                driver: Some([32, 0, 15, 6614]),
                vram_bytes: 12 << 30,
                software: false,
            }),
            device: DeviceCaps::Caps { tiled_tier: 3, shader_model: (6, 8), gpu_upload_heap: true },
            dxc: Some(PathBuf::from(r"C:\sdk\dxc.exe")),
        }
    }

    fn status(c: &[Check], name: &str) -> Status {
        c.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name}")).status
    }

    #[test]
    fn a_good_machine_passes_every_check() {
        let c = run(&good());
        assert_eq!(c.len(), 8);
        for x in &c {
            assert_eq!(x.status, Status::Pass, "{x:?}");
            assert!(x.fix.is_empty());
        }
        assert!(c.iter().any(|x| x.detail.starts_with("566.14")));
    }

    #[test]
    fn each_failure_is_caught_with_a_fix_line() {
        let cases: Vec<(&str, Box<dyn Fn(&mut Fake)>)> = vec![
            ("Game folder", Box::new(|f| f.game = PathBuf::new())),
            ("Game folder", Box::new(|f| f.game = f.game.join("missing"))),
            ("GPU", Box::new(|f| f.adapter.as_mut().unwrap().software = true)),
            ("Driver", Box::new(|f| f.adapter.as_mut().unwrap().driver = None)),
            ("VRAM", Box::new(|f| f.adapter.as_mut().unwrap().vram_bytes = 6 << 30)),
            ("Tiled resources", Box::new(|f| f.device = DeviceCaps::Caps { tiled_tier: 1, shader_model: (6, 8), gpu_upload_heap: true })),
            ("Shader model", Box::new(|f| f.device = DeviceCaps::Caps { tiled_tier: 3, shader_model: (6, 5), gpu_upload_heap: true })),
            ("Resizable BAR", Box::new(|f| f.device = DeviceCaps::Caps { tiled_tier: 3, shader_model: (6, 6), gpu_upload_heap: false })),
            ("DXC", Box::new(|f| f.dxc = None)),
        ];
        for (name, break_it) in cases {
            let mut f = good();
            break_it(&mut f);
            let c = run(&f);
            let x = c.iter().find(|x| x.name == name).unwrap();
            assert_eq!(x.status, Status::Fail, "{name}: {x:?}");
            assert!(!x.fix.is_empty() && !x.fix.contains('\n'), "{name}: one fix line");
            // and only that check fails
            assert_eq!(c.iter().filter(|x| x.status != Status::Pass).count(), 1, "{c:#?}");
        }
    }

    #[test]
    fn no_device_while_measuring() {
        let lock = crate::temp_dir("lock").join("MEASURING");
        std::fs::write(&lock, b"").unwrap();
        let p = SystemProbe { game_dir: PathBuf::new(), lock: Some(lock) };
        // the lock is checked before any device call, so this test never touches the GPU
        assert!(matches!(p.device(), DeviceCaps::Skipped(_)));
        let mut f = good();
        f.device = p.device();
        let c = run(&f);
        for n in ["Tiled resources", "Shader model", "Resizable BAR"] {
            assert_eq!(status(&c, n), Status::Skipped);
        }
    }

    #[test]
    fn nvidia_driver_numbers() {
        assert_eq!(driver_text(0x10de, [32, 0, 15, 6614]), "566.14 (32.0.15.6614)");
        assert_eq!(driver_text(0x10de, [32, 0, 15, 7680]), "576.80 (32.0.15.7680)");
        assert_eq!(driver_text(0x1002, [31, 0, 24027, 1012]), "31.0.24027.1012");
    }
}
