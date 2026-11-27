//! Where input comes from (the host, or a fake in tests) and one polled `Frame` of it.

use crate::keyboard;
use crate::pad::{KeyState, PadState, NEUTRAL};

/// Host pad slots.
pub const PADS: usize = 4;

/// Host input. The host implementation is `host::Host`; tests use `Fake`.
pub trait Source: Send {
    /// The pad in host slot `slot` (0-3), `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while this process's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// Motor speeds, 0 to 65535: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
}

/// One poll: every pad slot, the keyboard's stand-in pad, the keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub pads: [Option<PadState>; PADS],
    /// The keyboard's stand-in pad; present (connected) only while no real pad is.
    pub stand_in: Option<PadState>,
    pub keys: Vec<KeyState>,
}

impl Frame {
    /// Read every device once.
    pub fn read(source: &mut dyn Source) -> Frame {
        let pads: [Option<PadState>; PADS] = std::array::from_fn(|slot| source.pad(slot));
        let keys = source.keys();
        let stand_in = pads.iter().all(Option::is_none).then(|| keyboard::stand_in(&keys));
        Frame { pads, stand_in, keys }
    }

    /// The same devices, connected as they are, with nothing pressed and no keys.
    pub fn neutral(&self) -> Frame {
        Frame {
            pads: self.pads.map(|p| p.map(|_| NEUTRAL)),
            stand_in: self.stand_in.map(|_| NEUTRAL),
            keys: Vec::new(),
        }
    }

    /// No key down and every connected pad released (`PadState::released`).
    pub fn all_up(&self) -> bool {
        self.keys.is_empty() && self.pads.iter().chain([&self.stand_in]).flatten().all(PadState::released)
    }

    /// Is this virtual key down?
    pub fn key(&self, vk: u8) -> bool {
        self.keys.iter().any(|k| k.virtual_key == vk)
    }
}

/// A scripted source for tests: set `pads` and `keys`, read back `rumbles`.
#[derive(Clone, Debug, Default)]
pub struct Fake {
    pub pads: [Option<PadState>; PADS],
    pub keys: Vec<KeyState>,
    /// Every rumble call, in order: (slot, low, high).
    pub rumbles: Vec<(usize, u16, u16)>,
}

impl Fake {
    pub fn press(&mut self, vk: u8) {
        if !self.keys.iter().any(|k| k.virtual_key == vk) {
            self.keys.push(KeyState::vk(vk));
        }
    }
    pub fn release(&mut self, vk: u8) {
        self.keys.retain(|k| k.virtual_key != vk);
    }
}

impl Source for Fake {
    fn pad(&mut self, slot: usize) -> Option<PadState> {
        self.pads.get(slot).copied().flatten()
    }
    fn keys(&mut self) -> Vec<KeyState> {
        self.keys.clone()
    }
    fn rumble(&mut self, slot: usize, low: u16, high: u16) {
        self.rumbles.push((slot, low, high));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::buttons;

    #[test]
    fn stand_in_only_without_a_pad() {
        let mut fake = Fake::default();
        fake.press(keyboard::vk::RETURN);
        let f = Frame::read(&mut fake);
        assert_eq!(f.pads, [None; PADS]);
        assert_eq!(f.stand_in.unwrap().buttons, buttons::A);
        assert!(!f.all_up());

        fake.pads[2] = Some(PadState { buttons: buttons::B, ..NEUTRAL });
        let f = Frame::read(&mut fake);
        assert_eq!(f.stand_in, None, "a real pad replaces the stand-in");
        assert_eq!(f.pads[2].unwrap().buttons, buttons::B);
        assert_eq!(f.keys.len(), 1, "the keyboard stays a device of its own");
    }

    #[test]
    fn neutral_keeps_connections() {
        let mut fake = Fake::default();
        fake.pads[1] = Some(PadState { buttons: buttons::Y, left_trigger: 1.0, left_x: -1.0, ..NEUTRAL });
        fake.press(b'W');
        let n = Frame::read(&mut fake).neutral();
        assert_eq!(n.pads, [None, Some(NEUTRAL), None, None]);
        assert_eq!(n.stand_in, None);
        assert!(n.keys.is_empty() && n.all_up());
    }
}
