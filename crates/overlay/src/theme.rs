//! Every colour and size the overlay draws with, in one place (the design pass changes only this file).
//!
//! Sizes are pixels at 1080 lines of output and scale with the output's height ([`scale`]), so the menu
//! covers the same share of the screen at 1080p and at 4K. Colours are sRGB, `0xAABBGGRR` ([`rgba`]).
//!
//! # Palette — "Graphite"
//!
//! A quiet, near-black cool-slate HUD with a single azure accent, meant to stay legible over any game frame
//! without going opaque. The design holds to three rules so it reads at a glance and stays maintainable:
//!
//! * **One accent, one meaning.** [`ACCENT`] (azure) is the only hue that carries interaction: the selected
//!   row's stripe, the active tab's underline, the brand mark, the edit caret. Nothing decorative uses it.
//! * **Hierarchy by brightness, not hue.** The neutrals are a three-step ramp on the same cool slate —
//!   [`TITLE`] (brightest) ▸ [`TEXT`] (body) ▸ [`DIM`] (help, hints, inactive tabs) — with [`VALUE`] a pure
//!   white reserved for the focused row's value. State never relies on colour alone: the selected row pairs the
//!   accent with a filled bar, a left stripe and a brighter label; the active tab pairs it with a fill and a
//!   brighter title; a disabled action pairs its dimmed label with an explicit status value ("not bound").
//! * **Status colours are rare.** [`WARN`] (amber) and [`GOOD`] (green) appear only on the one-line result
//!   message, never on structure.
//!
//! Depth comes from a soft [`SHADOW`] halo behind the panel (so it separates from a bright frame) rather than
//! from opacity: the panel stays translucent. Type is one GDI-baked weight at one size — a dev-tool register,
//! information-dense — so the whole scale is spacing on a 4 px grid: body [`ROW_H`] 34, [`HEADER_H`] 48 giving
//! the title presence, [`PAD`] 20, [`MARGIN`] 40.

/// Pack an sRGB colour as the shader reads it: red in the low byte.
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    (r as u32) | (g as u32) << 8 | (b as u32) << 16 | (a as u32) << 24
}

/// The font: face, weight (GDI 100-900) and body size.
pub const FONT_FACE: &str = "Segoe UI";
pub const FONT_WEIGHT: i32 = 600;
pub const FONT_PX: f32 = 21.0;

/// The panel: margin from the top-left corner, width, inner padding, corner radius.
pub const MARGIN: f32 = 40.0;
pub const PANEL_W: f32 = 620.0;
pub const PAD: f32 = 20.0;
pub const RADIUS: f32 = 10.0;
/// One row (label and value), the header, the tab strip.
pub const ROW_H: f32 = 34.0;
pub const HEADER_H: f32 = 48.0;
pub const TAB_H: f32 = 36.0;
/// The selection bar: its accent stripe and its corner radius.
pub const STRIPE_W: f32 = 4.0;
pub const ROW_RADIUS: f32 = 6.0;
/// The soft shadow halo behind a panel: how far it reaches past the panel's edge (readout: [`HUD_SHADOW_SPREAD`]).
pub const SHADOW_SPREAD: f32 = 16.0;
pub const HUD_SHADOW_SPREAD: f32 = 8.0;
/// Lines of help text under the rows, at most.
pub const HELP_LINES: usize = 3;
/// Rows shown at once on a page; more scroll.
pub const MAX_ROWS: usize = 12;

/// The stats readout while the menu is closed (`diagnostics.overlay`): top-right corner.
pub const HUD_PAD: f32 = 10.0;
pub const HUD_RADIUS: f32 = 8.0;

// Surfaces (translucent: the frame shows through).
pub const PANEL: u32 = rgba(15, 17, 22, 236);
pub const HEADER: u32 = rgba(23, 27, 35, 255);
pub const HUD: u32 = rgba(12, 14, 19, 206);
/// The soft halo behind panels; layered to fake a blur, so kept low-alpha.
pub const SHADOW: u32 = rgba(0, 0, 0, 72);

// Neutral text ramp: brightness is the hierarchy.
pub const TITLE: u32 = rgba(240, 243, 250, 255);
pub const TEXT: u32 = rgba(200, 208, 221, 255);
pub const DIM: u32 = rgba(124, 134, 152, 255);
pub const VALUE: u32 = rgba(255, 255, 255, 255);
pub const DISABLED: u32 = rgba(110, 118, 134, 255);

// The one accent, and the two rare status hues.
pub const ACCENT: u32 = rgba(84, 162, 255, 255);
pub const SELECTED: u32 = rgba(34, 56, 92, 224);
pub const TAB_ON: u32 = rgba(30, 37, 50, 255);
pub const WARN: u32 = rgba(255, 188, 92, 255);
pub const GOOD: u32 = rgba(116, 214, 148, 255);

/// Output height to the theme's scale (1.0 at 1080 lines), never below 0.6 so small windows stay legible.
pub fn scale(height: u32) -> f32 {
    (height as f32 / 1080.0).max(0.6)
}

/// The body font's pixel size at an output height.
pub fn font_px(height: u32) -> u32 {
    (FONT_PX * scale(height)).round().max(8.0) as u32
}
