//! Every colour and size the overlay draws with, in one place (the design pass changes only this file).
//!
//! Sizes are pixels at 1080 lines of output and scale with the output's height ([`scale`]), so the menu
//! covers the same share of the screen at 1080p and at 4K. Colours are sRGB, `0xAABBGGRR` ([`rgba`]).

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
pub const RADIUS: f32 = 12.0;
/// One row (label and value), the header, the tab strip.
pub const ROW_H: f32 = 36.0;
pub const HEADER_H: f32 = 46.0;
pub const TAB_H: f32 = 38.0;
/// The selection bar: its accent stripe and its corner radius.
pub const STRIPE_W: f32 = 4.0;
pub const ROW_RADIUS: f32 = 6.0;
/// Lines of help text under the rows, at most.
pub const HELP_LINES: usize = 3;
/// Rows shown at once on a page; more scroll.
pub const MAX_ROWS: usize = 12;

/// The stats readout while the menu is closed (`diagnostics.overlay`): top-right corner.
pub const HUD_PAD: f32 = 10.0;
pub const HUD_RADIUS: f32 = 8.0;

pub const PANEL: u32 = rgba(16, 18, 24, 232);
pub const HEADER: u32 = rgba(28, 32, 42, 255);
pub const TITLE: u32 = rgba(236, 240, 248, 255);
pub const TEXT: u32 = rgba(214, 220, 230, 255);
pub const DIM: u32 = rgba(132, 142, 160, 255);
pub const VALUE: u32 = rgba(255, 255, 255, 255);
pub const ACCENT: u32 = rgba(90, 170, 255, 255);
pub const SELECTED: u32 = rgba(46, 72, 112, 210);
pub const TAB_ON: u32 = rgba(44, 52, 68, 255);
pub const DISABLED: u32 = rgba(96, 102, 116, 255);
pub const WARN: u32 = rgba(255, 186, 84, 255);
pub const GOOD: u32 = rgba(120, 220, 150, 255);
pub const HUD: u32 = rgba(10, 12, 16, 190);

/// Output height to the theme's scale (1.0 at 1080 lines), never below 0.6 so small windows stay legible.
pub fn scale(height: u32) -> f32 {
    (height as f32 / 1080.0).max(0.6)
}

/// The body font's pixel size at an output height.
pub fn font_px(height: u32) -> u32 {
    (FONT_PX * scale(height)).round().max(8.0) as u32
}
