//! The upscaler backends on the card, standalone (Build 7 piece, lane `upscale`).
//!
//! A synthetic scene (`rebuild_upscale::scene`) is rendered on the CPU at the render size with Halton
//! jitter, exact motion vectors and depth, uploaded, and upscaled by the backend. Every output frame is
//! checked for NaN and black; the last frames are compared with a 4x4-supersampled truth (PSNR) and
//! with a bilinear upscale of the same input (the baseline); a static camera gives the temporal
//! stability number (mean frame-to-frame difference, 0-255 scale).
//!
//! cargo run --release --bin upscaletest -- [--output 2560x1440] [--mode quality] [--frames 64]
//!     [--out DIR] [--sweep] [--preset N]
//!
//! DLSS: REBUILD_STREAMLINE_DIR (sl.interposer.dll, sl.common.dll, sl.dlss.dll) and REBUILD_DLSS_DLL
//! (nvngx_dlss.dll; the registry entry `upscaler.dlss_dll` once the lead adds it). PNGs go to `--out`
//! (default: %TEMP%\rebuild-upscaletest), never into the repository.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use rebuild_upscale::d3d::{self, Gpu, Texture};
use rebuild_upscale::dlss::{Dlss, Signs};
use rebuild_upscale::image::{self, bad_pixels, bilinear, mean_abs_diff, psnr};
use rebuild_upscale::scene::{Frame, Scene};
use rebuild_upscale::{jitter, jitter_phases, Inputs, Mode, Size, Upscaler};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const MEASURED: u32 = 8;
const SS: u32 = 4;

struct Args {
    output: Size,
    mode: Mode,
    frames: u32,
    out: PathBuf,
    sweep: bool,
    preset: u32,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        output: Size::new(2560, 1440),
        mode: Mode::Quality,
        frames: 64,
        out: std::env::temp_dir().join("rebuild-upscaletest"),
        sweep: false,
        preset: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or(format!("{k} needs a value"));
        match k.as_str() {
            "--output" => {
                let s = v()?;
                let (w, h) = s.split_once('x').ok_or("--output WxH")?;
                a.output = Size::new(w.parse().map_err(|_| "--output WxH")?, h.parse().map_err(|_| "--output WxH")?);
            }
            "--mode" => a.mode = Mode::parse(&v()?).ok_or("--mode: native-aa, quality, balanced, performance, ultra-performance")?,
            "--frames" => a.frames = v()?.parse().map_err(|_| "--frames N")?,
            "--out" => a.out = v()?.into(),
            "--preset" => a.preset = v()?.parse().map_err(|_| "--preset N")?,
            "--sweep" => a.sweep = true,
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    if a.frames <= MEASURED {
        return Err(format!("--frames must be above {MEASURED}"));
    }
    Ok(a)
}

fn env_path(name: &str) -> Result<PathBuf, String> {
    std::env::var_os(name).map(PathBuf::from).ok_or_else(|| format!("set {name}"))
}

struct Bench {
    gpu: Gpu,
    dlss: Dlss,
    render: Size,
    output: Size,
    mode: Mode,
    colour: Texture,
    depth: Texture,
    motion: Texture,
    out: Texture,
    queries: ID3D12QueryHeap,
    stamps: ID3D12Resource,
    tick_ms: f64,
    truths: HashMap<(bool, u32), Vec<[f32; 4]>>,
}

#[derive(Default, Debug)]
struct Stats {
    psnr: f64,
    psnr_bilinear: f64,
    stability: f64,
    stability_bilinear: f64,
    nan: usize,
    black_frames: usize,
}

const SRV: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0);

impl Bench {
    fn truth(&mut self, scene: Scene, frame: u32) -> &Vec<[f32; 4]> {
        let key = (scene.moving, if scene.moving { frame } else { 0 });
        let output = self.output;
        self.truths.entry(key).or_insert_with(|| scene.truth(frame, output, SS))
    }

