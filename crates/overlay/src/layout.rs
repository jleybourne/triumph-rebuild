//! The menu and the stats readout as quads: what the renderer draws, built on the input thread.
//!
//! A [`Quad`] is a solid rounded rectangle or one glyph (its atlas pixels, 1:1). Everything is placed in whole
//! pixels of the target (the shown image), sized from [`theme`] and the output's height. A [`Scene`] also lists
//! the panels' rectangles: nothing is drawn outside them (a WARP test checks the pixels).

use std::sync::Arc;

use crate::font::Font;
use crate::menu::{Menu, Page, Stats, PAGES};
use crate::theme as t;

/// `atlas[0]` of a solid quad.
pub const SOLID: u32 = u32::MAX;

/// One instance, as the shader reads it (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    /// x, y, width, height in target pixels
    pub rect: [f32; 4],
    /// the glyph's top-left in the atlas, or [`SOLID`]
    pub atlas: [u32; 2],
    /// sRGB, red in the low byte
    pub color: u32,
    /// corner radius of a solid quad, pixels
    pub radius: f32,
}

/// What the overlay draws on one target size.
#[derive(Clone, Debug)]
pub struct Scene {
    pub size: (u32, u32),
    pub font: Arc<Font>,
    pub quads: Vec<Quad>,
    /// every panel's rectangle: all quads lie inside them
    pub panels: Vec<[f32; 4]>,
}

impl Scene {
    pub fn empty(size: (u32, u32), font: Arc<Font>) -> Scene {
        Scene { size, font, quads: Vec::new(), panels: Vec::new() }
    }
}

struct Painter<'a> {
    font: &'a Font,
    s: f32,
    quads: Vec<Quad>,
}

impl Painter<'_> {
    fn px(&self, v: f32) -> f32 {
        (v * self.s).round()
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32, radius: f32) {
        if w > 0.0 && h > 0.0 {
            self.quads.push(Quad { rect: [x, y, w, h], atlas: [SOLID, 0], color, radius });
        }
    }

    /// Text with its line box's top at `top`; returns the pen's end.
    fn text(&mut self, x: f32, top: f32, s: &str, color: u32) -> f32 {
        let mut pen = x.round();
        let top = top.round();
        for c in s.chars() {
            let g = *self.font.glyph_or(c);
            if g.w > 0 {
                self.quads.push(Quad {
                    rect: [pen + f32::from(g.dx), top + f32::from(g.dy), f32::from(g.w), f32::from(g.h)],
                    atlas: [u32::from(g.x), u32::from(g.y)],
                    color,
                    radius: 0.0,
                });
            }
            pen += f32::from(g.advance);
        }
        pen
    }

    /// Text centred vertically in a row `h` high.
    fn row_text(&mut self, x: f32, y: f32, h: f32, s: &str, color: u32) -> f32 {
        self.text(x, y + ((h - self.font.line as f32) / 2.0).floor(), s, color)
    }

    fn row_text_right(&mut self, right: f32, y: f32, h: f32, s: &str, color: u32) {
        let w = self.font.measure(s) as f32;
        self.row_text(right - w, y, h, s, color);
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The Stats page's rows.
pub fn stats_rows(st: &Stats) -> Vec<(&'static str, String)> {
    let ms = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.2} ms"));
    let n = |v: Option<u64>| v.map_or("-".to_string(), thousands);
    let us = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{:.3} ms", v / 1000.0));
    let size = |v: Option<(u32, u32)>| v.map_or("-".to_string(), |(w, h)| format!("{w}x{h}"));
    vec![
        ("Frame rate", st.fps.map_or("-".to_string(), |v| format!("{v:.1} fps"))),
        ("Frame time", ms(st.ms)),
        ("GPU time", ms(st.gpu)),
        ("Wait", ms(st.wait)),
        ("Draws", n(st.draws)),
        ("Dispatches", n(st.dispatches)),
        ("Dropped", n(st.dropped)),
        ("Late presents", n(st.late)),
        ("Resolution", format!("{} → {}", size(st.internal), size(st.output))),
        ("Overlay CPU / GPU", format!("{} / {}", us(st.overlay_cpu_us), us(st.overlay_gpu_us))),
    ]
}

