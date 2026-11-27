//! The host source: XInput slots 0-3 and the keyboard through `GetAsyncKeyState`, read only while a
//! window of this process has focus (another application's keys are not ours).

use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};
use windows::Win32::UI::Input::XboxController::{
    XInputGetState, XInputSetState, XINPUT_STATE, XINPUT_VIBRATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::pad::{xinput, KeyState, PadState};
use crate::source::{Source, PADS};

/// Keys polled: backspace through 0xFE, without the generic Shift / Ctrl / Alt (their left and right
/// keys are reported instead) and without the mouse buttons below 0x08.
fn polled() -> impl Iterator<Item = u8> {
    (0x08u8..=0xFE).filter(|vk| !(0x10..=0x12).contains(vk))
}

#[derive(Default)]
pub struct Host;

impl Host {
    pub fn new() -> Host {
        Host
    }

    /// Does the foreground window belong to this process?
    pub fn focused() -> bool {
        // SAFETY: plain queries; a null window gives process id 0.
        unsafe {
            let window = GetForegroundWindow();
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            pid != 0 && pid == GetCurrentProcessId()
        }
    }
}

impl Source for Host {
    fn pad(&mut self, slot: usize) -> Option<PadState> {
        if slot >= PADS {
            return None;
        }
        let mut state = XINPUT_STATE::default();
        // SAFETY: a valid out pointer; slot is 0-3.
        if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
            return None;
        }
        let g = state.Gamepad;
        Some(xinput::pad(
            g.wButtons.0,
            g.bLeftTrigger,
            g.bRightTrigger,
            g.sThumbLX,
            g.sThumbLY,
            g.sThumbRX,
            g.sThumbRY,
        ))
    }

    fn keys(&mut self) -> Vec<KeyState> {
        if !Host::focused() {
            return Vec::new();
        }
        polled()
            // SAFETY: plain queries.
            .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } as u16 & 0x8000 != 0)
            .map(|vk| KeyState {
                scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                ..KeyState::vk(vk)
            })
            .collect()
    }

    fn rumble(&mut self, slot: usize, low: u16, high: u16) {
        if slot < PADS {
            let v = XINPUT_VIBRATION { wLeftMotorSpeed: low, wRightMotorSpeed: high };
            // SAFETY: a valid pointer; an empty slot just returns an error.
            unsafe { XInputSetState(slot as u32, &v) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Frame;

    #[test]
    fn host_polls_without_failing() {
        let mut host = Host::new();
        let f = Frame::read(&mut host);
        // Whatever is plugged in, the stand-in exists exactly when no pad does.
        assert_eq!(f.stand_in.is_some(), f.pads.iter().all(Option::is_none));
        assert!(!polled().any(|vk| vk == 0x10 || vk < 0x08));
        assert_eq!(polled().count(), 0xFE - 0x08 + 1 - 3);
    }

    /// The cost of one host poll (4 XInput slots and the keyboard). Run by hand:
    /// `cargo test --release -p rebuild-input host_poll_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn host_poll_cost() {
        let mut host = Host::new();
        let mut take = |n: usize, f: &mut dyn FnMut(&mut Host)| {
            let mut us: Vec<f64> = (0..n)
                .map(|_| {
                    let t = std::time::Instant::now();
                    f(&mut host);
                    t.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            us.sort_by(f64::total_cmp);
            (us[n / 2], us[n * 99 / 100])
        };
        let pads = take(1000, &mut |h| {
            for s in 0..PADS {
                std::hint::black_box(h.pad(s));
            }
        });
        let keys = take(1000, &mut |h| {
            std::hint::black_box(polled().filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } != 0).count());
            std::hint::black_box(h.keys());
        });
        let frame = take(1000, &mut |h| {
            std::hint::black_box(Frame::read(h));
        });
        let connected = (0..PADS).filter(|&s| host.pad(s).is_some()).count();
        println!("pads connected      {connected} of {PADS}; focused {}", Host::focused());
        println!("pads (4 slots)      median {:.1} us  p99 {:.1} us", pads.0, pads.1);
        println!("keyboard (244 keys) median {:.1} us  p99 {:.1} us", keys.0, keys.1);
        println!("Frame::read         median {:.1} us  p99 {:.1} us", frame.0, frame.1);
    }
}
