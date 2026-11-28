//! The overlay's input thread and what it shares with the present.
//!
//! The thread owns the [`Input`] (a poll costs ~170 µs with focus, so the present never polls), the
//! [`Navigator`] and the [`Menu`]. Each poll it publishes the title's isolated input ([`Shared::title_input`]),
//! turns menu input into navigation, and lays out a new [`Scene`] only when something shown changed. The present
//! thread reads [`Shared::visible`] (one relaxed load: the whole cost while nothing is shown) and, when visible,
//! clones the scene's `Arc` under a lock held for nanoseconds.
//!
//! Settings changed in the menu are saved when it closes, into the `rebuild.toml` it was given: the file is read
//! again and only the keys the menu changed are set, so nothing else in it is replaced (owner rule); the old file is
//! kept as `.bak` and the new one is written whole and then renamed into place.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rebuild_config::Config;
use rebuild_input::{Change, Frame, Input, Phase, Source};

use crate::font::Font;
use crate::layout::{self, Scene};
use crate::menu::{Backend, Menu, Outcome};
use crate::nav::Navigator;
use crate::theme;

/// How often the thread polls (the fork polled every 8 ms).
pub const POLL: Duration = Duration::from_millis(8);

#[derive(Default)]
pub struct Shared {
    visible: AtomicBool,
    scene: Mutex<Option<Arc<Scene>>>,
    title: Mutex<Frame>,
    /// the size of the image the overlay draws on (w << 32 | h), from the present; 0 = none yet
    target: AtomicU64,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared::default())
    }

    /// Anything to draw? (the present's check on every frame)
    #[inline]
    pub fn visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    pub fn scene(&self) -> Option<Arc<Scene>> {
        if !self.visible() {
            return None;
        }
        self.scene.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The present's image size (called every frame: one store).
    #[inline]
    pub fn set_target(&self, w: u32, h: u32) {
        self.target.store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
    }

    pub fn target(&self) -> Option<(u32, u32)> {
        let v = self.target.load(Ordering::Relaxed);
        (v != 0).then_some(((v >> 32) as u32, v as u32))
    }

    /// The input the title sees: neutral while the menu has it.
    pub fn title_input(&self) -> Frame {
        self.title.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn show(&self, scene: Option<Arc<Scene>>) {
        let on = scene.is_some();
        *self.scene.lock().unwrap_or_else(|e| e.into_inner()) = scene;
        self.visible.store(on, Ordering::Release);
    }
}

/// Makes the atlas for a pixel size (GDI in the runtime, the synthetic font in tests).
pub type Baker = Box<dyn FnMut(u32) -> Font + Send>;

pub struct Service<S: Source, B: Backend> {
    input: Input<S>,
    nav: Navigator,
    menu: Menu,
    backend: B,
    shared: Arc<Shared>,
    bake: Baker,
    font: Option<Arc<Font>>,
    /// what the shown scene was laid out from
    laid: Option<(u64, (u32, u32), bool, bool, u64)>,
    /// bumped by every change of the menu's state
    generation: u64,
    changed: BTreeSet<&'static str>,
    save_to: Option<PathBuf>,
    key_name: String,
}

impl<S: Source, B: Backend> Service<S, B> {
    pub fn new(input: Input<S>, menu: Menu, backend: B, shared: Arc<Shared>, bake: Baker, save_to: Option<PathBuf>, key_name: &str) -> Self {
        Service {
            input,
            nav: Navigator::default(),
            menu,
            backend,
            shared,
            bake,
            font: None,
            laid: None,
            generation: 0,
            changed: BTreeSet::new(),
            save_to,
            key_name: key_name.to_string(),
        }
    }

    pub fn menu(&self) -> &Menu {
        &self.menu
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn input(&mut self) -> &mut Input<S> {
        &mut self.input
    }

    /// One poll.
    pub fn step(&mut self, now: Instant) -> Change {
        let p = self.input.poll();
        let change = p.change;
        *self.shared.title.lock().unwrap_or_else(|e| e.into_inner()) = p.title;
        match change {
            Change::Opened => {
                if let Some(m) = &p.menu {
                    self.nav.reset(m);
                }
                self.menu.sync(&self.backend);
                self.generation += 1;
            }
            Change::Closed => {
                self.closed();
            }
            _ => {}
        }
        if let Some(m) = &p.menu {
            for n in self.nav.update(m, now) {
                match self.menu.nav(n, &mut self.backend) {
                    Outcome::None => continue,
                    Outcome::Close => {
                        self.input.close();
                        self.closed();
                        break;
                    }
                    Outcome::Changed(k) => {
                        self.changed.insert(k);
                    }
                    _ => {}
                }
                self.generation += 1;
            }
        }
        self.refresh();
        change
    }

    fn closed(&mut self) {
        self.generation += 1;
        self.menu.take_dirty();
        if let Err(e) = self.save() {
            eprintln!("overlay   settings not saved: {e}");
        }
    }

    /// Write the keys the menu changed into the `rebuild.toml` it was given.
    fn save(&mut self) -> Result<(), String> {
        let keys = std::mem::take(&mut self.changed);
        let Some(path) = &self.save_to else { return Ok(()) };
        if keys.is_empty() {
            return Ok(());
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut file = Config::parse(&text)?;
        for k in &keys {
            file.set(k, self.menu.config().get(k))?;
        }
        if !text.is_empty() {
            let _ = std::fs::write(path.with_extension("toml.bak"), &text);
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, file.to_toml()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("overlay   saved {} to {}", keys.iter().copied().collect::<Vec<_>>().join(", "), path.display());
        Ok(())
    }

    /// Lay out again when something shown changed; publish.
    fn refresh(&mut self) {
        let open = self.input.phase() == Phase::Open;
        let hud = match std::env::var("REBUILD_HUD").ok().as_deref().map(str::trim) {
            Some("1") => true,
            Some("0") => false,
            _ => self.menu.config().bool("diagnostics.overlay"),
        };
        let Some(size) = self.shared.target().filter(|_| open || hud) else {
            if self.laid.take().is_some() || self.shared.visible() {
                self.shared.show(None);
            }
            return;
        };
        let px = theme::font_px(size.1);
        if self.font.as_ref().is_none_or(|f| f.px != px) {
            self.font = Some(Arc::new((self.bake)(px)));
            self.laid = None;
        }
        let stats = self.backend.stats();
        let key = (stats.seq, size, open, hud, self.generation);
        if self.laid == Some(key) {
            return;
        }
        let font = self.font.clone().expect("font");
        let scene = layout::build(&self.menu, &stats, open, hud, size, font, &self.key_name);
        self.shared.show(Some(Arc::new(scene)));
        self.laid = Some(key);
    }

    /// Poll forever, every [`POLL`].
    pub fn run(mut self) -> ! {
        loop {
            let t = Instant::now();
            self.step(t);
            std::thread::sleep(POLL.saturating_sub(t.elapsed()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Registry;
    use crate::menu::tests::Fake as FakeBackend;
    use rebuild_config::Value;
    use rebuild_input::keyboard::vk;
    use rebuild_input::pad::buttons;
    use rebuild_input::{Fake, PadState, MENU_KEY, NEUTRAL, PAD_CHORD};

    fn service(save_to: Option<PathBuf>) -> (Service<Fake, FakeBackend>, Arc<Shared>) {
        service_with(save_to, Config::default())
    }

    /// The monitor off: only the menu draws.
    fn quiet() -> (Service<Fake, FakeBackend>, Arc<Shared>) {
        let mut c = Config::default();
        c.set("diagnostics.overlay", Value::Bool(false)).unwrap();
        service_with(None, c)
    }

    fn service_with(save_to: Option<PathBuf>, config: Config) -> (Service<Fake, FakeBackend>, Arc<Shared>) {
        let shared = Shared::new();
        let b = FakeBackend::default();
        let menu = Menu::new(config, &b, Registry::default().with_placeholders());
        let input = Input::new(Fake::default()).with_pad_chord(PAD_CHORD);
        (Service::new(input, menu, b, shared.clone(), Box::new(Font::synthetic), save_to, "F10"), shared)
    }

    fn tap(s: &mut Service<Fake, FakeBackend>, t: &mut Instant, vk: u8) {
        s.input().source_mut().press(vk);
        s.step(*t);
        *t += POLL;
        s.input().source_mut().release(vk);
        s.step(*t);
        *t += POLL;
    }

    #[test]
    fn nothing_is_shown_until_the_present_has_a_target_and_the_menu_opens() {
        let (mut s, shared) = quiet();
        let mut t = Instant::now();
        s.step(t);
        assert!(!shared.visible() && shared.scene().is_none());
        tap(&mut s, &mut t, MENU_KEY);
        assert!(!shared.visible(), "no target yet");
        shared.set_target(1920, 1080);
        s.step(t);
        assert!(shared.visible());
        let scene = shared.scene().unwrap();
        assert_eq!(scene.size, (1920, 1080));
        assert_eq!(scene.font.px, theme::font_px(1080));
        // nothing changed: the same scene, not laid out again
        s.step(t);
        assert!(Arc::ptr_eq(&scene, &shared.scene().unwrap()));
        // a new target size: laid out again, a font for the new height
        shared.set_target(3840, 2160);
        s.step(t);
        assert_eq!(shared.scene().unwrap().font.px, theme::font_px(2160));
        tap(&mut s, &mut t, MENU_KEY);
        assert!(!shared.visible() && shared.scene().is_none());
    }

    #[test]
    fn the_title_sees_neutral_input_while_the_menu_is_used() {
        let (mut s, shared) = quiet();
        shared.set_target(1920, 1080);
        let mut t = Instant::now();
        s.input().source_mut().pads[0] = Some(NEUTRAL);
        s.input().source_mut().press(b'W');
        s.step(t);
        assert!(shared.title_input().key(b'W'), "closed: live");
        s.input().source_mut().release(b'W');
        tap(&mut s, &mut t, MENU_KEY);
        // navigate with keys and the pad: none of it reaches the title
        for k in [vk::DOWN, vk::RIGHT] {
            s.input().source_mut().press(k);
            s.step(t);
            let f = shared.title_input();
            assert!(f.keys.is_empty(), "{k:#x} reached the title");
            assert_eq!(f.pads[0], Some(NEUTRAL));
            s.input().source_mut().release(k);
            s.step(t);
            t += POLL;
        }
        s.input().source_mut().pads[0] = Some(PadState { buttons: buttons::X, ..NEUTRAL });
        s.step(t);
        assert_eq!(shared.title_input().pads[0], Some(NEUTRAL));
        // Esc closes; the pad's X is still held, so the title stays neutral until it is up
        s.input().source_mut().press(vk::ESCAPE);
        s.step(t);
        assert_eq!(s.input().phase(), Phase::Draining);
        s.input().source_mut().release(vk::ESCAPE);
        s.step(t);
        assert_eq!(shared.title_input().pads[0], Some(NEUTRAL));
        assert!(!shared.visible());
        s.input().source_mut().pads[0] = Some(NEUTRAL);
        assert_eq!(s.step(t), Change::Released);
        s.input().source_mut().pads[0] = Some(PadState { buttons: buttons::B, ..NEUTRAL });
        s.step(t);
        assert_eq!(shared.title_input().pads[0].unwrap().buttons, buttons::B, "live again");
        // the menu took: Down then Right = Frame rate title -> capped, applied at once
        assert_eq!(s.backend().applied, vec![("display.frame_rate".to_string(), Value::Choice("capped".into()))]);
    }

    #[test]
    fn the_pad_alone_opens_navigates_and_closes() {
        let (mut s, shared) = service(None);
        shared.set_target(1920, 1080);
        let t = Instant::now();
        let pad = |s: &mut Service<Fake, FakeBackend>, b: u32| {
            s.input().source_mut().pads[2] = Some(PadState { buttons: b, ..NEUTRAL });
            s.step(t)
        };
        assert_eq!(pad(&mut s, PAD_CHORD), Change::Opened);
        pad(&mut s, 0);
        pad(&mut s, buttons::A); // Vsync on
        pad(&mut s, 0);
        assert!(s.menu().config().bool("display.vsync"));
        pad(&mut s, buttons::RIGHT_SHOULDER);
        pad(&mut s, 0);
        assert_eq!(s.menu().page(), crate::menu::Page::Stats);
        pad(&mut s, buttons::B); // back at the top level closes
        assert_eq!(s.input().phase(), Phase::Draining);
        assert_eq!(pad(&mut s, 0), Change::Released);
    }

    #[test]
    fn the_monitor_shows_while_closed_by_default_and_the_menu_turns_it_off() {
        let (mut s, shared) = service(None);
        shared.set_target(1920, 1080);
        let mut t = Instant::now();
        s.step(t);
        assert_eq!(s.input().phase(), Phase::Closed);
        let scene = shared.scene().expect("the monitor, on by default");
        assert!(scene.panels[0][0] > 960.0, "top right");
        // the menu's setting turns it off (the fourth setting)
        tap(&mut s, &mut t, MENU_KEY);
        for k in [vk::UP, vk::RETURN] {
            tap(&mut s, &mut t, k);
        }
        assert!(!s.menu().config().bool("diagnostics.overlay"));
        tap(&mut s, &mut t, MENU_KEY);
        assert!(!shared.visible() && shared.scene().is_none(), "off: nothing drawn while closed");
    }

    #[test]
    fn closing_saves_only_the_changed_keys_and_keeps_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("overlay-save-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rebuild.toml");
        let original = "profile = \"mine\"\nfuture_key = 7\n\n[settings]\n\"memory.upload_workers\" = 6\n\"display.frame_cap\" = 60\nunknown = \"kept\"\n";
        std::fs::write(&path, original).unwrap();
        let (mut s, shared) = service(Some(path.clone()));
        shared.set_target(1920, 1080);
        let mut t = Instant::now();
        tap(&mut s, &mut t, MENU_KEY);
        tap(&mut s, &mut t, vk::RETURN); // vsync on
        assert!(std::fs::read_to_string(&path).unwrap() == original, "not saved while open");
        tap(&mut s, &mut t, MENU_KEY);
        let saved = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.bool("display.vsync"));
        assert_eq!(saved.int("memory.upload_workers"), 6);
        assert_eq!(saved.profile, "mine");
        assert!(saved.kept().contains_key("unknown"));
        assert!(saved.to_toml().contains("future_key"));
        assert_eq!(std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(), original);
        // a close with nothing changed writes nothing
        std::fs::remove_file(path.with_extension("toml.bak")).unwrap();
        tap(&mut s, &mut t, MENU_KEY);
        tap(&mut s, &mut t, MENU_KEY);
        assert!(!path.with_extension("toml.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
