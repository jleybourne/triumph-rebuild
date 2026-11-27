//! The console's GameInput API as the title reaches it, mapped onto `PadState` and `Frame` (task I3).
//!
//! The title imports exactly one function from `gameinput.dll`: ordinal 1, `GameInputCreate` (lane
//! `loader`'s census). Everything else is a method call on the objects it returns, so the methods are
//! listed by interface; `agents/input.md` has the full table. This module holds what the runtime needs to
//! answer those calls from a `Frame`: the input-kind bits, the device-status bits, and the reading layouts
//! (a test checks them against the console's structures).

use crate::pad::{KeyState, PadState};
use crate::source::{Frame, PADS};

/// `gameinput.dll` imports in the title: (ordinal, name).
pub const IMPORTS: [(u16, &str); 1] = [(1, "GameInputCreate")];

/// `GameInputKind` bits our devices report.
pub mod kind {
    pub const KEYBOARD: u32 = 0x10;
    pub const GAMEPAD: u32 = 0x4_0000;
}

/// `GameInputDeviceStatus`: connected, input enabled.
pub const STATUS_CONNECTED: u32 = 0x3;
pub const STATUS_NONE: u32 = 0;

/// The devices the runtime exposes, in a fixed order: the four host slots, the stand-in pad, the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Pad(usize),
    StandIn,
    Keyboard,
}

pub const DEVICES: [Device; PADS + 2] =
    [Device::Pad(0), Device::Pad(1), Device::Pad(2), Device::Pad(3), Device::StandIn, Device::Keyboard];

impl Device {
    pub fn kind(self) -> u32 {
        match self {
            Device::Keyboard => kind::KEYBOARD,
            _ => kind::GAMEPAD,
        }
    }

    /// `IGameInputDevice::GetDeviceStatus`.
    pub fn status(self, frame: &Frame) -> u32 {
        let connected = match self {
            Device::Pad(slot) => frame.pads[slot].is_some(),
            Device::StandIn => frame.stand_in.is_some(),
            Device::Keyboard => true,
        };
        if connected {
            STATUS_CONNECTED
        } else {
            STATUS_NONE
        }
    }

    /// `IGameInputReading::GetGamepadState`: the state, or `None` (the call returns false).
    pub fn gamepad(self, frame: &Frame) -> Option<PadState> {
        match self {
            Device::Pad(slot) => frame.pads[slot],
            Device::StandIn => frame.stand_in,
            Device::Keyboard => None,
        }
    }

    /// `IGameInputReading::GetKeyCount` / `GetKeyState`: the keys, empty for a pad.
    pub fn keys(self, frame: &Frame) -> &[KeyState] {
        match self {
            Device::Keyboard => &frame.keys,
            _ => &[],
        }
    }
}

/// `IGameInput::GetCurrentReading(kind, device = null)`: the first connected device of that kind.
pub fn first_of(kind_mask: u32, frame: &Frame) -> Option<Device> {
    DEVICES.into_iter().find(|d| d.kind() & kind_mask != 0 && d.status(frame) == STATUS_CONNECTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pad::{buttons, NEUTRAL};
    use crate::source::Fake;
    use std::mem::{offset_of, size_of};

    /// `GameInputGamepadState` and `GameInputKeyState` field by field.
    #[test]
    fn layouts_match_the_console() {
        assert_eq!(size_of::<PadState>(), 28);
        assert_eq!(offset_of!(PadState, buttons), 0);
        assert_eq!(offset_of!(PadState, left_trigger), 4);
        assert_eq!(offset_of!(PadState, right_trigger), 8);
        assert_eq!(offset_of!(PadState, left_x), 12);
        assert_eq!(offset_of!(PadState, left_y), 16);
        assert_eq!(offset_of!(PadState, right_x), 20);
        assert_eq!(offset_of!(PadState, right_y), 24);
        assert_eq!(size_of::<KeyState>(), 12);
        assert_eq!(offset_of!(KeyState, scan_code), 0);
        assert_eq!(offset_of!(KeyState, code_point), 4);
        assert_eq!(offset_of!(KeyState, virtual_key), 8);
        assert_eq!(offset_of!(KeyState, dead_key), 9);
        // GameInputGamepadButtons: Menu 1 ... RightThumbstick 0x2000, one bit each.
        assert_eq!(buttons::ALL.count_ones(), 14);
        assert_eq!(buttons::RIGHT_THUMB, 1 << 13);
    }

    #[test]
    fn current_reading_picks_the_first_connected() {
        let mut fake = Fake::default();
        fake.press(b'W');
        let f = Frame::read(&mut fake);
        assert_eq!(first_of(kind::GAMEPAD, &f), Some(Device::StandIn));
        assert_eq!(first_of(kind::KEYBOARD, &f), Some(Device::Keyboard));
        assert_eq!(Device::StandIn.gamepad(&f).unwrap().left_y, 1.0);
        assert_eq!(Device::Keyboard.keys(&f).len(), 1);
        assert_eq!(Device::Pad(0).status(&f), STATUS_NONE);

        fake.pads[2] = Some(PadState { buttons: buttons::X, ..NEUTRAL });
        let f = Frame::read(&mut fake);
        assert_eq!(first_of(kind::GAMEPAD, &f), Some(Device::Pad(2)));
        assert_eq!(Device::StandIn.status(&f), STATUS_NONE);
        assert_eq!(Device::Pad(2).gamepad(&f).unwrap().buttons, buttons::X);
        assert_eq!(Device::Keyboard.gamepad(&f), None);
        assert!(Device::Pad(2).keys(&f).is_empty());
    }
}
