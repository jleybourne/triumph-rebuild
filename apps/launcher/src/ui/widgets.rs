//! The launcher's controls, painted in theme: every one takes keyboard focus, draws a focus ring, and
//! animates hover and press.

use super::background::{self, Art};
use super::theme::{self as th, alpha, ease_out, lerp};
use eframe::egui::{
    self, epaint::Shadow, Align2, Color32, CornerRadius, Id, Mesh, Pos2, Rect, Response, Sense, StrokeKind, Ui,
    Vec2,
};

/// A rounded rect filled with a left-to-right gradient (a fan over the rounded outline).
pub fn gradient_rrect(p: &egui::Painter, r: Rect, radius: f32, left: Color32, right: Color32) {
    let radius = radius.min(r.height() / 2.0).min(r.width() / 2.0);
    let mut pts = Vec::new();
    let corners = [
        (Pos2::new(r.right() - radius, r.bottom() - radius), 0.0f32),
        (Pos2::new(r.left() + radius, r.bottom() - radius), 90.0),
        (Pos2::new(r.left() + radius, r.top() + radius), 180.0),
        (Pos2::new(r.right() - radius, r.top() + radius), 270.0),
    ];
    for (c, start) in corners {
        for k in 0..=8 {
            let a = (start + 90.0 * k as f32 / 8.0).to_radians();
            pts.push(c + Vec2::new(a.cos(), a.sin()) * radius);
        }
    }
    let col = |x: f32| lerp(left, right, (x - r.left()) / r.width().max(1.0));
    let mut m = Mesh::default();
    m.colored_vertex(r.center(), col(r.center().x));
    for q in &pts {
        m.colored_vertex(*q, col(q.x));
    }
    let n = pts.len() as u32;
    for i in 0..n {
        m.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    p.add(m);
}

/// A soft glow around a rect.
pub fn glow(p: &egui::Painter, r: Rect, radius: u8, color: Color32, strength: f32) {
    if strength <= 0.01 {
        return;
    }
    let s = Shadow { offset: [0, 4], blur: 28, spread: 2, color: alpha(color, 0.45 * strength) };
    p.add(s.as_shape(r, CornerRadius::same(radius)));
}

pub fn focus_ring(ui: &Ui, resp: &Response, r: Rect, radius: u8) {
    if resp.has_focus() {
        ui.painter().rect_stroke(r.expand(3.0), CornerRadius::same(radius.saturating_add(3)), th::stroke(2.0, alpha(th::ACCENT, 0.9)), StrokeKind::Outside);
    }
}

/// How pressed a control looks (0..1), eased.
fn press(ui: &Ui, resp: &Response) -> f32 {
    ui.ctx().animate_bool_with_time(resp.id.with("press"), resp.is_pointer_button_down_on(), th::T_PRESS * 0.5)
}
fn hover(ui: &Ui, resp: &Response) -> f32 {
    ui.ctx().animate_bool_with_time(resp.id.with("hover"), resp.hovered() || resp.has_focus(), th::T_HOVER)
}

/// Text plus an optional leading icon, laid out and painted centred in `r`.
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
    let gap = 8.0;
    let total = ig.size().x + gap + tg.size().x;
    let x = r.center().x - total / 2.0;
    p.galley(Pos2::new(x, r.center().y - ig.size().y / 2.0), ig, color);
    p.galley(Pos2::new(x + total - tg.size().x, r.center().y - tg.size().y / 2.0), tg, color);
}

