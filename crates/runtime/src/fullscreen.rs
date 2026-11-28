//! Exclusive full screen (`display.window = fullscreen`, `REBUILD_WINDOW=fullscreen`): the state machine, the
//! display-mode choice and the present flags, behind a seam ([`Display`]) that the presenter implements with DXGI
//! (`present.rs`, `DxgiDisplay`) and the tests with a mock. Nothing here touches the screen.
//!
//! **The decision** (agents/fullscreen.md has the numbers). D3D12 has only flip-model swap chains. On Windows 10/11 a
//! flip-model swap chain whose window covers its monitor with nothing over it is shown by *independent flip*
//! (the DWM steps aside: the same latency as exclusive, tearing with `DXGI_PRESENT_ALLOW_TEARING`); that is the
//! **windowed fullscreen** mode (`borderless`), unchanged. **Fullscreen** adds what only `SetFullscreenState(TRUE)`
//! gives on top: a **display mode** of the title's own image size when the monitor has one (the display's scaler
//! does the scaling: no compositor pass, no blit, no `SetSourceSize` region for the DWM or an overlay plane to
//! scale), and the output owned by the swap chain, so nothing else (a notification, a volume popup) can knock the
//! frame back to composition. With the image at the desktop's size both give independent flip; exclusive never
//! gives less. The costs are DXGI's rules, handled here:
//!
//! - `DXGI_PRESENT_ALLOW_TEARING` is invalid while exclusive: vsync off presents `Present(0, 0)`, which tears there
//!   ([`present_flags`]);
//! - leave full screen before the swap chain is released (shutdown, device loss);
//! - after each transition the buffers are resized (same size, same flags) before the next present;
//! - focus loss (Alt+Tab, Win key): leave and minimize, so the desktop gets its mode back at once; focus regained:
//!   restore and enter again;
//! - Alt+Enter toggles exclusive ⇄ borderless over the same monitor (DXGI's own Alt+Enter is off:
//!   `DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES`, so every transition is this machine's);
//! - the monitor is the one the window is on (`IDXGISwapChain::GetContainingOutput`), its modes from that output.

/// What the window or the present reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The first present: go exclusive if the window has the focus.
    Start,
    FocusGained,
    FocusLost,
    AltEnter,
    /// `Present` said `DXGI_STATUS_OCCLUDED`, or a periodic check: DXGI may have taken full screen away (another
    /// exclusive app, the secure desktop).
    Check,
    /// The device was removed or reset: leave full screen for good.
    DeviceLost,
    /// The swap chain is about to be released.
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Wants exclusive, not there yet (no focus, or DXGI said "not now"): retried on focus or [`Event::Check`].
    Pending,
    Exclusive,
    /// Was exclusive; the focus went away: left, the window minimized. Focus back: restore and enter.
    Suspended,
    /// Alt+Enter (or entering failed): a borderless window over the monitor. Alt+Enter again: exclusive.
    Borderless,
    /// Device lost or shut down: no more transitions.
    Off,
}

/// The result of asking for exclusive full screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enter {
    Entered,
    /// `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` or `DXGI_STATUS_MODE_CHANGE_IN_PROGRESS`: try again later.
    NotNow,
    /// Anything else: stay borderless.
    Failed,
}

/// The display operations the machine needs. DXGI's (`present.rs`) or a mock's.
pub trait Display {
    /// `ResizeTarget(mode)`, `SetFullscreenState(TRUE, output)`, `ResizeTarget` again with the refresh zeroed.
    fn enter(&mut self) -> Enter;
    /// `SetFullscreenState(FALSE)`.
    fn leave(&mut self);
    /// `GetFullscreenState`.
    fn is_exclusive(&mut self) -> bool;
    /// `ResizeBuffers` at the same size and flags, before the next present.
    fn resize_buffers(&mut self);
    fn minimize(&mut self);
    /// Show, bring to the front and cover the window's monitor.
    fn restore(&mut self);
    /// The window has the focus (is the foreground window).
    fn focused(&mut self) -> bool;
}

/// The machine: [`State`] and the [`Display`] it drives.
pub struct Controller<D: Display> {
    pub display: D,
    state: State,
    /// transitions done (enter or leave), for the log and the tests
    pub transitions: u32,
}

