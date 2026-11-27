//! Controller and keyboard input for the layer (Build 3 piece; the in-game menu needs it).
//!
//! - `pad`: `PadState` and `KeyState`, laid out like the console's GameInput readings; XInput conversion.
//! - `keyboard`: the keyboard stand-in pad, used while no real pad is connected.
//! - `source`: `Source` (host or fake) and one polled `Frame`.
//! - `menu`: `Input`, which splits each poll between the title and the in-game menu, with isolation.
//! - `gameinput`: what the title's GameInput calls read from a `Frame`.
//! - `host` (Windows): XInput slots 0-3 and the focused keyboard.
//!
//! No GPU, no global state.

pub mod gameinput;
#[cfg(windows)]
pub mod host;
pub mod keyboard;
pub mod menu;
pub mod pad;
pub mod source;

pub use menu::{menu_key_named, Change, Input, Phase, Polled, MENU_KEY, MENU_KEYS};
pub use pad::{buttons, KeyState, PadState, NEUTRAL};
pub use source::{Fake, Frame, Source, PADS};
