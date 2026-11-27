//! L5: the in-game menu on the present path (`crates/overlay`), wired to the runtime.
//!
//! - [`attach`] (the presenter, once its swap chain exists) starts the `overlay-input` thread: it builds the
//!   renderer (shaders compiled there, not on the present thread), reads `rebuild.toml` from `REBUILD_CONFIG` when
//!   set, and runs the overlay's service: input polled every 8 ms, the title's input isolated while the menu is
//!   open ([`title_input`]), the menu laid out when something changes. `REBUILD_OVERLAY=0` turns all of it off.
//! - The menu key is `menu.key` (or `REBUILD_MENU_KEY`), F10 by default; on a pad, View + Menu together.
//! - Settings apply live through [`crate::frame::set_frame_limit`] / [`crate::frame::set_vsync`]; changes are saved
//!   into `REBUILD_CONFIG` when the menu closes. `diagnostics.overlay` shows the stats readout while it is closed.
//! - The present calls [`visible`] each frame (one relaxed load) and, when true, [`record`]: the scene's quads in
//!   one draw, in the frame slot's own list.
//! - Developer page: the research's actions as placeholders, plus "Capture frame" (never online-gated: it reads
//!   only our own buffer).

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rebuild_config::{Config, Value};
use rebuild_input::{menu_key_named, Frame, Input, MENU_KEY, PAD_CHORD};
use rebuild_overlay::render::Renderer;
use rebuild_overlay::{theme, Action, Backend, Font, Menu, ParamValue, Registry, Service, Shared, Stats};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource, D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_RESOURCE_STATES};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

use crate::frame::{self, FrameLimit};

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static RENDERER: Mutex<Option<Renderer>> = Mutex::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);
/// The present thread's time in [`record`] (ns, smoothed) and the pass's GPU time (µs, smoothed), as f64 bits.
static CPU_NS: AtomicU64 = AtomicU64::new(0);
static GPU_US: AtomicU64 = AtomicU64::new(0);

/// `REBUILD_OVERLAY=0` turns the menu off.
pub fn enabled() -> bool {
    !std::env::var("REBUILD_OVERLAY").is_ok_and(|v| v.trim() == "0")
}

/// `REBUILD_CAPTURE_OVERLAY=1`: captures include the overlay (by default they never do, so frame comparisons stay
/// clean).
pub fn capture_overlay() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| std::env::var("REBUILD_CAPTURE_OVERLAY").is_ok_and(|v| v.trim() == "1"))
}

/// Start the menu for a swap chain of `format` with `slots` frame slots (once; later calls do nothing).
pub fn attach(device: &ID3D12Device, format: DXGI_FORMAT, frequency: u64, slots: usize) {
    if !enabled() || STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let shared = SHARED.get_or_init(Shared::new).clone();
    let device = device.clone();
    let spawned = std::thread::Builder::new().name("overlay-input".into()).stack_size(1 << 20).spawn(move || {
        let t = Instant::now();
        match Renderer::new(&device, format, slots, frequency) {
            Ok(r) => *RENDERER.lock().unwrap_or_else(|e| e.into_inner()) = Some(r),
            Err(e) => {
                eprintln!("overlay   no renderer: {e}; the menu is off");
                return;
            }
        }
        let (config, path) = load_config();
        let key_name = std::env::var("REBUILD_MENU_KEY").ok().filter(|k| menu_key_named(k).is_some()).unwrap_or_else(|| config.choice("menu.key"));
        let key = menu_key_named(&key_name).unwrap_or(MENU_KEY);
        let backend = Runtime;
        let menu = Menu::new(config, &backend, registry());
        eprintln!("overlay   ready in {:.1} ms: {key_name} or View+Menu opens the menu", t.elapsed().as_secs_f64() * 1e3);
        let input = Input::with_menu_key(rebuild_input::host::Host::new(), key).with_pad_chord(PAD_CHORD);
        Service::new(input, menu, backend, shared, Box::new(bake), path, &key_name).run()
    });
    if let Err(e) = spawned {
        eprintln!("overlay   no input thread: {e}");
    }
}

