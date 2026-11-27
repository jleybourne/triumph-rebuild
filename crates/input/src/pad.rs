//! What the title reads: one gamepad state and the keys held down.
//!
//! Both are laid out like the console's GameInput structures, so the runtime can hand them to the title
//! by copy (see `gameinput.rs` for the mapping and the layout test).

/// A gamepad reading. Triggers 0 to 1; sticks -1 to 1, up and right positive.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PadState {
    /// `buttons::*` bits.
    pub buttons: u32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub left_x: f32,
    pub left_y: f32,
    pub right_x: f32,
    pub right_y: f32,
}

/// Every button released, triggers and sticks at rest: "connected, nothing pressed".
pub const NEUTRAL: PadState = PadState {
    buttons: 0,
    left_trigger: 0.0,
    right_trigger: 0.0,
    left_x: 0.0,
    left_y: 0.0,
    right_x: 0.0,
    right_y: 0.0,
};

/// A trigger counts as held from this level up (after the host dead zone).
pub const TRIGGER_HELD: f32 = 0.1;

impl PadState {
    /// No button down and both triggers below `TRIGGER_HELD`. Sticks are positions, not presses, and a
    /// worn stick may never read exactly zero, so they are not part of "released".
    pub fn released(&self) -> bool {
        self.buttons == 0 && self.left_trigger < TRIGGER_HELD && self.right_trigger < TRIGGER_HELD
    }
}

/// Gamepad button bits, the console's GameInput values.
pub mod buttons {
    pub const MENU: u32 = 0x1;
    pub const VIEW: u32 = 0x2;
    pub const A: u32 = 0x4;
    pub const B: u32 = 0x8;
    pub const X: u32 = 0x10;
    pub const Y: u32 = 0x20;
    pub const DPAD_UP: u32 = 0x40;
    pub const DPAD_DOWN: u32 = 0x80;
    pub const DPAD_LEFT: u32 = 0x100;
    pub const DPAD_RIGHT: u32 = 0x200;
    pub const LEFT_SHOULDER: u32 = 0x400;
    pub const RIGHT_SHOULDER: u32 = 0x800;
    pub const LEFT_THUMB: u32 = 0x1000;
    pub const RIGHT_THUMB: u32 = 0x2000;
    /// Every bit above.
    pub const ALL: u32 = 0x3FFF;
}

/// One key held down: scan code (0xE0xx when extended), character (0 when unknown), virtual key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyState {
    pub scan_code: u32,
    pub code_point: u32,
    pub virtual_key: u8,
    pub dead_key: bool,
}

impl KeyState {
    /// A key known only by its virtual key (fakes, scripted input).
    pub const fn vk(virtual_key: u8) -> KeyState {
        KeyState { scan_code: 0, code_point: 0, virtual_key, dead_key: false }
    }
}

/// Host XInput values to a `PadState`: XInput's documented dead zones, then rescaled so the edge of the
/// dead zone reads 0 and full travel reads 1.
pub mod xinput {
    use super::{buttons as b, PadState};

    pub const LEFT_DEAD_ZONE: i16 = 7849;
    pub const RIGHT_DEAD_ZONE: i16 = 8689;
    pub const TRIGGER_THRESHOLD: u8 = 30;

    /// XInput button bits to GameInput's.
    pub const BUTTONS: [(u16, u32); 14] = [
        (0x0001, b::DPAD_UP),
        (0x0002, b::DPAD_DOWN),
        (0x0004, b::DPAD_LEFT),
        (0x0008, b::DPAD_RIGHT),
        (0x0010, b::MENU), // Start
        (0x0020, b::VIEW), // Back
        (0x0040, b::LEFT_THUMB),
        (0x0080, b::RIGHT_THUMB),
        (0x0100, b::LEFT_SHOULDER),
        (0x0200, b::RIGHT_SHOULDER),
        (0x1000, b::A),
        (0x2000, b::B),
        (0x4000, b::X),
        (0x8000, b::Y),
    ];

    fn trigger(v: u8) -> f32 {
        if v <= TRIGGER_THRESHOLD {
            0.0
        } else {
            f32::from(v - TRIGGER_THRESHOLD) / f32::from(255 - TRIGGER_THRESHOLD)
        }
    }

    /// Radial dead zone: inside it the stick reads (0, 0); outside, the direction is kept and the length
    /// rescaled to 0..1.
    fn stick(x: i16, y: i16, dead: i16) -> (f32, f32) {
        let (x, y) = (f32::from(x), f32::from(y));
        let length = (x * x + y * y).sqrt();
        let dead = f32::from(dead);
        if length <= dead {
            return (0.0, 0.0);
        }
        let scaled = ((length - dead) / (32767.0 - dead)).min(1.0);
        ((x / length * scaled).clamp(-1.0, 1.0), (y / length * scaled).clamp(-1.0, 1.0))
    }

    pub fn pad(buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16) -> PadState {
        let (left_x, left_y) = stick(lx, ly, LEFT_DEAD_ZONE);
        let (right_x, right_y) = stick(rx, ry, RIGHT_DEAD_ZONE);
        PadState {
            buttons: BUTTONS.iter().filter(|(x, _)| buttons & x != 0).fold(0, |all, (_, g)| all | g),
            left_trigger: trigger(lt),
            right_trigger: trigger(rt),
            left_x,
            left_y,
            right_x,
            right_y,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xinput_buttons_map_one_to_one() {
        let mut seen = 0;
        for (x, g) in xinput::BUTTONS {
            let p = xinput::pad(x, 0, 0, 0, 0, 0, 0);
            assert_eq!(p.buttons, g);
            assert_eq!(seen & g, 0, "two XInput bits on one button");
            seen |= g;
        }
        assert_eq!(seen, buttons::ALL);
        assert_eq!(xinput::pad(0xFFFF, 0, 0, 0, 0, 0, 0).buttons, buttons::ALL);
    }

    #[test]
    fn xinput_dead_zones_and_ranges() {
        let rest = xinput::pad(0, 30, 10, 7000, -7000 / 2, 8000, 0);
        assert_eq!(rest, NEUTRAL);
        assert!(rest.released());
        let full = xinput::pad(0, 255, 255, 32767, 0, 0, -32768);
        assert_eq!((full.left_trigger, full.right_trigger), (1.0, 1.0));
        assert_eq!((full.left_x, full.left_y), (1.0, 0.0));
        assert_eq!((full.right_x, full.right_y), (0.0, -1.0));
        assert!(!full.released());
        // Diagonal at full travel keeps its direction and stays inside the unit circle.
        let d = xinput::pad(0, 0, 0, 32767, 32767, 0, 0);
        assert!((d.left_x - d.left_y).abs() < 1e-6 && d.left_x > 0.7 && d.left_x <= 1.0);
        // Just past the dead zone reads small, not a jump.
        let edge = xinput::pad(0, 31, 0, 7900, 0, 0, 0);
        assert!(edge.left_x > 0.0 && edge.left_x < 0.01 && edge.left_trigger < 0.01);
    }
}
