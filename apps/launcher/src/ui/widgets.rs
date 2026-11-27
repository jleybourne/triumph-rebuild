//! The launcher's controls, flat and painted in theme (Direction B): every one takes keyboard focus and
//! draws a focus ring; hover is a short colour change. Nothing here animates while idle.

use super::theme::{self as th, alpha, lerp};
use eframe::egui::{self, Align2, Color32, CornerRadius, Id, Pos2, Rect, Response, Sense, StrokeKind, Ui, Vec2};

/// The colour role of a chip or badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Danger,
    Accent,
    Muted,
}

impl Tone {
    /// (text, fill)
    pub fn colours(self) -> (Color32, Color32) {
        let c = th::p();
        match self {
            Tone::Ok => (c.ok, c.ok_bg),
            Tone::Warn => (c.warn, c.warn_bg),
            Tone::Danger => (c.danger, c.danger_bg),
            Tone::Accent => (c.accent, c.accent_bg),
            Tone::Muted => (c.text_2, c.control),
        }
    }
}

pub fn focus_ring(ui: &Ui, resp: &Response, r: Rect, radius: u8) {
    if resp.has_focus() {
        ui.painter().rect_stroke(r.expand(2.0), CornerRadius::same(radius.saturating_add(2)), th::stroke(2.0, alpha(th::p().accent, 0.8)), StrokeKind::Outside);
    }
}

fn hover(ui: &Ui, resp: &Response) -> f32 {
    ui.ctx().animate_bool_with_time(resp.id.with("hover"), resp.hovered() || resp.has_focus(), th::T_HOVER)
}

/// Text plus an optional leading icon, painted centred in `r`.
fn label_in(ui: &Ui, r: Rect, icon: &str, text: &str, font: egui::FontId, color: Color32) {
    let p = ui.painter();
    let ifont = th::icon(font.size + 2.0);
    if text.is_empty() {
        p.text(r.center(), Align2::CENTER_CENTER, icon, ifont, color);
        return;
    }
    let tg = p.layout_no_wrap(text.to_string(), font, color);
    if icon.is_empty() {
        p.galley(r.center() - tg.size() / 2.0, tg, color);
        return;
    }
    let ig = p.layout_no_wrap(icon.to_string(), ifont, color);
    let gap = 7.0;
    let total = ig.size().x + gap + tg.size().x;
    let x = r.center().x - total / 2.0;
    p.galley(Pos2::new(x, r.center().y - ig.size().y / 2.0), ig, color);
    p.galley(Pos2::new(x + total - tg.size().x, r.center().y - tg.size().y / 2.0), tg, color);
}

