//! A synthetic test scene, rendered analytically on the CPU at any size: a panning textured
//! background, a moving rotating square and a disc on a circular path, each at its own depth.
//!
//! One sample per pixel at the jittered position (what a game hands a temporal upscaler), exact
//! motion vectors from the analytic motion, device depth, and a supersampled ground truth at the
//! output size. Conventions are the crate's (see `lib.rs`): jitter and motion in render pixels, +y down,
//! motion = previous − current.
//!
//! Scene space: y spans [0, 1] top to bottom, x spans [0, ASPECT] left to right, for every image size
//! of that aspect, so a render-size frame and an output-size truth show the same picture.

use crate::Size;

pub const ASPECT: f32 = 16.0 / 9.0;

/// Which layer a sample hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Disc,
    Square,
    Background,
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub layer: Layer,
    /// The surface point in the layer's own coordinates (fixed to the surface as it moves).
    pub local: [f32; 2],
    pub depth: f32,
    pub colour: [f32; 3],
}

/// One rendered frame at render size.
pub struct Frame {
    pub size: Size,
    /// RGBA, linear, alpha 1.
    pub colour: Vec<[f32; 4]>,
    /// Device depth, 0 near unless the scene is reversed.
    pub depth: Vec<f32>,
    /// Render pixels, previous − current, +y down.
    pub motion: Vec<[f32; 2]>,
}

#[derive(Clone, Copy, Debug)]
pub struct Scene {
    /// Objects and camera move. False: a static camera on a static scene (the stability test).
    pub moving: bool,
    /// Reversed-Z depth (near = 1).
    pub depth_reversed: bool,
}

fn rot(a: f32, v: [f32; 2]) -> [f32; 2] {
    let (s, c) = a.sin_cos();
    [c * v[0] - s * v[1], s * v[0] + c * v[1]]
}

fn fract(x: f32) -> f32 {
    x - x.floor()
}

fn checker(v: [f32; 2], cells: f32) -> f32 {
    let a = (v[0] * cells).floor() as i64 + (v[1] * cells).floor() as i64;
    (a & 1) as f32
}

impl Scene {
    fn t(&self, frame: f32) -> f32 {
        if self.moving {
            frame
        } else {
            0.0
        }
    }

    /// Camera offset in scene units: the background scrolls left and up as the camera pans.
    fn camera(&self, t: f32) -> [f32; 2] {
        [0.0021 * t, 0.0009 * t]
    }

    fn disc_centre(&self, t: f32) -> [f32; 2] {
        let w = 0.031 * t;
        [0.95 + 0.42 * w.cos(), 0.5 + 0.22 * w.sin()]
    }

    fn square_centre(&self, t: f32) -> [f32; 2] {
        [0.45 + 0.0037 * t, 0.55 - 0.0011 * t]
    }

    fn square_angle(&self, t: f32) -> f32 {
        0.4 + 0.013 * t
    }

    fn depth(&self, d: f32) -> f32 {
        if self.depth_reversed {
            1.0 - d
        } else {
            d
        }
    }

    /// Where a layer's surface point `local` is on screen at time `frame` (scene units).
    pub fn project(&self, layer: Layer, local: [f32; 2], frame: f32) -> [f32; 2] {
        let t = self.t(frame);
        let cam = self.camera(t);
        let world = match layer {
            Layer::Disc => {
                let c = self.disc_centre(t);
                [c[0] + local[0], c[1] + local[1]]
            }
            Layer::Square => {
                let c = self.square_centre(t);
                let r = rot(self.square_angle(t), local);
                [c[0] + r[0], c[1] + r[1]]
            }
            Layer::Background => local,
        };
        [world[0] - cam[0], world[1] - cam[1]]
    }