/// The one-line readout while the menu is closed.
pub fn hud_line(st: &Stats) -> String {
    let mut s = st.fps.map_or("- fps".to_string(), |v| format!("{v:.1} fps"));
    if let Some(v) = st.ms {
        s += &format!("  {v:.1} ms");
    }
    if let Some(v) = st.gpu {
        s += &format!("  gpu {v:.1} ms");
    }
    if let Some(v) = st.draws {
        s += &format!("  draws {}", thousands(v));
    }
    if let Some(v) = st.dropped {
        s += &format!("  dropped {}", thousands(v));
    }
    s
}

/// The hint line at the panel's foot.
pub const HINT: &str = "←→ change   A/Enter ok   B/Esc back   LB/RB Q/E page";

/// Lay out the menu (when `open`) and the stats readout (when `hud`) on a target of `size`.
pub fn build(menu: &Menu, stats: &Stats, open: bool, hud: bool, size: (u32, u32), font: Arc<Font>, key_name: &str) -> Scene {
    let mut p = Painter { font: &font, s: t::scale(size.1), quads: Vec::new() };
    let mut panels = Vec::new();
    if open {
        panels.push(panel(&mut p, menu, stats, size, key_name));
    } else if hud {
        panels.push(readout(&mut p, stats, size));
    }
    let quads = std::mem::take(&mut p.quads);
    Scene { size, font: font.clone(), quads, panels }
}

fn readout(p: &mut Painter, st: &Stats, size: (u32, u32)) -> [f32; 4] {
    let line = hud_line(st);
    let pad = p.px(t::HUD_PAD);
    let w = p.font.measure(&line) as f32 + 2.0 * pad;
    let h = p.font.line as f32 + 2.0 * pad;
    let x = (size.0 as f32 - p.px(t::MARGIN) - w).max(0.0).round();
    let y = p.px(t::MARGIN).min((size.1 as f32 - h).max(0.0));
    let r = [x, y, w.min(size.0 as f32 - x), h.min(size.1 as f32 - y)];
    p.rect(r[0], r[1], r[2], r[3], t::HUD, p.px(t::HUD_RADIUS));
    p.text(x + pad, y + pad, &line, t::TEXT);
    r
}

