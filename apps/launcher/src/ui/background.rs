//! The moving background and the Play hero's key art. No texture or grain anywhere.
//!
//! The background is a vertex-coloured mesh: three soft glows in the accent family drift slowly over the
//! base colour. Key art is decoded
//! at run time from the game folder (read-only) and never copied anywhere.

use super::theme::{self, alpha, lerp};
use eframe::egui::{self, Color32, ColorImage, Mesh, Pos2, Rect, TextureHandle, TextureOptions, Vec2};

pub struct Art {
    pub key_art: Option<(TextureHandle, Vec2)>,
    pub logo: Option<(TextureHandle, Vec2)>,
}

impl Art {
    pub fn new(ctx: &egui::Context) -> Art {
        let _ = ctx;
        Art { key_art: None, logo: None }
    }

    pub fn load_title(&mut self, ctx: &egui::Context, visuals: &rebuild_launcher::gamemeta::Visuals) {
        let load = |p: &std::path::Path, name: &str| {
            let img = rebuild_launcher::gamemeta::load_png(p).ok()?;
            let size = Vec2::new(img.width as f32, img.height as f32);
            let ci = ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
            let opts = TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..TextureOptions::LINEAR };
            Some((ctx.load_texture(name, ci, opts), size))
        };
        self.key_art = visuals.splash.as_deref().and_then(|p| load(p, "key-art"));
        self.logo = visuals.logo.as_deref().and_then(|p| load(p, "title-logo"));
    }
}

/// The full-window background: base colour plus three drifting glows.
pub fn paint(p: &egui::Painter, rect: Rect, t: f32, intensity: f32) {
    p.rect_filled(rect, 0.0, theme::BG_BASE);
    let blobs = [
        (Pos2::new(0.18 + 0.06 * (t * 0.11).sin(), 0.12 + 0.05 * (t * 0.07).cos()), 0.55, theme::ACCENT, 0.30),
        (Pos2::new(0.86 + 0.05 * (t * 0.09).cos(), 0.28 + 0.07 * (t * 0.05).sin()), 0.50, theme::ACCENT_2, 0.22),
        (Pos2::new(0.55 + 0.10 * (t * 0.04).sin(), 1.05 + 0.04 * (t * 0.08).cos()), 0.65, theme::rgb(0xB0, 0x4C, 0xE8), 0.14),
    ];
    let (cols, rows) = (28usize, 18usize);
    let mut m = Mesh::default();
    let aspect = rect.width() / rect.height().max(1.0);
    for j in 0..=rows {
        for i in 0..=cols {
            let u = i as f32 / cols as f32;
            let v = j as f32 / rows as f32;
            let mut c = theme::BG_BASE;
            for (centre, radius, col, a) in blobs {
                let d = Vec2::new((u - centre.x) * aspect, v - centre.y).length() / (radius * aspect.max(1.0) * 0.75);
                let w = (-d * d * 2.2).exp() * a * intensity;
                c = lerp(c, col, w);
            }
            m.colored_vertex(Pos2::new(rect.left() + u * rect.width(), rect.top() + v * rect.height()), c);
        }
    }
    let w = (cols + 1) as u32;
    for j in 0..rows as u32 {
        for i in 0..cols as u32 {
            let a = j * w + i;
            m.add_triangle(a, a + 1, a + w);
            m.add_triangle(a + 1, a + w + 1, a + w);
        }
    }
    p.add(m);
}

