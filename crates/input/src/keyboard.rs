//! The keyboard stand-in pad: one gamepad driven by keys, used while no real pad is connected.
//!
//! The mapping is the one the fork documents (README, "Running it"): W A S D left stick, I J K L right
//! stick, arrows D-pad, Enter / Backspace / X / Y face buttons, Q / E shoulders, Z / C triggers,
//! Esc / Tab Menu / View. F and H click the left and right sticks (the fork's table has them; its README
//! does not list them).

use crate::pad::{buttons as b, KeyState, PadState};

/// Windows virtual keys used here.
pub mod vk {
    pub const BACK: u8 = 0x08;
    pub const TAB: u8 = 0x09;
    pub const RETURN: u8 = 0x0D;
    pub const ESCAPE: u8 = 0x1B;
    pub const LEFT: u8 = 0x25;
    pub const UP: u8 = 0x26;
    pub const RIGHT: u8 = 0x27;
    pub const DOWN: u8 = 0x28;
    pub const F10: u8 = 0x79;
}

/// Key → button.
pub const BUTTON_KEYS: [(u8, u32); 14] = [
    (vk::ESCAPE, b::MENU),
    (vk::TAB, b::VIEW),
    (vk::RETURN, b::A),
    (vk::BACK, b::B),
    (b'X', b::X),
    (b'Y', b::Y),
    (vk::UP, b::DPAD_UP),
    (vk::DOWN, b::DPAD_DOWN),
    (vk::LEFT, b::DPAD_LEFT),
    (vk::RIGHT, b::DPAD_RIGHT),
    (b'Q', b::LEFT_SHOULDER),
    (b'E', b::RIGHT_SHOULDER),
    (b'F', b::LEFT_THUMB),
    (b'H', b::RIGHT_THUMB),
];

/// Axis keys: (negative, positive) for left x, left y, right x, right y, and the two trigger keys.
pub const LEFT_X: (u8, u8) = (b'A', b'D');
pub const LEFT_Y: (u8, u8) = (b'S', b'W');
pub const RIGHT_X: (u8, u8) = (b'J', b'L');
pub const RIGHT_Y: (u8, u8) = (b'K', b'I');
pub const LEFT_TRIGGER: u8 = b'Z';
pub const RIGHT_TRIGGER: u8 = b'C';

/// Every key the stand-in pad reads.
pub fn mapped_keys() -> impl Iterator<Item = u8> {
    BUTTON_KEYS.iter().map(|(k, _)| *k).chain([
        LEFT_X.0,
        LEFT_X.1,
        LEFT_Y.0,
        LEFT_Y.1,
        RIGHT_X.0,
        RIGHT_X.1,
        RIGHT_Y.0,
        RIGHT_Y.1,
        LEFT_TRIGGER,
        RIGHT_TRIGGER,
    ])
}

/// The stand-in pad for the keys held now.
pub fn stand_in(keys: &[KeyState]) -> PadState {
    let down = |k: u8| keys.iter().any(|s| s.virtual_key == k);
    let level = |k: u8| if down(k) { 1.0 } else { 0.0 };
    let axis = |(neg, pos): (u8, u8)| level(pos) - level(neg);
    PadState {
        buttons: BUTTON_KEYS.iter().filter(|(k, _)| down(*k)).fold(0, |all, (_, bit)| all | bit),
        left_trigger: level(LEFT_TRIGGER),
        right_trigger: level(RIGHT_TRIGGER),
        left_x: axis(LEFT_X),
        left_y: axis(LEFT_Y),
        right_x: axis(RIGHT_X),
        right_y: axis(RIGHT_Y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::NEUTRAL;

    fn keys(vks: &[u8]) -> Vec<KeyState> {
        vks.iter().map(|&k| KeyState::vk(k)).collect()
    }

    #[test]
    fn documented_mapping() {
        assert_eq!(stand_in(&[]), NEUTRAL);
        let cases: [(u8, u32); 12] = [
            (vk::RETURN, b::A),
            (vk::BACK, b::B),
            (b'X', b::X),
            (b'Y', b::Y),
            (vk::UP, b::DPAD_UP),
            (vk::DOWN, b::DPAD_DOWN),
            (vk::LEFT, b::DPAD_LEFT),
            (vk::RIGHT, b::DPAD_RIGHT),
            (b'Q', b::LEFT_SHOULDER),
            (b'E', b::RIGHT_SHOULDER),
            (vk::ESCAPE, b::MENU),
            (vk::TAB, b::VIEW),
        ];
        for (k, bit) in cases {
            assert_eq!(stand_in(&keys(&[k])).buttons, bit, "key {k:#x}");
        }
        let p = stand_in(&keys(&[b'W', b'D', b'J', b'K', b'Z', b'C']));
        assert_eq!((p.left_x, p.left_y, p.right_x, p.right_y), (1.0, 1.0, -1.0, -1.0));
        assert_eq!((p.left_trigger, p.right_trigger), (1.0, 1.0));
        assert_eq!(p.buttons, 0);
        // Opposite keys cancel.
        let p = stand_in(&keys(&[b'A', b'D', b'I', b'K']));
        assert_eq!((p.left_x, p.right_y), (0.0, 0.0));
    }

    #[test]
    fn no_key_mapped_twice_and_menu_key_free() {
        let all: Vec<u8> = mapped_keys().collect();
        for (i, k) in all.iter().enumerate() {
            assert!(!all[i + 1..].contains(k), "key {k:#x} mapped twice");
        }
        assert_eq!(all.len(), 24);
        assert!(!all.contains(&vk::F10));
    }
}
