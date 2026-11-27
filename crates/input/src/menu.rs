//! Menu isolation (the fork's debug-menu design, A1).
//!
//! While the in-game menu is open the title sees every connected pad neutral (connected, nothing
//! pressed) and no keys; the menu reads the real input. On close the title stays neutral until every key
//! and button is up, so the key that closed the menu never reaches the title. Rumble is stopped on open,
//! and the title's rumble requests are dropped until the title sees input again. The menu key (`menu.key`
//! in `rebuild.toml`: F10 by default, or F11, F12, Insert, Home) is outside every title mapping and is never
//! delivered to the title.

use crate::keyboard::vk;
use crate::source::{Frame, Source, PADS};

/// Opens and closes the menu unless another is chosen. Not in the stand-in pad's table (a test checks).
pub const MENU_KEY: u8 = vk::F10;

/// The keys `menu.key` may name, by the setting's spelling. None is in the stand-in pad's table (a test checks).
pub const MENU_KEYS: [(&str, u8); 5] = [("F10", vk::F10), ("F11", vk::F11), ("F12", vk::F12), ("Insert", vk::INSERT), ("Home", vk::HOME)];

/// The virtual key a `menu.key` choice names (`F10`, `F11`, `F12`, `Insert`, `Home`; letter case ignored).
pub fn menu_key_named(name: &str) -> Option<u8> {
    MENU_KEYS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name.trim())).map(|&(_, k)| k)
}

/// Where the gate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The title sees live input.
    Closed,
    /// The menu has the input; the title sees neutral.
    Open,
    /// The menu has closed; the title sees neutral until every key and button is up.
    Draining,
}

/// What changed in one poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    None,
    Opened,
    Closed,
    /// Draining ended: the title sees live input again from this poll.
    Released,
}

/// One poll's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Polled {
    /// What the title is given.
    pub title: Frame,
    /// The real input, while the menu is open; `None` otherwise.
    pub menu: Option<Frame>,
    pub change: Change,
}

/// The input every consumer reads: one source, polled once per present, split between title and menu.
pub struct Input<S: Source> {
    source: S,
    phase: Phase,
    menu_key: u8,
    menu_key_was_down: bool,
}

impl<S: Source> Input<S> {
    /// With the default menu key, [`MENU_KEY`].
    pub fn new(source: S) -> Self {
        Self::with_menu_key(source, MENU_KEY)
    }

    /// With the menu key `menu.key` chose ([`menu_key_named`]).
    pub fn with_menu_key(source: S, menu_key: u8) -> Self {
        Input { source, phase: Phase::Closed, menu_key, menu_key_was_down: false }
    }