/// A soft radial glow: `color` at the centre, gone at `radius`.
pub fn radial(p: &egui::Painter, centre: Pos2, radius: f32, color: Color32) {
    let n = 48;
    let mut m = Mesh::default();
    m.colored_vertex(centre, color);
    let rings = [(0.35f32, 0.62f32), (0.7, 0.22), (1.0, 0.0)];
    for (rr, a) in rings {
        for k in 0..n {
            let ang = k as f32 / n as f32 * std::f32::consts::TAU;
            let c = Color32::from_rgba_premultiplied(
                (color.r() as f32 * a) as u8,
                (color.g() as f32 * a) as u8,
                (color.b() as f32 * a) as u8,
                (color.a() as f32 * a) as u8,
            );
            m.colored_vertex(centre + Vec2::angled(ang) * radius * rr, c);
        }
    }
    let n = n as u32;
    for k in 0..n {
        m.add_triangle(0, 1 + k, 1 + (k + 1) % n);
    }
    for ring in 0..2u32 {
        let (a0, b0) = (1 + ring * n, 1 + (ring + 1) * n);
        for k in 0..n {
            let k1 = (k + 1) % n;
            m.add_triangle(a0 + k, a0 + k1, b0 + k);
            m.add_triangle(a0 + k1, b0 + k1, b0 + k);
        }
    }
    p.add(m);
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The Play hero. A generated glow always (it is all there is without key art); key art, when the title
/// ships one, covers the right of the hero with a slow drift and fades out to the left and the bottom,
/// so the title, the meta line and Launch sit on a dark, readable area and nothing ends in a hard edge.
pub fn hero(p: &egui::Painter, art: &Art, rect: Rect, t: f32) {
    let w = rect.width();
    let h = rect.height();
    radial(p, rect.left_top() + Vec2::new(w * (0.70 + 0.04 * (t * 0.11).sin()), h * (0.38 + 0.05 * (t * 0.07).cos())), w * 0.42, alpha(theme::ACCENT, 0.34));
    radial(p, rect.left_top() + Vec2::new(w * (0.94 + 0.03 * (t * 0.08).cos()), h * (0.12 + 0.04 * (t * 0.06).sin())), w * 0.30, alpha(theme::ACCENT_2, 0.30));
    radial(p, rect.left_top() + Vec2::new(w * (0.48 + 0.05 * (t * 0.05).sin()), h * 0.95), w * 0.34, alpha(theme::rgb(0xB0, 0x4C, 0xE8), 0.16));
    if let Some((tex, size)) = &art.key_art {
        let area = Rect::from_min_max(Pos2::new(rect.left() + w * 0.42, rect.top()), rect.max);
        let scale = (area.width() / size.x).max(area.height() / size.y) * (1.05 + 0.012 * (t * 0.15).sin());
        let shown = *size * scale;
        let drift = Vec2::new(8.0 * (t * 0.07).sin(), 5.0 * (t * 0.05).cos());
        let img = Rect::from_min_size(
            Pos2::new(area.right() - shown.x + 12.0 + drift.x, area.top() + (area.height() - shown.y) * 0.4 + drift.y),
            shown,
        );
        let (cols, rows) = (16u32, 10u32);
        let mut m = Mesh::with_texture(tex.id());
        for j in 0..=rows {
            for i in 0..=cols {
                let (u, v) = (i as f32 / cols as f32, j as f32 / rows as f32);
                let pos = Pos2::new(area.left() + u * area.width(), area.top() + v * area.height());
                let uv = Pos2::new((pos.x - img.left()) / shown.x, (pos.y - img.top()) / shown.y);
                let a = smooth(0.0, 0.55, u) * (1.0 - smooth(0.45, 1.0, v)) * 0.95;
                m.vertices.push(egui::epaint::Vertex { pos, uv, color: Color32::from_white_alpha((a * 255.0) as u8) });
            }
        }
        let wv = cols + 1;
        for j in 0..rows {
            for i in 0..cols {
                let a = j * wv + i;
                m.add_triangle(a, a + 1, a + wv);
                m.add_triangle(a + 1, a + wv + 1, a + wv);
            }
        }
        p.add(m);
    }
    // the reading area: dark on the left, clearing by 85 % of the width, and itself fading out towards
    // the bottom so the hero has no edge
    let (cols, rows) = (12u32, 10u32);
    let mut m = Mesh::default();
    let sw = w * 0.85;
    for j in 0..=rows {
        for i in 0..=cols {
            let (u, v) = (i as f32 / cols as f32, j as f32 / rows as f32);
            // at least 80 % over the left half, where the title, the meta line and Launch sit
            let a = 0.94 * (1.0 - smooth(0.55, 1.0, u)) * (1.0 - smooth(0.6, 1.0, v));
            m.colored_vertex(Pos2::new(rect.left() + u * sw, rect.top() + v * h), alpha(theme::BG_BASE, a));
        }
    }
    let wv = cols + 1;
    for j in 0..rows {
        for i in 0..cols {
            let a = j * wv + i;
            m.add_triangle(a, a + 1, a + wv);
            m.add_triangle(a + 1, a + wv + 1, a + wv);
        }
    }
    p.add(m);
    // and the top, under the title bar
    vertical_fade(p, Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.top() + 72.0)), alpha(theme::BG_BASE, 0.55), Color32::TRANSPARENT);
}

pub fn vertical_fade(p: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.left_bottom(), bottom);
    m.colored_vertex(r.right_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 3, 2);
    p.add(m);
}

pub fn horizontal_fade(p: &egui::Painter, r: Rect, left: Color32, right: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), left);
    m.colored_vertex(r.right_top(), right);
    m.colored_vertex(r.left_bottom(), left);
    m.colored_vertex(r.right_bottom(), right);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 3, 2);
    p.add(m);
}