/// The primary action: a flat accent fill.
pub fn primary_button(ui: &mut Ui, icon: &str, text: &str, size: Vec2, enabled: bool) -> Response {
    let (r, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let c = th::p();
    let h = if enabled { hover(ui, &resp) } else { 0.0 };
    let fill = if enabled { lerp(c.accent, c.accent_hi, h) } else { c.control };
    ui.painter().rect_filled(r, CornerRadius::same(th::R_MD), fill);
    if !enabled {
        ui.painter().rect_stroke(r, CornerRadius::same(th::R_MD), th::stroke(1.0, c.line), StrokeKind::Inside);
    }
    let font = th::semibold((size.y * 0.33).clamp(13.0, 17.0));
    label_in(ui, r, icon, text, font, if enabled { Color32::WHITE } else { c.muted });
    focus_ring(ui, &resp, r, th::R_MD);
    resp
}

/// A quiet button: control fill, hairline, hover lift.
pub fn button(ui: &mut Ui, icon: &str, text: &str, enabled: bool) -> Response {
    let c = th::p();
    let font = th::medium(13.0);
    let w = ui.painter().layout_no_wrap(text.to_string(), font.clone(), c.text).size().x + if icon.is_empty() { 24.0 } else { 46.0 };
    let size = Vec2::new(if text.is_empty() { 30.0 } else { w }, 30.0);
    let (r, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let h = if enabled { hover(ui, &resp) } else { 0.0 };
    ui.painter().rect_filled(r, CornerRadius::same(th::R_MD), lerp(c.control, c.control_hi, h));
    ui.painter().rect_stroke(r, CornerRadius::same(th::R_MD), th::stroke(1.0, lerp(c.line, c.line_hi, h)), StrokeKind::Inside);
    label_in(ui, r, icon, text, font, if enabled { lerp(c.text_2, c.text, h) } else { c.muted });
    focus_ring(ui, &resp, r, th::R_MD);
    resp
}

/// A danger-tinted quiet button.
pub fn danger_button(ui: &mut Ui, icon: &str, text: &str, enabled: bool) -> Response {
    let resp = button(ui, icon, text, enabled);
    if enabled {
        let c = th::p();
        let h = hover(ui, &resp);
        ui.painter().rect_stroke(resp.rect, CornerRadius::same(th::R_MD), th::stroke(1.0, alpha(c.danger, 0.35 + 0.4 * h)), StrokeKind::Inside);
    }
    resp
}

/// A chip: small text on a tinted fill (Live / Restart / Experimental, output, limits).
pub fn chip(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let (fg, bg) = tone.colours();
    let galley = ui.painter().layout_no_wrap(text.to_string(), th::medium(11.5), fg);
    let size = galley.size() + Vec2::new(14.0, 6.0);
    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(th::R_SM), bg);
    ui.painter().galley(r.center() - galley.size() / 2.0, galley, fg);
    resp
}

/// A rounded pill with a dot: the top bar status.
pub fn status_pill(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let (fg, bg) = tone.colours();
    let galley = ui.painter().layout_no_wrap(text.to_string(), th::medium(12.5), fg);
    let size = Vec2::new(galley.size().x + 32.0, 26.0);
    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(th::R_PILL), bg);
    ui.painter().circle_filled(Pos2::new(r.left() + 13.0, r.center().y), 3.5, fg);
    ui.painter().galley(Pos2::new(r.left() + 22.0, r.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

/// A switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let c = th::p();
    let (r, mut resp) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let k = th::ease_out(ui.ctx().animate_bool_with_time(resp.id, *on, th::T_PRESS));
    let h = hover(ui, &resp);
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::same(th::R_PILL), lerp(lerp(c.line, c.line_hi, h), c.accent, k));
    let x = egui::lerp((r.left() + 11.0)..=(r.right() - 11.0), k);
    p.circle_filled(Pos2::new(x, r.center().y), 8.0, Color32::WHITE);
    focus_ring(ui, &resp, r, th::R_PILL);
    resp
}

/// A segmented control for a few choices. Arrow keys move the choice while it has focus.
pub fn segmented(ui: &mut Ui, _id: Id, options: &[&str], index: &mut usize) -> bool {
    let c = th::p();
    let font = th::medium(12.5);
    let widths: Vec<f32> = options.iter().map(|o| ui.painter().layout_no_wrap(o.to_string(), font.clone(), c.text).size().x + 24.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let (r, resp) = ui.allocate_exact_size(Vec2::new(total, 30.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let before = *index;
    if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let mut x = r.left() + 3.0;
            for (i, w) in widths.iter().enumerate() {
                if pos.x >= x && pos.x < x + w {
                    *index = i;
                }
                x += w;
            }
        }
    }
    if resp.has_focus() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) && *index + 1 < options.len() {
                *index += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) && *index > 0 {
                *index -= 1;
            }
        });
    }
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::same(th::R_MD), c.control);
    p.rect_stroke(r, CornerRadius::same(th::R_MD), th::stroke(1.0, c.line), StrokeKind::Inside);
    let x_of = |i: usize| r.left() + 3.0 + widths[..i].iter().sum::<f32>();
    let sel = Rect::from_min_size(Pos2::new(x_of(*index), r.top() + 3.0), Vec2::new(widths[*index], r.height() - 6.0));
    p.rect_filled(sel, CornerRadius::same(th::R_SM), c.accent);
    for (i, o) in options.iter().enumerate() {
        let cr = Rect::from_min_size(Pos2::new(x_of(i), r.top()), Vec2::new(widths[i], r.height()));
        let col = if i == *index { Color32::WHITE } else { c.text_2 };
        p.text(cr.center(), Align2::CENTER_CENTER, *o, font.clone(), col);
    }
    focus_ring(ui, &resp, r, th::R_MD);
    *index != before
}

/// Text with tracking.
pub fn text(ui: &mut Ui, s: &str, style: (egui::FontId, f32), color: Color32) -> Response {
    ui.add(egui::Label::new(th::job(s, style, color)).selectable(false))
}

/// Secondary text that wraps.
pub fn muted(ui: &mut Ui, s: &str) -> Response {
    ui.add(egui::Label::new(egui::RichText::new(s).font(th::small()).color(th::p().muted)).wrap())
}