    pub fn menu_key(&self) -> u8 {
        self.menu_key
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Open the menu (as the menu key does).
    pub fn open(&mut self) {
        if self.phase != Phase::Open {
            self.phase = Phase::Open;
            for slot in 0..PADS {
                self.source.rumble(slot, 0, 0);
            }
        }
    }

    /// Close the menu (as the menu key does, or the menu's own "close").
    pub fn close(&mut self) {
        if self.phase == Phase::Open {
            self.phase = Phase::Draining;
        }
    }

    /// The title asks for rumble. Dropped unless the title currently sees live input.
    pub fn title_rumble(&mut self, slot: usize, low: u16, high: u16) -> bool {
        let live = self.phase == Phase::Closed;
        if live {
            self.source.rumble(slot, low, high);
        }
        live
    }

    /// Poll every device once and split the result.
    pub fn poll(&mut self) -> Polled {
        let raw = Frame::read(&mut self.source);
        let down = raw.key(self.menu_key);
        let pressed = down && !self.menu_key_was_down;
        self.menu_key_was_down = down;

        let mut change = Change::None;
        if pressed {
            if self.phase == Phase::Open {
                self.close();
                change = Change::Closed;
            } else {
                self.open();
                change = Change::Opened;
            }
        }
        if self.phase == Phase::Draining && raw.all_up() {
            self.phase = Phase::Closed;
            change = Change::Released;
        }

        match self.phase {
            Phase::Closed => {
                let mut title = raw;
                title.keys.retain(|k| k.virtual_key != self.menu_key);
                Polled { title, menu: None, change }
            }
            Phase::Open => Polled { title: raw.neutral(), menu: Some(raw), change },
            Phase::Draining => Polled { title: raw.neutral(), menu: None, change },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::{self, vk};
    use crate::pad::{buttons, PadState, NEUTRAL};
    use crate::source::Fake;

    fn input() -> Input<Fake> {
        Input::new(Fake::default())
    }

    /// Press and release the menu key across two polls.
    fn tap_menu(i: &mut Input<Fake>) -> (Polled, Polled) {
        i.source_mut().press(MENU_KEY);
        let a = i.poll();
        i.source_mut().release(MENU_KEY);
        (a, i.poll())
    }

    #[test]
    fn closed_is_live_and_hides_the_menu_key() {
        let mut i = input();
        i.source_mut().press(vk::RETURN);
        let p = i.poll();
        assert_eq!(p.change, Change::None);
        assert_eq!(p.title.stand_in.unwrap().buttons, buttons::A);
        assert_eq!(p.menu, None);
        assert!(i.title_rumble(0, 100, 200));
        assert_eq!(i.source_mut().rumbles, vec![(0, 100, 200)]);
    }

    #[test]
    fn open_gives_title_neutral_and_menu_the_real_input() {
        let mut i = input();
        i.source_mut().pads[0] = Some(PadState { buttons: buttons::A, right_trigger: 1.0, ..NEUTRAL });
        i.source_mut().press(b'W');
        let (opened, held) = tap_menu(&mut i);
        assert_eq!(opened.change, Change::Opened);
        assert_eq!(i.phase(), Phase::Open);
        for p in [&opened, &held] {
            assert_eq!(p.title.pads, [Some(NEUTRAL), None, None, None], "connected, released");
            assert!(p.title.keys.is_empty());
            assert_eq!(p.title.stand_in, None);
        }
        let menu = held.menu.unwrap();
        assert_eq!(menu.pads[0].unwrap().buttons, buttons::A);
        assert!(menu.key(b'W'));
        // Rumble stopped on every slot when the menu opened; the title's rumble is dropped meanwhile.
        assert_eq!(i.source_mut().rumbles, (0..PADS).map(|s| (s, 0, 0)).collect::<Vec<_>>());
        assert!(!i.title_rumble(0, 9, 9));
        assert_eq!(i.source_mut().rumbles.len(), PADS);
    }

    #[test]
    fn hold_while_open_never_reaches_the_title() {
        let mut i = input();
        tap_menu(&mut i);
        // Pressed and released entirely inside the menu.
        for _ in 0..3 {
            i.source_mut().press(vk::RETURN);
            let p = i.poll();
            assert_eq!(p.title.stand_in, Some(NEUTRAL));
            assert!(p.menu.unwrap().key(vk::RETURN));
        }
        i.source_mut().release(vk::RETURN);
        assert_eq!(i.poll().title.stand_in, Some(NEUTRAL));
    }

    #[test]
    fn close_while_held_stays_neutral_until_all_up() {
        let mut i = input();
        i.source_mut().pads[3] = Some(NEUTRAL);
        tap_menu(&mut i);
        // The menu's "confirm and close": A on the pad and Enter held while the menu closes.
        i.source_mut().pads[3] = Some(PadState { buttons: buttons::A, ..NEUTRAL });
        i.source_mut().press(vk::RETURN);
        i.poll();
        i.close();
        assert_eq!(i.phase(), Phase::Draining);
        for _ in 0..5 {
            let p = i.poll();
            assert_eq!(p.title.pads[3], Some(NEUTRAL));
            assert!(p.title.keys.is_empty());
            assert_eq!(p.menu, None);
            assert!(!i.title_rumble(3, 1, 1));
        }
        i.source_mut().release(vk::RETURN);
        assert_eq!(i.poll().change, Change::None, "the pad button is still down");
        // A trigger resting slightly pressed counts as up; a held one does not.
        i.source_mut().pads[3] = Some(PadState { left_trigger: 0.5, ..NEUTRAL });
        i.poll();
        assert_eq!(i.phase(), Phase::Draining);
        i.source_mut().pads[3] = Some(PadState { left_trigger: 0.05, left_x: 0.3, ..NEUTRAL });
        let p = i.poll();
        assert_eq!(p.change, Change::Released);
        assert_eq!(i.phase(), Phase::Closed);
        assert_eq!(p.title.pads[3].unwrap().left_x, 0.3, "live again in the same poll");
        // A later press is the title's.
        i.source_mut().pads[3] = Some(PadState { buttons: buttons::B, ..NEUTRAL });
        assert_eq!(i.poll().title.pads[3].unwrap().buttons, buttons::B);
        assert!(i.title_rumble(3, 5, 6));
    }

    #[test]
    fn closing_menu_key_is_swallowed() {
        let mut i = input();
        tap_menu(&mut i);
        i.source_mut().press(MENU_KEY);
        let p = i.poll();
        assert_eq!(p.change, Change::Closed);
        assert_eq!(i.phase(), Phase::Draining, "F10 still held");
        assert!(p.title.keys.is_empty());
        i.source_mut().release(MENU_KEY);
        let p = i.poll();
        assert_eq!(p.change, Change::Released);
        assert!(p.title.keys.is_empty());
    }

    #[test]
    fn menu_key_reopens_while_draining_and_held_key_does_not_retoggle() {
        let mut i = input();
        tap_menu(&mut i);
        i.source_mut().press(vk::BACK);
        i.close();
        i.poll();
        assert_eq!(i.phase(), Phase::Draining);
        i.source_mut().press(MENU_KEY);
        assert_eq!(i.poll().change, Change::Opened);
        // Holding F10 over many polls toggles once.
        for _ in 0..4 {
            assert_eq!(i.poll().change, Change::None);
            assert_eq!(i.phase(), Phase::Open);
        }
    }

    #[test]
    fn the_chosen_menu_key_toggles_and_f10_is_the_titles() {
        assert_eq!(menu_key_named("F10"), Some(MENU_KEY));
        assert_eq!(menu_key_named("insert"), Some(vk::INSERT));
        assert_eq!(menu_key_named("F9"), None);
        let mut i = Input::with_menu_key(Fake::default(), menu_key_named("Home").unwrap());
        assert_eq!(i.menu_key(), vk::HOME);
        // F10 is an ordinary key now: it reaches the title and opens nothing.
        i.source_mut().press(vk::F10);
        let p = i.poll();
        assert_eq!(p.change, Change::None);
        assert!(p.title.key(vk::F10));
        i.source_mut().release(vk::F10);
        i.poll();
        // Home opens and closes the menu and never reaches the title.
        i.source_mut().press(vk::HOME);
        let p = i.poll();
        assert_eq!(p.change, Change::Opened);
        assert!(!p.title.key(vk::HOME));
        i.source_mut().release(vk::HOME);
        i.poll();
        i.source_mut().press(vk::HOME);
        assert_eq!(i.poll().change, Change::Closed);
        i.source_mut().release(vk::HOME);
        let p = i.poll();
        assert_eq!(p.change, Change::Released);
        assert!(p.title.keys.is_empty());
    }

    #[test]
    fn all_up_is_immediate_when_nothing_is_held() {
        let mut i = input();
        tap_menu(&mut i);
        i.close();
        let p = i.poll();
        assert_eq!(p.change, Change::Released);
        assert_eq!(p.title.stand_in, Some(keyboard::stand_in(&[])));
    }
}
