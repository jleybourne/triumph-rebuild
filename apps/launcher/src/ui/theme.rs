//! The launcher's own dark theme: every colour, radius, spacing step, motion time and type style as a
//! named token (the table in `agents/launcher.md`). Nothing outside this module picks a colour.

#![allow(dead_code)]

use eframe::egui::{
    self, epaint::Shadow, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextFormat,
    Visuals,
};
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

// colour
pub const BG_BASE: Color32 = rgb(0x07, 0x08, 0x0C);
pub const BG_SIDEBAR: Color32 = Color32::from_rgba_premultiplied(10, 12, 17, 235);
pub const SURFACE_1: Color32 = rgb(0x10, 0x13, 0x1A);
pub const SURFACE_2: Color32 = rgb(0x16, 0x1A, 0x23);
pub const SURFACE_3: Color32 = rgb(0x1D, 0x22, 0x30);
pub const HAIRLINE: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);
pub const HAIRLINE_HI: Color32 = Color32::from_rgba_premultiplied(31, 31, 31, 31);
pub const TEXT: Color32 = rgb(0xF2, 0xF4, 0xF8);
pub const TEXT_2: Color32 = rgb(0xB6, 0xBC, 0xCA);
pub const TEXT_3: Color32 = rgb(0x8C, 0x93, 0xA5);
pub const ACCENT: Color32 = rgb(0x7C, 0x6C, 0xFF);
pub const ACCENT_2: Color32 = rgb(0x4F, 0x8B, 0xFF);
pub const SUCCESS: Color32 = rgb(0x3D, 0xDC, 0x97);
pub const WARNING: Color32 = rgb(0xFF, 0xB5, 0x47);
pub const DANGER: Color32 = rgb(0xFF, 0x5C, 0x6C);

/// Cards: near-black glass at 80 %, over the moving background.
pub const GLASS: Color32 = rgb(0x0C, 0x0E, 0x14);
pub fn glass() -> Color32 {
    alpha(GLASS, 0.80)
}


// radius
pub const R_SM: u8 = 8;
pub const R_MD: u8 = 10;
pub const R_LG: u8 = 14;
pub const R_PILL: u8 = 255;

// spacing: an 8 px grid
pub const S1: f32 = 8.0;
pub const S2: f32 = 16.0;
pub const S3: f32 = 24.0;
pub const S4: f32 = 32.0;
pub const S6: f32 = 48.0;

// motion (seconds)
pub const T_HOVER: f32 = 0.12;
pub const T_PRESS: f32 = 0.18;
pub const T_PAGE: f32 = 0.32;
pub const T_STAGGER: f32 = 0.04;

pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn card_shadow() -> Shadow {
    Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(115) }
}

// type
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
    (bold(40.0), -1.0)
}
pub fn numerals() -> (FontId, f32) {
    (bold(36.0), -0.8)
}
pub fn h1() -> (FontId, f32) {
    (semibold(26.0), -0.4)
}
pub fn h2() -> (FontId, f32) {
    (semibold(16.0), -0.1)
}
pub fn body() -> FontId {
    regular(14.0)
}
pub fn small() -> FontId {
    regular(12.5)
}
pub fn caption() -> (FontId, f32) {
    (semibold(10.5), 0.6)
}

/// A one-style layout job with tracking.
pub fn job(text: &str, (font, tracking): (FontId, f32), color: Color32) -> egui::text::LayoutJob {
    let mut j = egui::text::LayoutJob::default();
    j.append(text, 0.0, TextFormat { font_id: font, color, extra_letter_spacing: tracking, ..Default::default() });
    j
}

pub fn install(ctx: &egui::Context) {
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

    let mut v = Visuals::dark();
    v.dark_mode = true;
    v.override_text_color = None;
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = SURFACE_2;
    v.window_stroke = stroke(1.0, HAIRLINE_HI);
    v.window_corner_radius = CornerRadius::same(R_LG);
    v.window_shadow = card_shadow();
    v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(140) };
    v.menu_corner_radius = CornerRadius::same(R_MD);
    v.extreme_bg_color = SURFACE_2;
    v.faint_bg_color = alpha(SURFACE_2, 0.5);
    v.code_bg_color = SURFACE_2;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = alpha(ACCENT, 0.35);
    v.selection.stroke = stroke(1.0, ACCENT);
    v.warn_fg_color = WARNING;
    v.error_fg_color = DANGER;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.text_cursor.stroke = stroke(2.0, ACCENT);
    let w = &mut v.widgets;
    let r = CornerRadius::same(R_MD);
    w.noninteractive.bg_fill = SURFACE_2;
    w.noninteractive.weak_bg_fill = SURFACE_2;
    w.noninteractive.bg_stroke = stroke(1.0, HAIRLINE);
    w.noninteractive.fg_stroke = stroke(1.0, TEXT_2);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = SURFACE_3;
    w.inactive.weak_bg_fill = SURFACE_2;
    w.inactive.bg_stroke = stroke(1.0, HAIRLINE);
    w.inactive.fg_stroke = stroke(1.0, TEXT);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = SURFACE_3;
    w.hovered.weak_bg_fill = SURFACE_3;
    w.hovered.bg_stroke = stroke(1.0, HAIRLINE_HI);
    w.hovered.fg_stroke = stroke(1.0, TEXT);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = ACCENT;
    w.active.weak_bg_fill = alpha(ACCENT, 0.25);
    w.active.bg_stroke = stroke(1.0, ACCENT);
    w.active.fg_stroke = stroke(1.0, TEXT);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.hovered;
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(S1, S1);
        s.spacing.button_padding = egui::vec2(14.0, 8.0);
        s.spacing.interact_size.y = 32.0;
        s.spacing.slider_width = 240.0;
        s.spacing.slider_rail_height = 6.0;
        s.spacing.combo_width = 200.0;
        s.spacing.scroll = egui::style::ScrollStyle::thin();
        s.animation_time = T_HOVER;
        s.text_styles.insert(egui::TextStyle::Body, body());
        s.text_styles.insert(egui::TextStyle::Button, medium(13.5));
        s.text_styles.insert(egui::TextStyle::Small, small());
        s.text_styles.insert(egui::TextStyle::Heading, semibold(22.0));
        s.text_styles.insert(egui::TextStyle::Monospace, mono(12.5));
    });
}

/// A stroke (typed, so literals stay f32).
pub fn stroke(width: f32, color: Color32) -> Stroke {
    Stroke { width, color: color.into() }
}