fn panel(p: &mut Painter, menu: &Menu, st: &Stats, size: (u32, u32), key_name: &str) -> [f32; 4] {
    let (sw, sh) = (size.0 as f32, size.1 as f32);
    let margin = p.px(t::MARGIN).min(sw / 20.0).round();
    let x0 = margin;
    let y0 = margin;
    let w = p.px(t::PANEL_W).min(sw - 2.0 * margin).max(1.0);
    let pad = p.px(t::PAD);
    let inner = (w - 2.0 * pad).max(1.0);
    let row_h = p.px(t::ROW_H).max(p.font.line as f32 + 2.0);
    let (left, right) = (x0 + pad, x0 + w - pad);
    // the panel goes under everything: drawn first, its height known at the end
    let panel_at = p.quads.len();
    p.rect(x0, y0, w, 1.0, t::PANEL, 0.0);

    // header
    let header_h = p.px(t::HEADER_H).max(row_h);
    p.rect(x0, y0, w, header_h, t::HEADER, p.px(t::RADIUS));
    p.rect(x0, y0 + header_h - p.px(t::RADIUS), w, p.px(t::RADIUS), t::HEADER, 0.0);
    p.row_text(left, y0, header_h, "Triumph rebuild", t::TITLE);
    let hint = format!("{key_name} to close");
    p.row_text_right(right, y0, header_h, &p.font.fit(&hint, (inner / 2.0) as u32), t::DIM);
    let mut y = y0 + header_h + pad / 2.0;

    // tabs
    let tab_h = p.px(t::TAB_H).max(row_h);
    let mut x = left;
    for page in PAGES {
        let label = page.title();
        let tw = p.font.measure(label) as f32 + pad;
        let on = page == menu.page();
        if on {
            p.rect(x, y, tw, tab_h, t::TAB_ON, p.px(t::ROW_RADIUS));
            p.rect(x, y + tab_h - p.px(2.0).max(1.0), tw, p.px(2.0).max(1.0), t::ACCENT, 0.0);
        }
        p.row_text(x + pad / 2.0, y, tab_h, label, if on { t::TITLE } else { t::DIM });
        x += tw + pad / 2.0;
    }
    y += tab_h + pad / 2.0;

    // rows
    let rows: Vec<(String, String, bool, bool)> = match menu.page() {
        Page::Stats => stats_rows(st).into_iter().map(|(l, v)| (l.to_string(), v, true, false)).collect(),
        _ => menu.rows().into_iter().map(|r| (r.label, r.value, r.enabled, r.adjustable)).collect(),
    };
    let selected = match (menu.page(), menu.edit()) {
        (Page::Stats, _) => None,
        (Page::Developer, Some(e)) => Some(e.row),
        _ => Some(menu.selected()),
    };
    if let (Page::Developer, Some(e)) = (menu.page(), menu.edit()) {
        let name = menu.actions().get(e.action).map_or("", |a| a.name());
        p.row_text(left, y, row_h, &format!("‹ {name}"), t::ACCENT);
        y += row_h;
    }
    if rows.is_empty() && menu.page() == Page::Developer {
        p.row_text(left, y, row_h, "No developer actions yet.", t::DIM);
        y += row_h;
    }
    let first = selected.map_or(0, |s| s.saturating_sub(t::MAX_ROWS - 1));
    let value_w = (inner * 0.45) as u32;
    for (i, (label, value, enabled, adjustable)) in rows.iter().enumerate().skip(first).take(t::MAX_ROWS) {
        let on = selected == Some(i);
        if on {
            p.rect(x0 + pad / 2.0, y, w - pad, row_h, t::SELECTED, p.px(t::ROW_RADIUS));
            p.rect(x0 + pad / 2.0, y + row_h * 0.2, p.px(t::STRIPE_W).max(2.0), row_h * 0.6, t::ACCENT, 0.0);
        }
        let label_color = if !enabled { t::DISABLED } else if on { t::TITLE } else { t::TEXT };
        let label_w = (inner as u32).saturating_sub(p.font.measure(value).min(value_w) + p.px(12.0) as u32);
        p.row_text(left + pad / 2.0, y, row_h, &p.font.fit(label, label_w), label_color);
        let shown = if on && *adjustable && !value.is_empty() { format!("‹ {value} ›") } else { value.clone() };
        let value_color = if !enabled { t::DISABLED } else if on { t::VALUE } else { t::TEXT };
        p.row_text_right(right - pad / 2.0, y, row_h, &p.font.fit(&shown, value_w), value_color);
        y += row_h;
    }

    // help, pacing, the last message
    let help = match menu.page() {
        Page::Stats => (!st.pacing.is_empty()).then(|| st.pacing.clone()),
        _ => menu.help(),
    };
    let line = p.font.line as f32;
    if let Some(h) = help {
        y += pad / 2.0;
        for l in p.font.wrap(&h, inner as u32, t::HELP_LINES) {
            p.text(left, y, &l, t::DIM);
            y += line;
        }
    }
    if let Some((m, error)) = menu.message() {
        y += pad / 2.0;
        p.text(left, y, &p.font.fit(m, inner as u32), if error { t::WARN } else { t::GOOD });
        y += line;
    }
    y += pad / 2.0;
    p.rect(left, y, inner, p.px(1.0).max(1.0), t::TAB_ON, 0.0);
    y += pad / 2.0;
    p.text(left, y, &p.font.fit(HINT, inner as u32), t::DIM);
    y += line + pad;

    let h = (y - y0).min(sh - y0).max(1.0);
    p.quads[panel_at] = Quad { rect: [x0, y0, w, h], atlas: [SOLID, 0], color: t::PANEL, radius: p.px(t::RADIUS) };
    // nothing past the panel's foot (a very short target)
    let bottom = y0 + h;
    p.quads.retain(|q| q.rect[1] + q.rect[3] <= bottom + 0.5);
    [x0, y0, w, h]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Registry;
    use crate::menu::tests::{menu, Fake};
    use crate::menu::Nav;
    use rebuild_config::Config;

    fn inside(q: &Quad, r: &[f32; 4]) -> bool {
        q.rect[0] >= r[0] && q.rect[1] >= r[1] && q.rect[0] + q.rect[2] <= r[0] + r[2] + 0.01 && q.rect[1] + q.rect[3] <= r[1] + r[3] + 0.01
    }

    #[test]
    fn every_page_lays_out_inside_its_panel_and_scales_with_height() {
        let mut b = Fake::default();
        let mut m = menu(&b);
        let stats = b_stats();
        for size in [(1920, 1080), (3840, 2160), (1280, 720)] {
            let font = Arc::new(Font::synthetic(t::font_px(size.1)));
            for _ in PAGES {
                let s = build(&m, &stats, true, false, size, font.clone(), "F10");
                assert_eq!(s.panels.len(), 1);
                let r = s.panels[0];
                assert!(s.quads.len() > 20);
                assert!(r[0] + r[2] <= size.0 as f32 && r[1] + r[3] <= size.1 as f32, "{r:?} on {size:?}");
                for q in &s.quads {
                    assert!(inside(q, &r), "{q:?} outside {r:?}");
                }
                // glyphs are whole pixels, drawn 1:1
                for q in s.quads.iter().filter(|q| q.atlas[0] != SOLID) {
                    assert!(q.rect.iter().all(|v| v.fract() == 0.0), "{q:?}");
                }
                m.nav(Nav::NextPage, &mut b);
            }
        }
        let font = Arc::new(Font::synthetic(20));
        let hd = build(&m, &stats, true, false, (1920, 1080), font.clone(), "F10").panels[0];
        let uhd = build(&m, &stats, true, false, (3840, 2160), font, "F10").panels[0];
        assert_eq!(uhd[2], hd[2] * 2.0, "the panel's width scales with the output's height");
    }

    fn b_stats() -> Stats {
        Stats { seq: 3, fps: Some(30.04), ms: Some(33.29), gpu: Some(21.0), draws: Some(4512), dropped: Some(0), pacing: "frame rate title | vsync off".into(), ..Stats::default() }
    }

    #[test]
    fn closed_draws_nothing_or_only_the_readout() {
        let b = Fake::default();
        let m = Menu::new(Config::default(), &b, Registry::default());
        let font = Arc::new(Font::synthetic(20));
        let s = build(&m, &b_stats(), false, false, (1920, 1080), font.clone(), "F10");
        assert!(s.quads.is_empty() && s.panels.is_empty());
        let s = build(&m, &b_stats(), false, true, (1920, 1080), font, "F10");
        assert_eq!(s.panels.len(), 1);
        let r = s.panels[0];
        assert!(r[0] > 960.0, "top-right: {r:?}");
        assert!(s.quads.iter().all(|q| inside(q, &r)));
        assert_eq!(hud_line(&b_stats()), "30.0 fps  33.3 ms  gpu 21.0 ms  draws 4,512  dropped 0");
    }

    #[test]
    fn the_stats_page_reads_the_frame_log() {
        let rows = stats_rows(&b_stats());
        assert_eq!(rows[0], ("Frame rate", "30.0 fps".to_string()));
        assert_eq!(rows[4], ("Draws", "4,512".to_string()));
        assert_eq!(rows[5].1, "-", "absent values show a dash");
    }

    #[test]
    fn quads_are_what_the_shader_reads() {
        assert_eq!(std::mem::size_of::<Quad>(), 32);
    }
}
