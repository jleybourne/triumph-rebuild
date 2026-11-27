//! The inverse MDCT, half form: `n` coefficients give the middle `n` samples of the `2n`-sample block
//! (the other half follows by symmetry and is never needed). Computed through an `n/4`-point complex
//! FFT between a pre- and a post-rotation by `e^(-i 2pi (k + 1/8) / 2n)`, with the FFT's input in
//! bit-reversed order. This is the conventional factorisation; its output order and sign are the ones
//! WMA Pro's windowing expects.

use std::sync::OnceLock;

struct Plan {
    /// Rotation for k < n/2: (cos, sin) of 2pi (k + 1/8) / 2n, negated.
    rot: Vec<(f32, f32)>,
    /// FFT twiddles for the n/4-point transform: e^(+i 2pi k / (n/4)), k < n/8.
    tw: Vec<(f32, f32)>,
    rev: Vec<u16>,
}

fn plan(n: usize) -> &'static Plan {
    static PLANS: OnceLock<[Plan; 3]> = OnceLock::new();
    let plans = PLANS.get_or_init(|| {
        [128usize, 256, 512].map(|n| {
            let two_n = 2 * n;
            let q = n / 2;
            let bits = q.trailing_zeros();
            let rot = (0..q)
                .map(|k| {
                    let a = 2.0 * std::f64::consts::PI * (k as f64 + 0.125) / two_n as f64;
                    (-a.cos() as f32, -a.sin() as f32)
                })
                .collect();
            let tw = (0..q / 2)
                .map(|k| {
                    let a = 2.0 * std::f64::consts::PI * k as f64 / q as f64;
                    (a.cos() as f32, a.sin() as f32)
                })
                .collect();
            let rev = (0..q).map(|k| ((k as u32).reverse_bits() >> (32 - bits)) as u16).collect();
            Plan { rot, tw, rev }
        })
    });
    &plans[(n.trailing_zeros() - 7) as usize]
}

/// `input.len()` coefficients (128, 256 or 512) to as many samples, times `scale`.
pub fn imdct_half(input: &[f32], out: &mut [f32], scale: f32) {
    let n = input.len();
    let q = n / 2;
    let e = n / 4;
    let p = plan(n);
    let mut z = [(0f32, 0f32); 256];
    let z = &mut z[..q];
    // Pre-rotation of (x[n-1-2k], x[2k]) into bit-reversed order.
    for k in 0..q {
        let (are, aim) = (input[n - 1 - 2 * k], input[2 * k]);
        let (c, s) = p.rot[k];
        z[p.rev[k] as usize] = (are * c - aim * s, are * s + aim * c);
    }
    fft(z, &p.tw);
    // Post-rotation, pairing k with its mirror about n/8.
    for k in 0..e {
        let (a, b) = (e - k - 1, e + k);
        let (zr, zi) = z[a];
        let (c, s) = p.rot[a];
        let (r0, i1) = (zi * s - zr * c, zi * c + zr * s);
        let (zr, zi) = z[b];
        let (c, s) = p.rot[b];
        let (r1, i0) = (zi * s - zr * c, zi * c + zr * s);
        z[a] = (r0, i0);
        z[b] = (r1, i1);
    }
    for (k, &(re, im)) in z.iter().enumerate() {
        out[2 * k] = re * scale;
        out[2 * k + 1] = im * scale;
    }
}

/// In-place radix-2 FFT, `e^(+i 2pi jk/N)` (unnormalised), on input already in bit-reversed order.
fn fft(z: &mut [(f32, f32)], tw: &[(f32, f32)]) {
    let n = z.len();
    let mut half = 1;
    while half < n {
        let stride = n / (2 * half);
        for start in (0..n).step_by(2 * half) {
            for k in 0..half {
                let (wr, wi) = tw[k * stride];
                let (a, b) = (start + k, start + k + half);
                let (br, bi) = z[b];
                let (tr, ti) = (br * wr - bi * wi, br * wi + bi * wr);
                let (ar, ai) = z[a];
                z[b] = (ar - tr, ai - ti);
                z[a] = (ar + tr, ai + ti);
            }
        }
        half *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fast form against the same maths done directly with a DFT.
    #[test]
    fn a5_fast_imdct_matches_the_direct_sum() {
        for n in [128usize, 256, 512] {
            let input: Vec<f32> = (0..n).map(|i| (((i * 7919) % 97) as f32 - 48.0) / 7.0).collect();
            let mut fast = vec![0f32; n];
            imdct_half(&input, &mut fast, 1.0);
            // Direct: pre-rotate, DFT, post-rotate, all in f64.
            let (q, two_n) = (n / 2, 2.0 * n as f64);
            let rot = |k: usize| {
                let a = 2.0 * std::f64::consts::PI * (k as f64 + 0.125) / two_n;
                (-a.cos(), -a.sin())
            };
            let pre: Vec<(f64, f64)> = (0..q)
                .map(|k| {
                    let (are, aim) = (input[n - 1 - 2 * k] as f64, input[2 * k] as f64);
                    let (c, s) = rot(k);
                    (are * c - aim * s, are * s + aim * c)
                })
                .collect();
            let mut z: Vec<(f64, f64)> = (0..q)
                .map(|j| {
                    pre.iter().enumerate().fold((0.0, 0.0), |(r, i), (k, &(a, b))| {
                        let t = 2.0 * std::f64::consts::PI * (j * k) as f64 / q as f64;
                        (r + a * t.cos() - b * t.sin(), i + a * t.sin() + b * t.cos())
                    })
                })
                .collect();
            let e = q / 2;
            for k in 0..e {
                let (a, b) = (e - k - 1, e + k);
                let ((ar, ai), (c, s)) = (z[a], rot(a));
                let (r0, i1) = (ai * s - ar * c, ai * c + ar * s);
                let ((br, bi), (c, s)) = (z[b], rot(b));
                let (r1, i0) = (bi * s - br * c, bi * c + br * s);
                z[a] = (r0, i0);
                z[b] = (r1, i1);
            }
            let worst = z
                .iter()
                .flat_map(|&(r, i)| [r, i])
                .zip(&fast)
                .map(|(d, &f)| (d - f as f64).abs())
                .fold(0.0, f64::max);
            assert!(worst < 1e-3, "n {n}: worst {worst}");
        }
    }
}
