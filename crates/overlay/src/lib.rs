//! The in-game overlay menu (task L5): drawn by the layer over the title's frame at present time.
//!
//! ```text
//! input thread (service)                         present thread (runtime::present)
//!   Input::poll every 8 ms (~170 µs a poll)         visible()?  one relaxed load when closed
//!   title's Frame -> Shared::title (isolated)       scene()     Arc clone under a short lock
//!   Navigator: presses + repeat -> Nav              Renderer::record: copy quads, one draw
//!   Menu: settings, stats, developer actions
//!   layout -> Scene (quads) when something changed
//! ```
//!
//! - [`menu`]: the pages, the selection and what each key does; settings go through a [`menu::Backend`].
//! - [`nav`]: menu input (keyboard and pads) to [`menu::Nav`] events with key repeat.
//! - [`actions`]: the Developer page's pluggable actions, gated by an `online()` probe.
//! - [`font`]: a text atlas baked at start (GDI, the output's size) and a synthetic one for tests.
//! - [`layout`]: the menu as quads ([`layout::Scene`]); every colour and size comes from [`theme`].
//! - [`service`]: the input thread's loop and the state it shares with the present.
//! - [`render`] (Windows): one small D3D12 pass, quads and the atlas, no dependency on the title's state.
//!
//! The overlay never touches the title's image: it draws on the swap chain's buffer after the title's frame was
//! copied there, and a capture copies that buffer before the overlay unless `REBUILD_CAPTURE_OVERLAY=1`.

pub mod actions;
pub mod font;
pub mod layout;
pub mod menu;
pub mod nav;
#[cfg(windows)]
pub mod render;
pub mod service;
pub mod theme;

pub use actions::{online, Action, Param, ParamKind, ParamValue, Placeholder, Registry};
pub use font::Font;
pub use layout::{Quad, Scene};
pub use menu::{Backend, Menu, Nav, Outcome, Page, Stats};
pub use nav::Navigator;
pub use service::{Service, Shared};