/// The primary action: an accent gradient, a glow on hover, a press-scale.
pub fn primary_button(ui: &mut Ui, icon: &str, text: &str, size: Vec2, enabled: bool) -> Response {
    let (r, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let h = if enabled { hover(ui, &resp) } else { 0.0 };
    let pr = if enabled { press(ui, &resp) } else { 0.0 };
    let r2 = r.shrink2(r.size() * 0.03 * pr);
    let radius = th::R_MD as f32 + 2.0;
    if enabled {
        glow(ui.painter(), r2, radius as u8, th::ACCENT, 0.5 + 0.7 * h);
        let lift = 0.10 * h;
        gradient_rrect(ui.painter(), r2, radius, lerp(th::ACCENT, Color32::WHITE, lift), lerp(th::ACCENT_2, Color32::WHITE, lift));
        // a hairline of light along the top edge
        ui.painter().rect_stroke(r2, CornerRadius::same(radius as u8), th::stroke(1.0, Color32::from_white_alpha(40)), StrokeKind::Inside);
    } else {
        ui.painter().rect_filled(r2, CornerRadius::same(radius as u8), th::SURFACE_3);
    }
    let font = th::semibold((size.y * 0.32).max(13.5));
    label_in(ui, r2, icon, text, font, if enabled { Color32::WHITE } else { th::TEXT_3 });
    focus_ring(ui, &resp, r, radius as u8);
    resp
}

/// A quiet button: surface fill, hairline, hover lift.
pub fn button(ui: &mut Ui, icon: &str, text: &str, enabled: bool) -> Response {
    let font = th::medium(13.5);
    let w = ui.painter().layout_no_wrap(text.to_string(), font.clone(), th::TEXT).size().x + if icon.is_empty() { 28.0 } else { 52.0 };
    let size = Vec2::new(if text.is_empty() { 32.0 } else { w }, 32.0);
    let (r, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let h = if enabled { hover(ui, &resp) } else { 0.0 };
    let pr = if enabled { press(ui, &resp) } else { 0.0 };
    let r2 = r.shrink2(r.size() * 0.03 * pr);
    ui.painter().rect_filled(r2, CornerRadius::same(th::R_MD), lerp(th::SURFACE_2, th::SURFACE_3, h));
    ui.painter().rect_stroke(r2, CornerRadius::same(th::R_MD), th::stroke(1.0, lerp(th::HAIRLINE, th::HAIRLINE_HI, h)), StrokeKind::Inside);
    label_in(ui, r2, icon, text, font, if enabled { lerp(th::TEXT_2, th::TEXT, h) } else { th::TEXT_3 });
    focus_ring(ui, &resp, r, th::R_MD);
    resp
}

/// A danger-tinted quiet button.
pub fn danger_button(ui: &mut Ui, icon: &str, text: &str, enabled: bool) -> Response {
    let resp = button(ui, icon, text, enabled);
    if enabled {
        let h = hover(ui, &resp);
        ui.painter().rect_stroke(resp.rect, CornerRadius::same(th::R_MD), th::stroke(1.0, alpha(th::DANGER, 0.25 + 0.4 * h)), StrokeKind::Inside);
    }
    resp
}

/// A pill: caption type, tinted fill, optional glow.
pub fn pill(ui: &mut Ui, text: &str, color: Color32, glowing: bool) -> Response {
    let (font, track) = th::caption();
    let galley = ui.painter().layout_job(th::job(&text.to_uppercase(), (font, track), lerp(color, Color32::WHITE, 0.18)));
    let size = galley.size() + Vec2::new(18.0, 8.0);
    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
    if glowing {
        let t = ui.input(|i| i.time) as f32;
        glow(ui.painter(), r, th::R_PILL, color, 0.35 + 0.15 * (t * 2.0).sin());
    }
    ui.painter().rect_filled(r, CornerRadius::same(th::R_PILL), alpha(color, 0.14));
    ui.painter().rect_stroke(r, CornerRadius::same(th::R_PILL), th::stroke(1.0, alpha(color, 0.35)), StrokeKind::Inside);
    ui.painter().galley(r.center() - galley.size() / 2.0, galley, color);
    resp
}

/// A status pill with a dot: "System ready".
pub fn status_pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let font = th::medium(12.5);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, th::TEXT);
    let size = Vec2::new(galley.size().x + 38.0, 30.0);
    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    glow(ui.painter(), r, th::R_PILL, color, 0.45 + 0.2 * (t * 1.6).sin());
    ui.painter().rect_filled(r, CornerRadius::same(th::R_PILL), alpha(th::SURFACE_1, 0.85));
    ui.painter().rect_stroke(r, CornerRadius::same(th::R_PILL), th::stroke(1.0, alpha(color, 0.45)), StrokeKind::Inside);
    let dot = Pos2::new(r.left() + 16.0, r.center().y);
    ui.painter().circle_filled(dot, 7.0 + 1.5 * (t * 1.6).sin(), alpha(color, 0.18));
    ui.painter().circle_filled(dot, 4.0, color);
    ui.painter().galley(Pos2::new(r.left() + 28.0, r.center().y - galley.size().y / 2.0), galley, th::TEXT);
    resp
}

