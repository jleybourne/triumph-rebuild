//! The launcher's two themes (L6, owner-approved Direction B): every colour, radius, spacing step and
//! type style as a named token. Colours are functions of the current theme (`set_dark`), so one switch
//! repaints everything. Nothing outside this module picks a colour.
//!
//! | token      | dark      | light     |
//! |:--         |:--        |:--        |
//! | bg         | `#0E1014` | `#F5F6F8` |
//! | header     | `#12151B` | `#FFFFFF` |
//! | panel      | `#151A23` | `#FFFFFF` |
//! | line       | `#262C37` | `#DCE1E8` |
//! | text       | `#E8EAED` | `#14171C` |
//! | muted      | `#8C95A3` | `#5B6472` |
//! | accent     | `#3D7BFF` | `#2F6BEB` |
//! | ok on      | `#9BE3B0` on `#17321F` | `#1D6B36` on `#E3F5E8` |
//! | warn on    | `#F5C36B` on `#2E2714` | `#7A4F00` on `#FFF2D6` |

#![allow(dead_code)]

use eframe::egui::{self, epaint::Shadow, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextFormat, Visuals};
use std::cell::Cell;
use std::sync::Arc;

pub const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}
pub fn alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}
pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let (a, b) = (a.to_array(), b.to_array());
    let m = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(m(0), m(1), m(2), m(3))
}

/// One theme's colours.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub header: Color32,
    pub panel: Color32,
    /// inputs, rows, quiet buttons
    pub control: Color32,
    /// hover on a control
    pub control_hi: Color32,
    pub line: Color32,
    pub line_hi: Color32,
    pub text: Color32,
    /// secondary text: between text and muted
    pub text_2: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_hi: Color32,
    /// an accent-tinted fill (active link, selected card)
    pub accent_bg: Color32,
    pub ok: Color32,
    pub ok_bg: Color32,
    pub warn: Color32,
    pub warn_bg: Color32,
    pub danger: Color32,
    pub danger_bg: Color32,
    /// the log and code wells
    pub well: Color32,
}

pub const DARK: Palette = Palette {
    bg: rgb(0x0E, 0x10, 0x14),
    header: rgb(0x12, 0x15, 0x1B),
    panel: rgb(0x15, 0x1A, 0x23),
    control: rgb(0x1B, 0x21, 0x2C),
    control_hi: rgb(0x23, 0x2A, 0x37),
    line: rgb(0x26, 0x2C, 0x37),
    line_hi: rgb(0x36, 0x3E, 0x4C),
    text: rgb(0xE8, 0xEA, 0xED),
    text_2: rgb(0xB8, 0xBE, 0xC8),
    muted: rgb(0x8C, 0x95, 0xA3),
    accent: rgb(0x3D, 0x7B, 0xFF),
    accent_hi: rgb(0x5A, 0x8F, 0xFF),
    accent_bg: rgb(0x16, 0x24, 0x40),
    ok: rgb(0x9B, 0xE3, 0xB0),
    ok_bg: rgb(0x17, 0x32, 0x1F),
    warn: rgb(0xF5, 0xC3, 0x6B),
    warn_bg: rgb(0x2E, 0x27, 0x14),
    danger: rgb(0xFF, 0x8A, 0x8A),
    danger_bg: rgb(0x3A, 0x18, 0x1B),
    well: rgb(0x0B, 0x0D, 0x11),
};

pub const LIGHT: Palette = Palette {
    bg: rgb(0xF5, 0xF6, 0xF8),
    header: rgb(0xFF, 0xFF, 0xFF),
    panel: rgb(0xFF, 0xFF, 0xFF),
    control: rgb(0xF2, 0xF4, 0xF7),
    control_hi: rgb(0xE8, 0xEC, 0xF1),
    line: rgb(0xDC, 0xE1, 0xE8),
    line_hi: rgb(0xC3, 0xCA, 0xD5),
    text: rgb(0x14, 0x17, 0x1C),
    text_2: rgb(0x36, 0x3D, 0x48),
    muted: rgb(0x5B, 0x64, 0x72),
    accent: rgb(0x2F, 0x6B, 0xEB),
    accent_hi: rgb(0x24, 0x5B, 0xD1),
    accent_bg: rgb(0xE6, 0xEE, 0xFD),
    ok: rgb(0x1D, 0x6B, 0x36),
    ok_bg: rgb(0xE3, 0xF5, 0xE8),
    warn: rgb(0x7A, 0x4F, 0x00),
    warn_bg: rgb(0xFF, 0xF2, 0xD6),
    danger: rgb(0xB4, 0x23, 0x18),
    danger_bg: rgb(0xFD, 0xE8, 0xE6),
    well: rgb(0xF7, 0xF8, 0xFA),
};

thread_local! {
    static IS_DARK: Cell<bool> = const { Cell::new(true) };
}

/// The current palette.
pub fn p() -> Palette {
    if is_dark() {
        DARK
    } else {
        LIGHT
    }
}

pub fn is_dark() -> bool {
    IS_DARK.with(|d| d.get())
}

/// Switch the theme: colours and egui's own widget visuals.
pub fn set_dark(ctx: &egui::Context, dark: bool) {
    IS_DARK.with(|d| d.set(dark));
    ctx.set_visuals(visuals());
    ctx.request_repaint();
}

// radius
pub const R_SM: u8 = 6;
pub const R_MD: u8 = 8;
pub const R_LG: u8 = 10;
pub const R_PILL: u8 = 255;

// spacing: an 8 px grid
pub const S1: f32 = 8.0;
pub const S2: f32 = 16.0;
pub const S3: f32 = 24.0;
pub const S4: f32 = 32.0;
pub const S6: f32 = 48.0;