    /// One frame through the backend: returns the output and the GPU time of the upscale alone.
    fn step(&mut self, f: &Frame, j: [f32; 2], reset: bool) -> Result<(Vec<[f32; 4]>, f64), String> {
        let g = &self.gpu;
        g.begin();
        let _s1 = g.upload(&mut self.colour, &d3d::rgba16f(&f.colour));
        let _s2 = g.upload(&mut self.depth, &d3d::r32f(&f.depth));
        let _s3 = g.upload(&mut self.motion, &d3d::rg16f(&f.motion));
        let inputs = Inputs {
            colour: &self.colour.resource,
            depth: &self.depth.resource,
            motion: &self.motion.resource,
            exposure: None,
            output: &self.out.resource,
            render: self.render,
            output_size: self.output,
            jitter: j,
            motion_scale: [1.0, 1.0],
            depth_reversed: false,
            reset,
            hdr: false,
            pre_exposure: 1.0,
            frame_ms: 16.7,
            near: 0.1,
            far: 1000.0,
            fov_y: 1.0,
        };
        unsafe { g.list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0) };
        self.dlss.evaluate(&g.list, self.mode, &inputs)?;
        unsafe {
            g.list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            g.list.ResolveQueryData(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.stamps, 0);
        }
        let back = g.copy_out(&mut self.out);
        self.gpu.finish()?;
        let out = d3d::from_rgba16f(&Gpu::read(&self.out, &back));
        let ms = self.stamp_ms()?;
        Ok((out, ms))
    }

    /// GPU time of the upscale alone, `n` evaluations on the inputs already uploaded: (median, p90) ms.
    fn time(&mut self, n: usize) -> Result<(f64, f64), String> {
        let mut ms = Vec::with_capacity(n);
        for _ in 0..n {
            let g = &self.gpu;
            g.begin();
            let inputs = Inputs {
                colour: &self.colour.resource,
                depth: &self.depth.resource,
                motion: &self.motion.resource,
                exposure: None,
                output: &self.out.resource,
                render: self.render,
                output_size: self.output,
                jitter: [0.0, 0.0],
                motion_scale: [1.0, 1.0],
                depth_reversed: false,
                reset: false,
                hdr: false,
                pre_exposure: 1.0,
                frame_ms: 16.7,
                near: 0.1,
                far: 1000.0,
                fov_y: 1.0,
            };
            unsafe { g.list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0) };
            self.dlss.evaluate(&g.list, self.mode, &inputs)?;
            unsafe {
                g.list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                g.list.ResolveQueryData(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.stamps, 0);
            }
            self.gpu.finish()?;
            ms.push(self.stamp_ms()?);
        }
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Ok((ms[n / 2], ms[n * 9 / 10]))
    }

    fn stamp_ms(&self) -> Result<f64, String> {
        unsafe {
            let mut m: *mut std::ffi::c_void = std::ptr::null_mut();
            self.stamps.Map(0, None, Some(&mut m)).map_err(|e| e.to_string())?;
            let t = std::slice::from_raw_parts(m as *const u64, 2);
            let d = t[1].wrapping_sub(t[0]) as f64 * self.tick_ms;
            self.stamps.Unmap(0, None);
            Ok(d)
        }
    }

    /// A whole sequence from a reset: per-frame checks on every frame, quality on the last MEASURED.
    fn sequence(&mut self, scene: Scene, frames: u32, signs: Signs, keep: Option<&PathBuf>) -> Result<Stats, String> {
        self.dlss.signs = signs;
        let phases = jitter_phases(self.render, self.output);
        let mut st = Stats::default();
        let (mut prev, mut prev_b): (Option<Vec<[f32; 4]>>, Option<Vec<[f32; 4]>>) = (None, None);
        for frame in 0..frames {
            let j = jitter(frame, phases);
            let f = scene.render(frame, self.render, j);
            let (out, _) = self.step(&f, j, frame == 0)?;
            let (nan, black) = bad_pixels(&out);
            st.nan += nan;
            if black == out.len() {
                st.black_frames += 1;
            }
            if frame + MEASURED < frames {
                continue;
            }
            let b = bilinear(&f.colour, self.render, self.output);
            let truth = self.truth(scene, frame).clone();
            st.psnr += psnr(&out, &truth) / MEASURED as f64;
            st.psnr_bilinear += psnr(&b, &truth) / MEASURED as f64;
            if let (Some(p), Some(pb)) = (&prev, &prev_b) {
                st.stability += mean_abs_diff(&out, p) / (MEASURED - 1) as f64;
                st.stability_bilinear += mean_abs_diff(&b, pb) / (MEASURED - 1) as f64;
            }
            if frame + 1 == frames {
                if let Some(dir) = keep {
                    let tag = if scene.moving { "moving" } else { "static" };
                    let w = |name: &str, size: Size, px: &[[f32; 4]]| std::fs::write(dir.join(format!("{tag}-{name}.png")), image::png(size, px));
                    w("input", self.render, &f.colour).map_err(|e| e.to_string())?;
                    w("bilinear", self.output, &b).map_err(|e| e.to_string())?;
                    w("dlss", self.output, &out).map_err(|e| e.to_string())?;
                    w("truth", self.output, &truth).map_err(|e| e.to_string())?;
                    let crop = side_by_side(&[&truth, &b, &out], self.output, 2);
                    w("side-by-side-truth-bilinear-dlss", crop.0, &crop.1).map_err(|e| e.to_string())?;
                }
            }
            prev = Some(out);
            prev_b = Some(b);
        }
        Ok(st)
    }
}

