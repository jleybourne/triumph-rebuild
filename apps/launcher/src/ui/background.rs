//! The title's key art and logo, decoded at run time from the game folder (read-only) and never copied
//! anywhere; drawn still (nothing moves, so an idle launcher draws nothing).

use super::theme::{self as th, alpha};
use eframe::egui::{self, Color32, ColorImage, Mesh, Pos2, Rect, TextureHandle, TextureOptions, Vec2};

pub struct Art {
    pub key_art: Option<(TextureHandle, Vec2)>,
    pub logo: Option<(TextureHandle, Vec2)>,
}

impl Art {
    pub fn new() -> Art {
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

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Key art over the right half of the Play header band, fading into the band's fill to the left so the
/// text stays readable. Nothing when the title ships no key art.
pub fn band_art(p: &egui::Painter, art: &Art, rect: Rect) {
    let Some((tex, size)) = &art.key_art else { return };
    let area = Rect::from_min_max(Pos2::new(rect.left() + rect.width() * 0.45, rect.top()), rect.max);
    let scale = (area.width() / size.x).max(area.height() / size.y);
    let shown = *size * scale;
    let img = Rect::from_min_size(Pos2::new(area.right() - shown.x, area.top() + (area.height() - shown.y) * 0.4), shown);
    let (cols, rows) = (12u32, 1u32);
    let mut m = Mesh::with_texture(tex.id());
    let strength = if th::is_dark() { 0.55 } else { 0.35 };
    for j in 0..=rows {
        for i in 0..=cols {
            let (u, v) = (i as f32 / cols as f32, j as f32 / rows as f32);
            let pos = Pos2::new(area.left() + u * area.width(), area.top() + v * area.height());
            let uv = Pos2::new((pos.x - img.left()) / shown.x, (pos.y - img.top()) / shown.y);
            let a = smooth(0.0, 0.7, u) * strength;
            m.vertices.push(egui::epaint::Vertex { pos, uv, color: alpha(Color32::WHITE, a) });
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
    p.with_clip_rect(rect).add(m);
}