/// An animated switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (r, mut resp) = ui.allocate_exact_size(Vec2::new(44.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let k = ease_out(ui.ctx().animate_bool_with_time(resp.id, *on, th::T_PRESS));
    let h = hover(ui, &resp);
    let p = ui.painter();
    if k > 0.0 {
        glow(p, r, th::R_PILL, th::ACCENT, 0.4 * k);
    }
    p.rect_filled(r, CornerRadius::same(th::R_PILL), lerp(lerp(th::SURFACE_3, Color32::from_rgb(0x2A, 0x30, 0x40), h), th::ACCENT, k));
    p.rect_stroke(r, CornerRadius::same(th::R_PILL), th::stroke(1.0, th::HAIRLINE_HI), StrokeKind::Inside);
    let x = egui::lerp((r.left() + 12.0)..=(r.right() - 12.0), k);
    let pr = press(ui, &resp);
    p.circle_filled(Pos2::new(x, r.center().y + 1.0), 9.0, Color32::from_black_alpha(60));
    p.circle_filled(Pos2::new(x, r.center().y), 9.0 - 1.0 * pr, Color32::WHITE);
    focus_ring(ui, &resp, r, th::R_PILL);
    resp
}

/// A segmented control for a few choices. Arrow keys move the choice while it has focus.
pub fn segmented(ui: &mut Ui, id: Id, options: &[&str], index: &mut usize) -> bool {
    let font = th::medium(13.0);
    let widths: Vec<f32> = options.iter().map(|o| ui.painter().layout_no_wrap(o.to_string(), font.clone(), th::TEXT).size().x + 28.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 8.0;
    let (r, resp) = ui.allocate_exact_size(Vec2::new(total, 34.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let before = *index;
    if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let mut x = r.left() + 4.0;
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
    p.rect_filled(r, CornerRadius::same(th::R_MD), th::SURFACE_2);
    p.rect_stroke(r, CornerRadius::same(th::R_MD), th::stroke(1.0, th::HAIRLINE), StrokeKind::Inside);
    let x_of = |i: usize| r.left() + 4.0 + widths[..i].iter().sum::<f32>();
    let target_x = x_of(*index);
    let x = ui.ctx().animate_value_with_time(id.with("seg-x"), target_x - r.left(), th::T_PRESS) + r.left();
    let w = ui.ctx().animate_value_with_time(id.with("seg-w"), widths[*index], th::T_PRESS);
    let sel = Rect::from_min_size(Pos2::new(x, r.top() + 4.0), Vec2::new(w, r.height() - 8.0));
    glow(p, sel, th::R_SM, th::ACCENT, 0.35);
    gradient_rrect(p, sel, th::R_SM as f32, th::ACCENT, lerp(th::ACCENT, th::ACCENT_2, 0.6));
    for (i, o) in options.iter().enumerate() {
        let c = Rect::from_min_size(Pos2::new(x_of(i), r.top()), Vec2::new(widths[i], r.height()));
        let col = if i == *index { Color32::WHITE } else { th::TEXT_2 };
        p.text(c.center(), Align2::CENTER_CENTER, *o, font.clone(), col);
    }
    focus_ring(ui, &resp, r, th::R_MD);
    *index != before
}

/// A shimmering placeholder bar.
pub fn skeleton(ui: &mut Ui, size: Vec2) {
    let (r, _) = ui.allocate_exact_size(size, Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::same(th::R_SM), th::SURFACE_2);
    let sweep = ((t * 0.9) % 1.6) / 1.6;
    let x = r.left() - 120.0 + (r.width() + 240.0) * sweep;
    let band = Rect::from_min_max(Pos2::new(x - 60.0, r.top()), Pos2::new(x + 60.0, r.bottom())).intersect(r);
    if band.width() > 0.0 {
        let p2 = p.with_clip_rect(r);
        background::horizontal_fade(&p2, Rect::from_min_max(band.min, Pos2::new(band.center().x, band.bottom())), Color32::TRANSPARENT, Color32::from_white_alpha(14));
        background::horizontal_fade(&p2, Rect::from_min_max(Pos2::new(band.center().x, band.top()), band.max), Color32::from_white_alpha(14), Color32::TRANSPARENT);
    }
    ui.ctx().request_repaint();
}

/// Text with tracking.
pub fn text(ui: &mut Ui, s: &str, style: (egui::FontId, f32), color: Color32) -> Response {
    ui.add(egui::Label::new(th::job(s, style, color)).selectable(false))
}

pub fn muted(ui: &mut Ui, s: &str) -> Response {
    ui.add(egui::Label::new(egui::RichText::new(s).font(th::small()).color(th::TEXT_2)).wrap())
}

/// The time a page was opened, for staggered entrances.
pub fn page_t0(ctx: &egui::Context) -> f64 {
    ctx.data(|d| d.get_temp::<f64>(Id::new("page-t0"))).unwrap_or(0.0)
}

/// A glass card. `index` staggers its entrance after a page switch.
pub fn card<R>(ui: &mut Ui, _art: &Art, index: usize, title: Option<(&str, &str)>, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_ex(ui, index, title, None, add).0
}

/// A card that lifts on hover (a brighter rim, a deeper shadow, 2 px up). Returns its rect too.
pub fn card_hover<R>(ui: &mut Ui, index: usize, id: Id, add: impl FnOnce(&mut Ui) -> R) -> (R, Rect) {
    card_ex(ui, index, None, Some(id), add)
}

fn card_ex<R>(ui: &mut Ui, index: usize, title: Option<(&str, &str)>, hover: Option<Id>, add: impl FnOnce(&mut Ui) -> R) -> (R, Rect) {
    let now = ui.input(|i| i.time);
    let a = ease_out(((now - page_t0(ui.ctx())) as f32 - th::T_STAGGER * index as f32) / th::T_PAGE);
    if a < 1.0 {
        ui.ctx().request_repaint();
    }
    let hovered_before = hover.map(|id| ui.ctx().data(|d| d.get_temp::<bool>(id.with("hov")).unwrap_or(false))).unwrap_or(false);
    let h = hover.map(|id| ui.ctx().animate_bool_with_time(id.with("lift"), hovered_before, th::T_HOVER)).unwrap_or(0.0);
    ui.add_space((1.0 - a) * 12.0 + 2.0 * (1.0 - h));
    let (out, rect) = ui
        .scope(|ui| {
            ui.multiply_opacity(a);
            let shadow = egui::epaint::Shadow { offset: [0, (8.0 + 6.0 * h) as i8], blur: (24.0 + 12.0 * h) as u8, spread: 0, color: Color32::from_black_alpha((115.0 + 40.0 * h) as u8) };
            let frame = egui::Frame::new()
                .fill(lerp(th::glass(), alpha(th::SURFACE_2, 0.86), h))
                .stroke(th::stroke(1.0, lerp(th::HAIRLINE, alpha(th::ACCENT, 0.45), h)))
                .corner_radius(CornerRadius::same(th::R_LG))
                .inner_margin(egui::Margin::same(th::S3 as i8))
                .shadow(shadow);
            let inner = frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                if let Some((icon, t)) = title {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icon).font(th::icon(17.0)).color(th::ACCENT));
                        text(ui, t, th::h2(), th::TEXT);
                    });
                    ui.add_space(th::S1);
                }
                add(ui)
            });
            let rect = inner.response.rect;
            // the glass's top highlight
            ui.painter().line_segment(
                [Pos2::new(rect.left() + th::R_LG as f32, rect.top() + 0.5), Pos2::new(rect.right() - th::R_LG as f32, rect.top() + 0.5)],
                th::stroke(1.0, alpha(Color32::WHITE, (0.09 + 0.05 * h) * a)),
            );
            (inner.inner, rect)
        })
        .inner;
    if let Some(id) = hover {
        let over = ui.rect_contains_pointer(rect);
        ui.ctx().data_mut(|d| d.insert_temp(id.with("hov"), over));
    }
    ui.add_space(th::S2 + 2.0 * h);
    (out, rect)
}