// motion (seconds)
pub const T_HOVER: f32 = 0.10;
pub const T_PRESS: f32 = 0.15;

pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn card_shadow() -> Shadow {
    if is_dark() {
        Shadow::NONE
    } else {
        Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(14) }
    }
}

pub fn popup_shadow() -> Shadow {
    Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(if is_dark() { 150 } else { 40 }) }
}

// type: Inter for text, a heavier weight for headings
pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}
/// Phosphor icons on their own.
pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn display() -> (FontId, f32) {
    (bold(30.0), -0.6)
}
pub fn numerals() -> (FontId, f32) {
    (bold(28.0), -0.5)
}
pub fn h1() -> (FontId, f32) {
    (bold(22.0), -0.3)
}
pub fn h2() -> (FontId, f32) {
    (semibold(15.0), -0.1)
}
pub fn body() -> FontId {
    regular(13.5)
}
pub fn small() -> FontId {
    regular(12.5)
}
pub fn caption() -> (FontId, f32) {
    (semibold(10.5), 0.5)
}

/// A one-style layout job with tracking.
pub fn job(text: &str, (font, tracking): (FontId, f32), color: Color32) -> egui::text::LayoutJob {
    let mut j = egui::text::LayoutJob::default();
    j.append(text, 0.0, TextFormat { font_id: font, color, extra_letter_spacing: tracking, ..Default::default() });
    j
}

fn visuals() -> Visuals {
    let c = p();
    let mut v = if is_dark() { Visuals::dark() } else { Visuals::light() };
    v.override_text_color = None;
    v.panel_fill = c.bg;
    v.window_fill = c.panel;
    v.window_stroke = stroke(1.0, c.line);
    v.window_corner_radius = CornerRadius::same(R_LG);
    v.window_shadow = popup_shadow();
    v.popup_shadow = popup_shadow();
    v.menu_corner_radius = CornerRadius::same(R_MD);
    v.extreme_bg_color = c.control;
    v.faint_bg_color = c.control;
    v.code_bg_color = c.well;
    v.hyperlink_color = c.accent;
    v.selection.bg_fill = alpha(c.accent, 0.30);
    v.selection.stroke = stroke(1.0, c.accent);
    v.warn_fg_color = c.warn;
    v.error_fg_color = c.danger;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.text_cursor.stroke = stroke(2.0, c.accent);
    let w = &mut v.widgets;
    let r = CornerRadius::same(R_MD);
    w.noninteractive.bg_fill = c.panel;
    w.noninteractive.weak_bg_fill = c.panel;
    w.noninteractive.bg_stroke = stroke(1.0, c.line);
    w.noninteractive.fg_stroke = stroke(1.0, c.text_2);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = c.line_hi;
    w.inactive.weak_bg_fill = c.control;
    w.inactive.bg_stroke = stroke(1.0, c.line);
    w.inactive.fg_stroke = stroke(1.0, c.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = c.line_hi;
    w.hovered.weak_bg_fill = c.control_hi;
    w.hovered.bg_stroke = stroke(1.0, c.line_hi);
    w.hovered.fg_stroke = stroke(1.0, c.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = c.accent;
    w.active.weak_bg_fill = c.accent_bg;
    w.active.bg_stroke = stroke(1.0, c.accent);
    w.active.fg_stroke = stroke(1.0, c.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.hovered;
    v
}

pub fn install(ctx: &egui::Context, dark: bool) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut fonts, "inter", include_bytes!("../../assets/fonts/Inter-Regular.ttf"));
    add(&mut fonts, "inter-medium", include_bytes!("../../assets/fonts/Inter-Medium.ttf"));
    add(&mut fonts, "inter-semibold", include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"));
    add(&mut fonts, "inter-bold", include_bytes!("../../assets/fonts/Inter-Bold.ttf"));
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    let fallback: Vec<String> = fonts.families[&FontFamily::Proportional].clone();
    // Inter first, then Phosphor (for icons inside text), then egui's emoji faces
    let rest: Vec<String> = fallback.iter().filter(|f| *f != "Ubuntu-Light").cloned().collect();
    for (fam, face) in [(FontFamily::Proportional, "inter"), (FontFamily::Name("medium".into()), "inter-medium"), (FontFamily::Name("semibold".into()), "inter-semibold"), (FontFamily::Name("bold".into()), "inter-bold")] {
        let mut v = vec![face.to_string()];
        v.extend(rest.iter().cloned());
        fonts.families.insert(fam, v);
    }
    // standalone icons use Phosphor alone: Inter has private-use alternates that would win, and
    // Phosphor has blank Latin glyphs, so neither can go first in a shared family
    fonts.families.insert(FontFamily::Name("icons".into()), vec!["phosphor".to_string()]);
    if let Some(m) = fonts.families.get_mut(&FontFamily::Monospace) {
        m.push("phosphor".into());
    }
    ctx.set_fonts(fonts);
    set_dark(ctx, dark);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(S1, S1);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.slider_width = 220.0;
        s.spacing.slider_rail_height = 5.0;
        s.spacing.combo_width = 200.0;
        s.spacing.scroll = egui::style::ScrollStyle::thin();
        s.animation_time = T_HOVER;
        s.text_styles.insert(egui::TextStyle::Body, body());
        s.text_styles.insert(egui::TextStyle::Button, medium(13.0));
        s.text_styles.insert(egui::TextStyle::Small, small());
        s.text_styles.insert(egui::TextStyle::Heading, bold(20.0));
        s.text_styles.insert(egui::TextStyle::Monospace, mono(12.0));
    });
}

/// A stroke (typed, so literals stay f32).
pub fn stroke(width: f32, color: Color32) -> Stroke {
    Stroke { width, color: color.into() }
}
