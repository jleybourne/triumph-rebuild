//! Which upscaler a context gets: DLSS through Streamline on NVIDIA, FSR 3.1 through the PC
//! FidelityFX SDK otherwise (plan decision 5). Both are `rebuild-upscale` backends, loaded from folders
//! the owner supplies (never committed):
//!
//! ```text
//! REBUILD_STREAMLINE_DIR + REBUILD_DLSS_DLL   DLSS (registry: upscaler.streamline_dir, upscaler.dlss_dll)
//! REBUILD_FIDELITYFX_DIR                      FSR 3.1 (registry: upscaler.fidelityfx_dir)
//! REBUILD_UPSCALER=dlss|fsr                   force one (default: DLSS when it attaches, else FSR)
//! ```
//!
//! Streamline must be initialised before the D3D12 device exists: the runtime calls [`prepare_dlss`]
//! just before it creates its device. One Streamline per process, so every context shares the one DLSS
//! instance ([`Shared`]); each context gets its own FSR context.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rebuild_upscale::dlss::Dlss;
use rebuild_upscale::fsr::Fsr;
use rebuild_upscale::{Inputs, Mode, Signs, Size, Upscaler};
use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{Backend, Wanted};

/// A backend shared by every context (Streamline is one per process).
pub struct Shared<U>(pub Arc<Mutex<U>>);

// SAFETY: the backends hold function pointers and COM objects that are free-threaded; the mutex
// serialises every call.
unsafe impl<U> Send for Shared<U> {}
unsafe impl<U> Sync for Shared<U> {}

impl<U: Upscaler<Resource = ID3D12Resource, CommandList = ID3D12GraphicsCommandList>> Upscaler for Shared<U> {
    type Resource = ID3D12Resource;
    type CommandList = ID3D12GraphicsCommandList;
    fn name(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).name()
    }
    fn render_size(&self, output: Size, mode: Mode) -> Result<Size, String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).render_size(output, mode)
    }
    fn evaluate(&mut self, list: &ID3D12GraphicsCommandList, mode: Mode, inputs: &Inputs<ID3D12Resource>) -> Result<(), String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).evaluate(list, mode, inputs)
    }
    fn set_signs(&mut self, signs: Signs) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).set_signs(signs)
    }
}

impl<U> Clone for Shared<U> {
    fn clone(&self) -> Self {
        Shared(self.0.clone())
    }
}

struct Prepared {
    dlss: Shared<Dlss>,
    attached: bool,
}

static DLSS: Mutex<Option<Prepared>> = Mutex::new(None);

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Before the device: initialise Streamline if DLSS is configured. Errors are logged; FSR remains.
pub fn prepare_dlss() {
    let (Some(sl), Some(dll)) = (env_path("REBUILD_STREAMLINE_DIR"), env_path("REBUILD_DLSS_DLL")) else { return };
    if matches!(std::env::var("REBUILD_UPSCALER").as_deref(), Ok("fsr")) {
        return;
    }
    let mut g = DLSS.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_some() {
        return;
    }
    match Dlss::init(&sl, &dll, env_path("REBUILD_SL_LOGS").as_deref()) {
        Ok(d) => *g = Some(Prepared { dlss: Shared(Arc::new(Mutex::new(d))), attached: false }),
        Err(e) => eprintln!("ffx: DLSS not available: {e}"),
    }
}

/// Makes a context's upscaler on one device.
pub struct Factory {
    device: ID3D12Device,
    fidelityfx: Option<PathBuf>,
    prefer_fsr: bool,
}

// SAFETY: ID3D12Device is free-threaded.
unsafe impl Send for Factory {}
unsafe impl Sync for Factory {}

impl Factory {
    pub fn new(device: &ID3D12Device, fidelityfx: Option<PathBuf>, prefer_fsr: bool) -> Factory {
        Factory { device: device.clone(), fidelityfx, prefer_fsr }
    }

    pub fn from_env(device: &ID3D12Device) -> Factory {
        Factory::new(device, env_path("REBUILD_FIDELITYFX_DIR"), matches!(std::env::var("REBUILD_UPSCALER").as_deref(), Ok("fsr")))
    }

    pub fn describe(&self) -> String {
        let dlss = DLSS.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        format!(
            "DLSS {}, FSR {}{}",
            if dlss { "prepared" } else { "not configured" },
            if self.fidelityfx.is_some() { "configured" } else { "not configured" },
            if self.prefer_fsr { ", FSR preferred" } else { "" }
        )
    }

    fn dlss(&self) -> Result<Backend, String> {
        let mut g = DLSS.lock().unwrap_or_else(|e| e.into_inner());
        let p = g.as_mut().ok_or("DLSS not prepared (REBUILD_STREAMLINE_DIR, REBUILD_DLSS_DLL)")?;
        if !p.attached {
            let l = unsafe { self.device.GetAdapterLuid() };
            let mut luid = [0u8; 8];
            luid[..4].copy_from_slice(&l.LowPart.to_le_bytes());
            luid[4..].copy_from_slice(&l.HighPart.to_le_bytes());
            p.dlss.0.lock().unwrap_or_else(|e| e.into_inner()).attach(&self.device, luid)?;
            p.attached = true;
        }
        Ok(Box::new(p.dlss.clone()))
    }

    fn fsr(&self, want: &Wanted) -> Result<Backend, String> {
        let dir = self.fidelityfx.as_ref().ok_or("FSR not configured (REBUILD_FIDELITYFX_DIR)")?;
        let mut f = Fsr::load(dir)?;
        f.attach(&self.device, want.max_output)?;
        Ok(Box::new(f))
    }

    /// DLSS when prepared and supported on this adapter (NVIDIA), else FSR.
    pub fn make(&self, want: &Wanted) -> Result<Backend, String> {
        if self.prefer_fsr {
            return self.fsr(want);
        }
        match self.dlss() {
            Ok(b) => Ok(b),
            Err(dlss) => self.fsr(want).map_err(|fsr| format!("{dlss}; {fsr}")),
        }
    }
}