impl<D: Display> Controller<D> {
    pub fn new(display: D) -> Self {
        Controller { display, state: State::Pending, transitions: 0 }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The swap chain owns the output: present without `DXGI_PRESENT_ALLOW_TEARING`.
    pub fn exclusive(&self) -> bool {
        self.state == State::Exclusive
    }

    fn try_enter(&mut self) {
        if !self.display.focused() {
            self.state = State::Pending;
            return;
        }
        self.state = match self.display.enter() {
            Enter::Entered => {
                self.transitions += 1;
                self.display.resize_buffers();
                State::Exclusive
            }
            Enter::NotNow => State::Pending,
            Enter::Failed => {
                self.display.restore();
                State::Borderless
            }
        };
    }

    fn leave(&mut self) {
        self.display.leave();
        self.transitions += 1;
        self.display.resize_buffers();
    }

    /// One event; the state after it.
    pub fn handle(&mut self, e: Event) -> State {
        use {Event as E, State as S};
        match (self.state, e) {
            (S::Off, _) => {}
            (S::Exclusive, E::Shutdown | E::DeviceLost) => {
                // DXGI: never release a swap chain in full screen
                self.leave();
                self.state = S::Off;
            }
            (_, E::Shutdown | E::DeviceLost) => self.state = S::Off,
            (S::Pending, E::Start | E::FocusGained | E::Check) => self.try_enter(),
            (S::Suspended, E::FocusGained) => {
                self.display.restore();
                self.try_enter();
            }
            // a focus loss already undone by the time it is handled (the mode change itself can send one) is ignored
            (S::Exclusive, E::FocusLost) if !self.display.focused() => {
                self.leave();
                self.display.minimize();
                self.state = S::Suspended;
            }
            (S::Exclusive, E::Check | E::FocusGained) => {
                // taken away behind our back (another exclusive app, the secure desktop): buffers back to windowed,
                // and enter again once we have the focus
                if !self.display.is_exclusive() {
                    self.transitions += 1;
                    self.display.resize_buffers();
                    self.try_enter();
                }
            }
            (S::Exclusive, E::AltEnter) => {
                self.leave();
                self.display.restore();
                self.state = S::Borderless;
            }
            (S::Borderless, E::AltEnter) => self.try_enter(),
            (S::Pending | S::Suspended, E::AltEnter) => {
                self.display.restore();
                self.state = S::Borderless;
            }
            // borderless stays borderless on focus changes (windowed fullscreen's behaviour); a pending one waits
            _ => {}
        }
        self.state
    }
}

/// `Present`'s sync interval and flags: vsync on = (1, 0); off = (0, ALLOW_TEARING) when the swap chain allows
/// tearing and is not exclusive (DXGI refuses the flag in exclusive full screen, where (0, 0) tears by itself).
pub fn present_flags(vsync: bool, tearing: bool, exclusive: bool) -> (u32, bool) {
    if vsync {
        (1, false)
    } else {
        (0, tearing && !exclusive)
    }
}

/// A display mode: size and refresh rate (numerator / denominator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeDesc {
    pub width: u32,
    pub height: u32,
    pub refresh: (u32, u32),
}

impl ModeDesc {
    pub fn hz(&self) -> f64 {
        if self.refresh.1 == 0 {
            0.0
        } else {
            f64::from(self.refresh.0) / f64::from(self.refresh.1)
        }
    }
}

