//! Menu input to [`Nav`] events, on the input thread (every poll, so no press is missed between frames).
//!
//! Keyboard and pads drive the menu together: every connected pad's buttons, the left stick past half travel as
//! the D-pad, and the keys read through the stand-in pad's table (arrows, Enter, Backspace, Esc, Q / E) even
//! while a real pad is connected. A press is an event once; the four directions repeat while held
//! ([`REPEAT_DELAY`], then every [`REPEAT_EVERY`]). Anything already held when the menu opens is ignored until
//! released, so the key or chord that opened the menu does nothing in it.

use std::time::{Duration, Instant};

use rebuild_input::pad::buttons as b;
use rebuild_input::{keyboard, Frame};

use crate::menu::Nav;

pub const REPEAT_DELAY: Duration = Duration::from_millis(350);
pub const REPEAT_EVERY: Duration = Duration::from_millis(70);
/// Stick travel that counts as a D-pad press.
pub const STICK: f32 = 0.5;

/// Events in the order they are handled within one poll.
const ORDER: [(Nav, u32); 9] = [
    (Nav::Up, b::DPAD_UP),
    (Nav::Down, b::DPAD_DOWN),
    (Nav::Left, b::DPAD_LEFT),
    (Nav::Right, b::DPAD_RIGHT),
    (Nav::Accept, b::A),
    (Nav::Back, b::B),
    (Nav::PrevPage, b::LEFT_SHOULDER),
    (Nav::NextPage, b::RIGHT_SHOULDER),
    (Nav::Close, b::MENU),
];

const DIRECTIONS: u32 = b::DPAD_UP | b::DPAD_DOWN | b::DPAD_LEFT | b::DPAD_RIGHT;

/// Every button held across the keyboard and every pad, sticks folded into the D-pad.
pub fn held(frame: &Frame) -> u32 {
    let mut all = keyboard::stand_in(&frame.keys).buttons;
    for p in frame.pads.iter().flatten() {
        all |= p.buttons;
        if p.left_y > STICK {
            all |= b::DPAD_UP;
        }
        if p.left_y < -STICK {
            all |= b::DPAD_DOWN;
        }
        if p.left_x < -STICK {
            all |= b::DPAD_LEFT;
        }
        if p.left_x > STICK {
            all |= b::DPAD_RIGHT;
        }
    }
    all
}

#[derive(Debug, Default)]
pub struct Navigator {
    prev: u32,
    /// held when the menu opened: ignored until released
    ignored: u32,
    /// when the held direction was pressed, and when it last fired
    since: Option<(u32, Instant, Instant)>,
}

impl Navigator {
    /// The menu opened with `frame`'s buttons down: they do nothing until released.
    pub fn reset(&mut self, frame: &Frame) {
        let h = held(frame);
        *self = Navigator { prev: h, ignored: h, since: None };
    }

    /// One poll's events, in [`ORDER`].
    pub fn update(&mut self, frame: &Frame, now: Instant) -> Vec<Nav> {
        let h = held(frame);
        self.ignored &= h;
        let live = h & !self.ignored;
        let pressed = live & !self.prev;
        self.prev = h;
        let mut fire = pressed;
        // repeat the newest held direction
        let dir = live & DIRECTIONS;
        if pressed & DIRECTIONS != 0 {
            let bit = 1 << (31 - (pressed & DIRECTIONS).leading_zeros());
            self.since = Some((bit, now, now));
        } else if let Some((bit, start, last)) = self.since {
            if dir & bit == 0 {
                self.since = None;
            } else if now.duration_since(start) >= REPEAT_DELAY && now.duration_since(last) >= REPEAT_EVERY {
                fire |= bit;
                self.since = Some((bit, start, now));
            }
        }
        ORDER.iter().filter(|(_, bit)| fire & bit != 0).map(|(n, _)| *n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebuild_input::keyboard::vk;
    use rebuild_input::{KeyState, PadState, NEUTRAL};

    fn keys(k: &[u8]) -> Frame {
        Frame { keys: k.iter().map(|&k| KeyState::vk(k)).collect(), ..Frame::default() }
    }

    fn pad(p: PadState) -> Frame {
        Frame { pads: [Some(p), None, None, None], ..Frame::default() }
    }

    #[test]
    fn keys_and_pads_both_navigate() {
        let t = Instant::now();
        let mut n = Navigator::default();
        assert_eq!(n.update(&keys(&[vk::DOWN]), t), [Nav::Down]);
        assert_eq!(n.update(&keys(&[vk::DOWN]), t), [], "held is one press until the repeat delay");
        assert_eq!(n.update(&keys(&[]), t), []);
        let cases = [
            (vk::UP, Nav::Up),
            (vk::LEFT, Nav::Left),
            (vk::RIGHT, Nav::Right),
            (vk::RETURN, Nav::Accept),
            (vk::BACK, Nav::Back),
            (b'Q', Nav::PrevPage),
            (b'E', Nav::NextPage),
            (vk::ESCAPE, Nav::Close),
        ];
        for (k, want) in cases {
            assert_eq!(n.update(&keys(&[k]), t), [want], "key {k:#x}");
            n.update(&keys(&[]), t);
        }
        // a pad and the keyboard at once: the keyboard still counts
        let mut f = pad(PadState { buttons: b::A, ..NEUTRAL });
        f.keys.push(KeyState::vk(vk::DOWN));
        assert_eq!(n.update(&f, t), [Nav::Down, Nav::Accept]);
        n.update(&Frame::default(), t);
        // the left stick is a D-pad past half travel
        assert_eq!(n.update(&pad(PadState { left_y: 0.4, ..NEUTRAL }), t), []);
        assert_eq!(n.update(&pad(PadState { left_y: -0.9, ..NEUTRAL }), t), [Nav::Down]);
        assert_eq!(n.update(&pad(PadState { buttons: b::RIGHT_SHOULDER, ..NEUTRAL }), t), [Nav::NextPage]);
    }

    #[test]
    fn directions_repeat_after_a_delay() {
        let t = Instant::now();
        let mut n = Navigator::default();
        let down = keys(&[vk::DOWN]);
        assert_eq!(n.update(&down, t), [Nav::Down]);
        assert_eq!(n.update(&down, t + Duration::from_millis(300)), []);
        assert_eq!(n.update(&down, t + Duration::from_millis(350)), [Nav::Down]);
        assert_eq!(n.update(&down, t + Duration::from_millis(400)), []);
        assert_eq!(n.update(&down, t + Duration::from_millis(420)), [Nav::Down]);
        // Accept never repeats
        let mut n = Navigator::default();
        let a = keys(&[vk::RETURN]);
        assert_eq!(n.update(&a, t), [Nav::Accept]);
        assert_eq!(n.update(&a, t + Duration::from_secs(2)), []);
    }

    #[test]
    fn what_was_held_at_open_is_ignored_until_released() {
        let t = Instant::now();
        let mut n = Navigator::default();
        let held = keys(&[vk::RETURN, vk::DOWN]);
        n.reset(&held);
        assert_eq!(n.update(&held, t), []);
        assert_eq!(n.update(&held, t + Duration::from_secs(1)), [], "no repeat either");
        assert_eq!(n.update(&keys(&[vk::RETURN]), t), []);
        assert_eq!(n.update(&keys(&[]), t), []);
        assert_eq!(n.update(&keys(&[vk::RETURN]), t), [Nav::Accept], "a new press counts");
    }
}