    /// What is visible at screen point `p` (scene units) at time `frame`.
    pub fn sample(&self, frame: f32, p: [f32; 2]) -> Hit {
        let t = self.t(frame);
        let cam = self.camera(t);
        let world = [p[0] + cam[0], p[1] + cam[1]];
        let tau = std::f32::consts::TAU;

        let c = self.disc_centre(t);
        let q = [world[0] - c[0], world[1] - c[1]];
        let r = (q[0] * q[0] + q[1] * q[1]).sqrt();
        if r < 0.16 {
            let ring = 0.5 + 0.5 * (tau * r * 55.0).cos();
            let spoke = if fract(q[1].atan2(q[0]) / tau * 12.0) < 0.5 { 1.0 } else { 0.35 };
            let v = ring * spoke;
            return Hit { layer: Layer::Disc, local: q, depth: self.depth(0.2), colour: [0.95 * v + 0.05, 0.55 * v + 0.05, 0.1 * v + 0.05] };
        }

        let c = self.square_centre(t);
        let q = rot(-self.square_angle(t), [world[0] - c[0], world[1] - c[1]]);
        if q[0].abs() < 0.17 && q[1].abs() < 0.17 {
            let ch = checker(q, 18.0);
            // thin diagonal lines: the classic aliasing case
            let line = if fract((q[0] + q[1]) * 30.0) < 0.12 { 1.0 } else { 0.0 };
            let v = 0.15 + 0.7 * ch;
            let col = [v * 0.3 + 0.6 * line, v * 0.8 + 0.2 * line, v + 0.0 * line];
            return Hit { layer: Layer::Square, local: q, depth: self.depth(0.4), colour: col.map(|x: f32| x.min(1.0)) };
        }

        let ch = checker(world, 40.0);
        let zone = 0.5 + 0.5 * (tau * (world[0] * world[0] * 9.0 + world[1] * world[1] * 7.0) * 6.0).cos();
        let stripe = if fract(world[1] * 90.0) < 0.2 { 0.0 } else { 1.0 };
        Hit {
            layer: Layer::Background,
            local: world,
            depth: self.depth(0.9),
            colour: [0.2 + 0.5 * ch * stripe, 0.25 + 0.45 * zone, 0.3 + 0.3 * ch],
        }
    }

    /// Screen point (scene units) of pixel coordinate (x, y), which may be fractional.
    pub fn point(size: Size, x: f32, y: f32) -> [f32; 2] {
        [x / size.width as f32 * ASPECT, y / size.height as f32]
    }

    /// Render one frame: one sample per pixel at the pixel centre plus `jitter` (render pixels, +y down).
    pub fn render(&self, frame: u32, size: Size, jitter: [f32; 2]) -> Frame {
        let tf = frame as f32;
        let (sx, sy) = (size.width as f32 / ASPECT, size.height as f32);
        let px = per_pixel(size, |x, y| {
            let p = Self::point(size, x as f32 + 0.5 + jitter[0], y as f32 + 0.5 + jitter[1]);
            let h = self.sample(tf, p);
            let prev = self.project(h.layer, h.local, tf - 1.0);
            ([h.colour[0], h.colour[1], h.colour[2], 1.0], h.depth, [(prev[0] - p[0]) * sx, (prev[1] - p[1]) * sy])
        });
        Frame {
            size,
            colour: px.iter().map(|p| p.0).collect(),
            depth: px.iter().map(|p| p.1).collect(),
            motion: px.iter().map(|p| p.2).collect(),
        }
    }

    /// The reference picture at `size`: `ss` × `ss` samples per pixel, no jitter.
    pub fn truth(&self, frame: u32, size: Size, ss: u32) -> Vec<[f32; 4]> {
        let tf = frame as f32;
        let w = 1.0 / (ss * ss) as f32;
        per_pixel(size, |x, y| {
            let mut c = [0.0f32; 3];
            for j in 0..ss {
                for i in 0..ss {
                    let p = Self::point(size, x as f32 + (i as f32 + 0.5) / ss as f32, y as f32 + (j as f32 + 0.5) / ss as f32);
                    let h = self.sample(tf, p);
                    for k in 0..3 {
                        c[k] += h.colour[k] * w;
                    }
                }
            }
            [c[0], c[1], c[2], 1.0]
        })
    }
}