/// The same centre crop of each image, `zoom`× nearest-neighbour, placed left to right.
fn side_by_side(images: &[&Vec<[f32; 4]>], size: Size, zoom: u32) -> (Size, Vec<[f32; 4]>) {
    let (cw, ch) = (size.width / 6, size.height / 4);
    let (x0, y0) = (size.width / 2 - cw / 2, size.height / 2 - ch / 2);
    let out = Size::new(cw * zoom * images.len() as u32 + 4 * (images.len() as u32 - 1), ch * zoom);
    let mut px = vec![[1.0, 1.0, 1.0, 1.0]; out.pixels()];
    for (n, img) in images.iter().enumerate() {
        let ox = n as u32 * (cw * zoom + 4);
        for y in 0..ch * zoom {
            for x in 0..cw * zoom {
                let s = img[((y0 + y / zoom) * size.width + x0 + x / zoom) as usize];
                px[(y * out.width + ox + x) as usize] = s;
            }
        }
    }
    (out, px)
}

fn main() {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("upscaletest: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(a) {
        eprintln!("upscaletest: {e}");
        std::process::exit(1);
    }
}

fn run(a: Args) -> Result<(), String> {
    let sl = env_path("REBUILD_STREAMLINE_DIR")?;
    let dll = env_path("REBUILD_DLSS_DLL")?;
    std::fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;

    println!("upscaletest — the upscaler backends on a synthetic scene, standalone");
    let logs = std::env::var_os("REBUILD_SL_LOGS").map(PathBuf::from);
    let mut dlss = Dlss::init(&sl, &dll, logs.as_deref())?;
    let gpu = Gpu::new()?;
    dlss.attach(&gpu.device, gpu.luid())?;
    dlss.preset = a.preset;
    println!("backend   {}", dlss.name());
    for m in Mode::ALL {
        let (opt, min, max) = dlss.sizes(a.output, m)?;
        let ours = m.render_size(a.output);
        println!(
            "mode      {:<18} render {}x{} (range {}x{} .. {}x{}; ratio rule gives {}x{})",
            m.name(),
            opt.width,
            opt.height,
            min.width,
            min.height,
            max.width,
            max.height,
            ours.width,
            ours.height
        );
    }
    let render = dlss.render_size(a.output, a.mode)?;
    println!("run       output {}x{}, {} → render {}x{}, {} frames, truth {SS}x{SS} supersampled, measured on the last {MEASURED}", a.output.width, a.output.height, a.mode.name(), render.width, render.height, a.frames);
    println!("out       {}", a.out.display());

    let colour = gpu.texture(render, DXGI_FORMAT_R16G16B16A16_FLOAT, false, SRV);
    let depth = gpu.texture(render, DXGI_FORMAT_R32_FLOAT, false, SRV);
    let motion = gpu.texture(render, DXGI_FORMAT_R16G16_FLOAT, false, SRV);
    let out = gpu.texture(a.output, DXGI_FORMAT_R16G16B16A16_FLOAT, true, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    let (queries, stamps, tick_ms) = unsafe {
        let mut q: Option<ID3D12QueryHeap> = None;
        gpu.device.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2, NodeMask: 0 }, &mut q).map_err(|e| e.to_string())?;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 16,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut r: Option<ID3D12Resource> = None;
        gpu.device
            .CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut r)
            .map_err(|e| e.to_string())?;
        let hz = gpu.queue.GetTimestampFrequency().map_err(|e| e.to_string())?;
        (q.unwrap(), r.unwrap(), 1000.0 / hz as f64)
    };
    let mut b = Bench { gpu, dlss, render, output: a.output, mode: a.mode, colour, depth, motion, out, queries, stamps, tick_ms, truths: HashMap::new() };

    if a.sweep {
        println!();
        println!("sweep     jitter and motion-vector signs into Streamline (moving scene, {} frames each); (+,+,+,+) is the layer's convention passed through", a.frames);
        println!("          jitter x  jitter y  motion x  motion y   PSNR dB   stability");
        let mut best = (f64::MIN, Signs::LAYER);
        for code in 0..16u32 {
            let s = |bit: u32| if code & (1 << bit) != 0 { -1.0 } else { 1.0 };
            let signs = Signs { jitter: [s(0), s(1)], motion: [s(2), s(3)] };
            let st = b.sequence(Scene { moving: true, depth_reversed: false }, a.frames, signs, None)?;
            let sign = |v: f32| if v > 0.0 { "+" } else { "-" };
            println!(
                "          {:>8}  {:>8}  {:>8}  {:>8}   {:>7.2}   {:>9.2}",
                sign(signs.jitter[0]),
                sign(signs.jitter[1]),
                sign(signs.motion[0]),
                sign(signs.motion[1]),
                st.psnr,
                st.stability
            );
            if st.psnr > best.0 {
                best = (st.psnr, signs);
            }
        }
        println!("best      {:?} at {:.2} dB", best.1, best.0);
        return Ok(());
    }

    let t = Instant::now();
    let mut all_ok = true;
    for moving in [true, false] {
        let scene = Scene { moving, depth_reversed: false };
        let st = b.sequence(scene, a.frames, Signs::STREAMLINE, Some(&a.out))?;
        let name = if moving { "moving" } else { "static" };
        println!();
        println!("{name:<9} NaN pixels {} · black frames {} (over {} frames)", st.nan, st.black_frames, a.frames);
        println!("          PSNR vs truth      DLSS {:6.2} dB   bilinear {:6.2} dB   ({:+.2} dB)", st.psnr, st.psnr_bilinear, st.psnr - st.psnr_bilinear);
        println!("          frame-to-frame     DLSS {:6.3}      bilinear {:6.3}      (mean |Δ|, 0-255; {})", st.stability, st.stability_bilinear, if moving { "the scene moves: not a stability number" } else { "static camera: the temporal-stability number" });
        all_ok &= st.nan == 0 && st.black_frames == 0 && st.psnr > st.psnr_bilinear;
        if !moving {
            all_ok &= st.stability < st.stability_bilinear;
        }
    }
    b.dlss.refresh_version()?;
    let (median, p90) = b.time(200)?;
    println!();
    println!("timing    {}: upscale alone, 200 evaluations back to back on one frame: median {median:.3} ms, p90 {p90:.3} ms", b.dlss.name());
    println!();
    println!("result    {} ({:.1} s)", if all_ok { "PASS: no NaN, no black frame, DLSS beats bilinear on PSNR and on static stability" } else { "FAIL" }, t.elapsed().as_secs_f64());
    if !all_ok {
        return Err("checks failed".into());
    }
    Ok(())
}