/// A font for `px`, its atlas buffers made here (on the input thread) so the present only records their copy.
fn bake(px: u32) -> Font {
    let font = Font::bake(theme::FONT_FACE, theme::FONT_WEIGHT, px).unwrap_or_else(|| {
        eprintln!("overlay   GDI could not bake {} at {px} px: box glyphs", theme::FONT_FACE);
        Font::synthetic(px)
    });
    if let Some(r) = RENDERER.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        if let Err(e) = r.prepare(&font) {
            eprintln!("overlay   atlas: {e}");
        }
    }
    font
}

/// `REBUILD_CONFIG` (the launcher's `rebuild.toml`), else defaults and nothing saved.
fn load_config() -> (Config, Option<PathBuf>) {
    let Some(path) = std::env::var_os("REBUILD_CONFIG").map(PathBuf::from) else { return (Config::default(), None) };
    match std::fs::read_to_string(&path) {
        Ok(text) => match Config::parse(&text) {
            Ok(c) => (c, Some(path)),
            Err(e) => {
                eprintln!("overlay   {e}: defaults, and the menu will not save");
                (Config::default(), None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Some(path)),
        Err(e) => {
            eprintln!("overlay   {}: {e}: defaults, and the menu will not save", path.display());
            (Config::default(), None)
        }
    }
}

/// The Developer page: the research's placeholders, and what the layer can do itself.
fn registry() -> Registry {
    let mut r = Registry::default();
    r.register(Box::new(CaptureFrame));
    r.with_placeholders()
}

struct CaptureFrame;

impl Action for CaptureFrame {
    fn name(&self) -> &str {
        "Capture frame"
    }
    fn group(&self) -> &str {
        "Layer: the next shown frame as a PNG in the session's captures folder (without this menu)"
    }
    fn offline_only(&self) -> bool {
        false
    }
    fn run(&mut self, _: &[ParamValue]) -> Result<String, String> {
        crate::present::request_capture(None);
        Ok("capture requested: the next frame, in captures/".into())
    }
}

/// Anything to draw this frame? (the present's check)
#[inline]
pub fn visible() -> bool {
    SHARED.get().is_some_and(|s| s.visible())
}

/// The image the overlay draws on (the present, every frame).
#[inline]
pub fn set_target(w: u32, h: u32) {
    if let Some(s) = SHARED.get() {
        s.set_target(w, h);
    }
}

/// The input the title sees (neutral while the menu is open); `None` before the menu started.
pub fn title_input() -> Option<Frame> {
    SHARED.get().map(|s| s.title_input())
}

fn smooth(cell: &AtomicU64, v: f64) {
    let old = f64::from_bits(cell.load(Ordering::Relaxed));
    cell.store((if old == 0.0 { v } else { old * 0.9 + v * 0.1 }).to_bits(), Ordering::Relaxed);
}

/// Record the overlay over `target` (in `state`) in frame slot `slot`: the list, open, the target in
/// `RENDER_TARGET`; `None` when there is nothing to draw or the renderer is not ready.
///
/// # Safety
/// As [`Renderer::record`].
pub unsafe fn record(slot: usize, target: &ID3D12Resource, state: D3D12_RESOURCE_STATES, rtv: D3D12_CPU_DESCRIPTOR_HANDLE, done: u64, next: u64) -> Option<ID3D12GraphicsCommandList> {
    let t = Instant::now();
    let scene = SHARED.get()?.scene()?;
    let mut g = RENDERER.try_lock().ok()?;
    let r = g.as_mut()?;
    if scene.size.0 == 0 || scene.size.1 == 0 {
        return None;
    }
    let list = unsafe { r.record(slot, &scene, target, state, rtv, done, next) };
    if let Some(us) = r.gpu_us() {
        GPU_US.store(us.to_bits(), Ordering::Relaxed);
    }
    smooth(&CPU_NS, t.elapsed().as_nanos() as f64);
    list
}

/// The runtime's side of the menu's settings and stats.
struct Runtime;

const APPLIED: [&str; 4] = ["display.vsync", "display.frame_rate", "display.frame_cap", "diagnostics.overlay"];

impl Backend for Runtime {
    fn applies(&self, key: &str) -> bool {
        APPLIED.contains(&key)
    }

    fn current(&self, key: &str) -> Option<Value> {
        match key {
            "display.vsync" => Some(Value::Bool(frame::vsync())),
            "display.frame_rate" => Some(Value::Choice(
                match frame::frame_limit() {
                    FrameLimit::Title => "title",
                    FrameLimit::Cap(_) => "capped",
                    FrameLimit::Unlimited => "unlimited",
                }
                .into(),
            )),
            "display.frame_cap" => match frame::frame_limit() {
                FrameLimit::Cap(n) => Some(Value::Int(i64::from(n))),
                _ => None,
            },
            _ => None,
        }
    }

    fn apply(&mut self, key: &str, c: &Config) {
        apply(key, c);
    }

    fn stats(&self) -> Stats {
        let l = crate::framelog::latest().unwrap_or_default();
        let f = |cell: &AtomicU64| Some(f64::from_bits(cell.load(Ordering::Relaxed))).filter(|v| *v > 0.0);
        Stats {
            seq: l.t,
            fps: (l.t > 0).then_some(l.fps),
            ms: l.ms,
            gpu: l.gpu,
            draws: l.draws,
            dispatches: l.dispatches,
            dropped: l.dropped,
            late: (l.t > 0).then_some(l.late),
            wait: l.wait,
            internal: l.internal,
            output: l.output,
            overlay_cpu_us: f(&CPU_NS).map(|ns| ns / 1000.0),
            overlay_gpu_us: f(&GPU_US),
            pacing: frame::describe(),
        }
    }
}

/// A menu change to the frame API.
fn apply(key: &str, c: &Config) {
    match key {
        "display.vsync" => frame::set_vsync(c.bool("display.vsync")),
        "display.frame_rate" | "display.frame_cap" => {
            // a new cap only matters while the rate is capped
            if key == "display.frame_cap" && c.choice("display.frame_rate") != "capped" {
                return;
            }
            if let Some(l) = FrameLimit::parse(&rebuild_config::frame_rate_env(c)) {
                frame::set_frame_limit(l);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_overlay::Nav;

    #[test]
    fn menu_settings_go_through_the_frame_api() {
        let _g = frame::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        frame::set_frame_limit(FrameLimit::Title);
        frame::set_vsync(false);
        let mut b = Runtime;
        let mut m = Menu::new(Config::default(), &b, registry());
        let keys: Vec<&str> = m.settings().iter().map(|s| s.key).collect();
        assert_eq!(keys, APPLIED);
        // Vsync on
        m.nav(Nav::Accept, &mut b);
        assert!(frame::vsync());
        // Frame rate: title -> capped (at the cap, 60) -> unlimited
        m.nav(Nav::Down, &mut b);
        m.nav(Nav::Right, &mut b);
        assert_eq!(frame::frame_limit(), FrameLimit::Cap(60));
        // the cap: 60 -> 90 -> 120, live while capped
        m.nav(Nav::Down, &mut b);
        m.nav(Nav::Right, &mut b);
        m.nav(Nav::Right, &mut b);
        assert_eq!(frame::frame_limit(), FrameLimit::Cap(120));
        m.nav(Nav::Up, &mut b);
        m.nav(Nav::Right, &mut b);
        assert_eq!(frame::frame_limit(), FrameLimit::Unlimited);
        // a cap change while unlimited is kept for later, not applied
        m.nav(Nav::Down, &mut b);
        m.nav(Nav::Left, &mut b);
        assert_eq!(frame::frame_limit(), FrameLimit::Unlimited);
        m.nav(Nav::Up, &mut b);
        m.nav(Nav::Right, &mut b); // wraps to title
        assert_eq!(frame::frame_limit(), FrameLimit::Title);
        m.nav(Nav::Right, &mut b);
        assert_eq!(frame::frame_limit(), FrameLimit::Cap(90));
        // the menu reads what is in effect when it opens
        frame::set_frame_limit(FrameLimit::Cap(144));
        frame::set_vsync(false);
        m.sync(&b);
        assert_eq!(m.config().choice("display.frame_rate"), "capped");
        assert_eq!(m.config().int("display.frame_cap"), 144);
        assert!(!m.config().bool("display.vsync"));
        frame::set_frame_limit(FrameLimit::Title);
    }

    #[test]
    fn the_developer_page_has_capture_and_the_placeholders() {
        let r = registry();
        assert_eq!(r.get(0).unwrap().name(), "Capture frame");
        assert_eq!(r.blocked(0), None);
        assert_eq!(r.len(), 5);
        assert!(!r.online());
    }
}