/// The full-screen mode: one of the output's `modes` of the title's image size (`want`), else of the desktop's
/// (`native`, with its refresh `native_hz`). Among modes of that size, the desktop's refresh rate when the size
/// has it (what the user set; a title paced at 60 stays even on a 60-multiple), else the highest. No mode of
/// either size listed (or no list): `native` itself.
pub fn pick_mode(modes: &[ModeDesc], want: (u32, u32), native: (u32, u32), native_hz: f64) -> ModeDesc {
    let of = |size: (u32, u32)| -> Option<ModeDesc> {
        let sized: Vec<&ModeDesc> = modes.iter().filter(|m| (m.width, m.height) == size && m.hz() > 0.0).collect();
        if sized.is_empty() {
            return None;
        }
        // the closest to the desktop's (60 before 59.94 for a 60 Hz desktop), within half a hertz
        sized
            .iter()
            .filter(|m| native_hz > 0.0 && (m.hz() - native_hz).abs() < 0.5)
            .min_by(|a, b| (a.hz() - native_hz).abs().total_cmp(&(b.hz() - native_hz).abs()))
            .or_else(|| sized.iter().max_by(|a, b| a.hz().total_cmp(&b.hz())))
            .map(|m| **m)
    };
    of(want).or_else(|| of(native)).unwrap_or(ModeDesc { width: native.0, height: native.1, refresh: (native_hz.round().max(0.0) as u32, u32::from(native_hz > 0.0)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every display call; `enter` answers from a script (then `Entered`).
    #[derive(Default)]
    struct Mock {
        calls: Vec<&'static str>,
        answers: Vec<Enter>,
        exclusive: bool,
        unfocused: bool,
    }

    impl Display for Mock {
        fn enter(&mut self) -> Enter {
            self.calls.push("enter");
            let a = if self.answers.is_empty() { Enter::Entered } else { self.answers.remove(0) };
            self.exclusive = a == Enter::Entered;
            a
        }
        fn leave(&mut self) {
            self.calls.push("leave");
            self.exclusive = false;
        }
        fn is_exclusive(&mut self) -> bool {
            self.exclusive
        }
        fn resize_buffers(&mut self) {
            self.calls.push("resize");
        }
        fn minimize(&mut self) {
            self.calls.push("minimize");
        }
        fn restore(&mut self) {
            self.calls.push("restore");
        }
        fn focused(&mut self) -> bool {
            !self.unfocused
        }
    }

    fn take(c: &mut Controller<Mock>) -> Vec<&'static str> {
        std::mem::take(&mut c.display.calls)
    }

    #[test]
    fn start_enters_then_resizes_and_tearing_is_off_while_exclusive() {
        let mut c = Controller::new(Mock::default());
        assert_eq!(c.state(), State::Pending);
        assert_eq!(c.handle(Event::Start), State::Exclusive);
        assert_eq!(take(&mut c), ["enter", "resize"]);
        assert!(c.exclusive());
        assert_eq!(present_flags(false, true, c.exclusive()), (0, false), "no ALLOW_TEARING in exclusive");
        assert_eq!(present_flags(false, true, false), (0, true));
        assert_eq!(present_flags(true, true, false), (1, false));
        assert_eq!(present_flags(false, false, false), (0, false));
    }

    #[test]
    fn focus_loss_leaves_and_minimizes_and_focus_restores_and_enters() {
        let mut c = Controller::new(Mock::default());
        c.handle(Event::Start);
        take(&mut c);
        // a loss already over (the window is foreground again when it is handled): nothing
        assert_eq!(c.handle(Event::FocusLost), State::Exclusive);
        assert!(take(&mut c).is_empty());
        c.display.unfocused = true;
        assert_eq!(c.handle(Event::FocusLost), State::Suspended);
        assert_eq!(take(&mut c), ["leave", "resize", "minimize"]);
        assert!(!c.display.exclusive);
        // a second loss changes nothing
        assert_eq!(c.handle(Event::FocusLost), State::Suspended);
        assert!(take(&mut c).is_empty());
        c.display.unfocused = false;
        assert_eq!(c.handle(Event::FocusGained), State::Exclusive);
        assert_eq!(take(&mut c), ["restore", "enter", "resize"]);
        assert_eq!(c.transitions, 3);
    }

    #[test]
    fn without_focus_it_waits_and_not_now_is_retried() {
        let mut c = Controller::new(Mock { unfocused: true, ..Default::default() });
        assert_eq!(c.handle(Event::Start), State::Pending);
        assert!(take(&mut c).is_empty(), "never enters without the focus");
        c.display.unfocused = false;
        c.display.answers = vec![Enter::NotNow];
        assert_eq!(c.handle(Event::FocusGained), State::Pending);
        assert_eq!(take(&mut c), ["enter"]);
        assert_eq!(c.handle(Event::Check), State::Exclusive, "the periodic check retries");
        assert_eq!(take(&mut c), ["enter", "resize"]);
    }

    #[test]
    fn alt_enter_toggles_exclusive_and_borderless() {
        let mut c = Controller::new(Mock::default());
        c.handle(Event::Start);
        take(&mut c);
        assert_eq!(c.handle(Event::AltEnter), State::Borderless);
        assert_eq!(take(&mut c), ["leave", "resize", "restore"]);
        assert!(!c.exclusive());
        // borderless keeps its window on focus changes, like windowed fullscreen
        assert_eq!(c.handle(Event::FocusLost), State::Borderless);
        assert_eq!(c.handle(Event::FocusGained), State::Borderless);
        assert!(take(&mut c).is_empty());
        assert_eq!(c.handle(Event::AltEnter), State::Exclusive);
        assert_eq!(take(&mut c), ["enter", "resize"]);
        // Alt+Enter while still waiting to enter: the user wants the window
        let mut p = Controller::new(Mock { unfocused: true, ..Default::default() });
        p.handle(Event::Start);
        assert_eq!(p.handle(Event::AltEnter), State::Borderless);
    }

    #[test]
    fn a_failed_enter_stays_borderless_and_alt_enter_retries() {
        let mut c = Controller::new(Mock { answers: vec![Enter::Failed], ..Default::default() });
        assert_eq!(c.handle(Event::Start), State::Borderless);
        assert_eq!(take(&mut c), ["enter", "restore"]);
        assert_eq!(c.handle(Event::AltEnter), State::Exclusive);
    }

    #[test]
    fn full_screen_taken_away_resizes_and_enters_again() {
        let mut c = Controller::new(Mock::default());
        c.handle(Event::Start);
        take(&mut c);
        // still exclusive: a check costs nothing
        assert_eq!(c.handle(Event::Check), State::Exclusive);
        assert!(take(&mut c).is_empty());
        c.display.exclusive = false; // another app took the output
        c.display.unfocused = true;
        assert_eq!(c.handle(Event::Check), State::Pending);
        assert_eq!(take(&mut c), ["resize"]);
        c.display.unfocused = false;
        assert_eq!(c.handle(Event::FocusGained), State::Exclusive);
    }

    #[test]
    fn device_loss_and_shutdown_leave_first_and_end_it() {
        for e in [Event::DeviceLost, Event::Shutdown] {
            let mut c = Controller::new(Mock::default());
            c.handle(Event::Start);
            take(&mut c);
            assert_eq!(c.handle(e), State::Off);
            assert_eq!(take(&mut c), ["leave", "resize"], "{e:?}: left before release");
            for e in [Event::Start, Event::FocusGained, Event::AltEnter, Event::Check] {
                assert_eq!(c.handle(e), State::Off);
            }
            assert!(take(&mut c).is_empty());
        }
        // not exclusive: nothing to leave
        let mut c = Controller::new(Mock::default());
        assert_eq!(c.handle(Event::Shutdown), State::Off);
        assert!(take(&mut c).is_empty());
    }

    #[test]
    fn the_mode_is_the_images_size_at_the_desktops_refresh_else_the_desktops() {
        let m = |w, h, n, d| ModeDesc { width: w, height: h, refresh: (n, d) };
        let modes = [m(1920, 1080, 60000, 1001), m(1920, 1080, 60, 1), m(1920, 1080, 144, 1), m(2560, 1440, 60, 1), m(2560, 1440, 165, 1), m(1280, 720, 0, 0)];
        assert_eq!(pick_mode(&modes, (1920, 1080), (2560, 1440), 60.0), m(1920, 1080, 60, 1), "the desktop's 60 (not 59.94)");
        assert_eq!(pick_mode(&modes, (1920, 1080), (2560, 1440), 165.0), m(1920, 1080, 144, 1), "no 165 at 1080p: the highest");
        assert_eq!(pick_mode(&modes, (3840, 2160), (2560, 1440), 165.0), m(2560, 1440, 165, 1), "no 4K mode: the desktop's");
        assert_eq!(pick_mode(&modes, (1280, 720), (2560, 1440), 60.0), m(2560, 1440, 60, 1), "a mode without a refresh is skipped");
        assert_eq!(pick_mode(&[], (1920, 1080), (2560, 1440), 60.0), m(2560, 1440, 60, 1), "no list: the desktop");
        assert_eq!(pick_mode(&[], (1920, 1080), (2560, 1440), 0.0), m(2560, 1440, 0, 0));
    }
}