/// Evaluate `f` for every pixel, rows split over at most 4 threads (leaves cores for the lead's runs).
fn per_pixel<T: Send + Copy + Default>(size: Size, f: impl Fn(u32, u32) -> T + Sync) -> Vec<T> {
    let mut out = vec![T::default(); size.pixels()];
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
    let rows = (size.height as usize).div_ceil(threads).max(1);
    let w = size.width as usize;
    std::thread::scope(|s| {
        for (c, chunk) in out.chunks_mut(rows * w).enumerate() {
            let f = &f;
            s.spawn(move || {
                for (i, v) in chunk.iter_mut().enumerate() {
                    let y = c * rows + i / w;
                    *v = f((i % w) as u32, y as u32);
                }
            });
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RENDER: Size = Size::new(320, 180);

    /// Every motion vector, followed back into the previous frame, lands on the same surface point
    /// (same layer, same local coordinates) unless that point was hidden there.
    #[test]
    fn motion_vectors_match_analytic_motion() {
        let s = Scene { moving: true, depth_reversed: false };
        let frame = 40;
        let j = crate::jitter(frame, 8);
        let f = s.render(frame, RENDER, j);
        let (mut checked, mut hidden, mut worst) = (0usize, 0usize, 0.0f32);
        for y in 0..RENDER.height {
            for x in 0..RENDER.width {
                let i = (y * RENDER.width + x) as usize;
                let px = x as f32 + 0.5 + j[0] + f.motion[i][0];
                let py = y as f32 + 0.5 + j[1] + f.motion[i][1];
                let now = s.sample(frame as f32, Scene::point(RENDER, x as f32 + 0.5 + j[0], y as f32 + 0.5 + j[1]));
                let then = s.sample(frame as f32 - 1.0, Scene::point(RENDER, px, py));
                if then.layer != now.layer {
                    hidden += 1; // disoccluded: the point was behind something last frame
                    continue;
                }
                let d = (then.local[0] - now.local[0]).abs().max((then.local[1] - now.local[1]).abs());
                worst = worst.max(d);
                checked += 1;
            }
        }
        assert!(worst < 1e-4, "worst local-coordinate error {worst}");
        assert!(checked > RENDER.pixels() * 98 / 100, "checked {checked}, hidden {hidden}");
        // something actually moves, and differently per layer
        let max = f.motion.iter().map(|m| m[0].abs().max(m[1].abs())).fold(0.0, f32::max);
        assert!(max > 0.5, "max motion {max}");
    }

    #[test]
    fn background_motion_is_exactly_the_camera_pan() {
        let s = Scene { moving: true, depth_reversed: false };
        let f = s.render(10, RENDER, [0.0, 0.0]);
        let (sx, sy) = (RENDER.width as f32 / ASPECT, RENDER.height as f32);
        // the camera pans right/down by (0.0021, 0.0009) a frame, so last frame the point was further right/down
        let want = [0.0021 * sx, 0.0009 * sy];
        let mut n = 0;
        for (i, m) in f.motion.iter().enumerate() {
            if f.depth[i] == 0.9 {
                assert!((m[0] - want[0]).abs() < 1e-3 && (m[1] - want[1]).abs() < 1e-3, "{m:?} vs {want:?}");
                n += 1;
            }
        }
        assert!(n > RENDER.pixels() / 2);
    }

    #[test]
    fn static_scene_has_zero_motion_and_depth_orders_layers() {
        let s = Scene { moving: false, depth_reversed: false };
        let f = s.render(7, RENDER, crate::jitter(7, 8));
        assert!(f.motion.iter().all(|m| m[0].abs() < 1e-4 && m[1].abs() < 1e-4)); // rotation round trip only
        let r = Scene { moving: false, depth_reversed: true };
        let g = r.render(7, RENDER, crate::jitter(7, 8));
        assert!(f.depth.iter().zip(&g.depth).all(|(a, b)| (a + b - 1.0).abs() < 1e-6));
        let mut seen: Vec<f32> = f.depth.clone();
        seen.sort_by(|a, b| a.partial_cmp(b).unwrap());
        seen.dedup();
        assert_eq!(seen, vec![0.2, 0.4, 0.9]);
    }

    #[test]
    fn truth_is_finite_and_not_black() {
        let s = Scene { moving: true, depth_reversed: false };
        let t = s.truth(5, Size::new(96, 54), 4);
        assert!(t.iter().all(|c| c.iter().all(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0)));
        let mean: f32 = t.iter().map(|c| c[0] + c[1] + c[2]).sum::<f32>() / t.len() as f32;
        assert!(mean > 0.3, "mean {mean}");
    }
}