/// A small upper-case caption.
pub fn caption(ui: &mut Ui, s: &str) -> Response {
    text(ui, &s.to_uppercase(), th::caption(), th::p().muted)
}

/// A panel: flat fill, hairline, optional title. Its content is laid out top-down and left-aligned (not
/// justified, even inside `ui.columns`, so wrapped text keeps its spacing).
pub fn card<R>(ui: &mut Ui, title: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    let c = th::p();
    panel(ui, c.panel, c.line, 1.0, th::S2, |ui| {
        if let Some(t) = title {
            text(ui, t, th::h2(), c.text);
            ui.add_space(4.0);
        }
        add(ui)
    })
    .0
}

/// The frame under `card`, with its colours and margin given; returns the frame's rect too.
pub fn panel<R>(ui: &mut Ui, fill: Color32, line: Color32, width: f32, margin: f32, add: impl FnOnce(&mut Ui) -> R) -> (R, Rect) {
    let r = egui::Frame::new()
        .fill(fill)
        .stroke(th::stroke(width, line))
        .corner_radius(CornerRadius::same(th::R_LG))
        .shadow(th::card_shadow())
        .inner_margin(egui::Margin::same(margin as i8))
        .show(ui, |ui| {
            let wd = ui.available_width();
            ui.set_width(wd);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(wd);
                add(ui)
            })
            .inner
        });
    (r.inner, r.response.rect)
}

/// A label in a fixed-width, left-aligned slot (a table's first column).
pub fn fixed(ui: &mut Ui, width: f32, t: egui::RichText) {
    ui.allocate_ui_with_layout(Vec2::new(width, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_width(width);
        ui.add(egui::Label::new(t).truncate());
    });
}

/// A check box: accent fill with a tick when on.
pub fn checkbox(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let c = th::p();
    let galley = ui.painter().layout_no_wrap(label.to_string(), th::regular(13.0), c.text);
    let (r, mut resp) = ui.allocate_exact_size(Vec2::new(18.0 + 8.0 + galley.size().x, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let h = hover(ui, &resp);
    let b = Rect::from_min_size(Pos2::new(r.left(), r.center().y - 8.0), Vec2::splat(16.0));
    if *on {
        ui.painter().rect_filled(b, CornerRadius::same(4), lerp(c.accent, c.accent_hi, h));
        ui.painter().text(b.center(), Align2::CENTER_CENTER, super::icons::CHECK, th::icon(12.0), Color32::WHITE);
    } else {
        ui.painter().rect_filled(b, CornerRadius::same(4), c.control);
        ui.painter().rect_stroke(b, CornerRadius::same(4), th::stroke(1.0, lerp(c.line_hi, c.accent, h * 0.6)), StrokeKind::Inside);
    }
    ui.painter().galley(Pos2::new(b.right() + 8.0, r.center().y - galley.size().y / 2.0), galley, c.text);
    focus_ring(ui, &resp, b, 4);
    resp
}

/// A hairline across the available width.
pub fn rule(ui: &mut Ui) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), th::stroke(1.0, th::p().line));
    ui.add_space(th::S1);
}

/// A stat tile's body: caption, big value (or "—"), and a line under it.
pub fn stat(ui: &mut Ui, label: &str, value: Option<&str>, unit: &str, sub: &str, sub_tone: Option<Tone>) {
    let c = th::p();
    caption(ui, label);
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        match value {
            Some(v) => {
                text(ui, v, th::numerals(), c.text);
            }
            None => {
                text(ui, "—", th::numerals(), c.muted);
            }
        }
        if !unit.is_empty() && value.is_some() {
            ui.label(egui::RichText::new(unit).font(th::medium(13.0)).color(c.muted));
        }
    });
    let col = sub_tone.map(|t| t.colours().0).unwrap_or(c.muted);
    ui.add(egui::Label::new(egui::RichText::new(if sub.is_empty() { " " } else { sub }).font(th::small()).color(col)).wrap());
}

/// A text link (accent, underlined on hover).
pub fn link(ui: &mut Ui, s: &str) -> Response {
    let c = th::p();
    let resp = ui.add(egui::Label::new(egui::RichText::new(s).font(th::medium(12.5)).color(c.accent)).sense(Sense::click()));
    if resp.hovered() {
        let r = resp.rect;
        ui.painter().hline(r.x_range(), r.bottom(), th::stroke(1.0, c.accent));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
