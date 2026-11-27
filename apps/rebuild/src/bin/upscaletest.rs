//! The upscaler backends on the card, standalone (Build 7 piece, lane `upscale`).
//!
//! A synthetic scene (`rebuild_upscale::scene`) is rendered on the CPU at the render size with Halton
//! jitter, exact motion vectors and depth, uploaded once per frame, and upscaled by every selected
//! backend from the same inputs. Every output frame is checked for NaN and black; the last frames are
//! compared with a 4x4-supersampled truth (PSNR) and with a bilinear upscale of the same input (the
//! baseline); a static camera gives the temporal stability number (mean frame-to-frame difference,
//! 0-255 scale). With two backends, a side-by-side crop shows truth | bilinear | DLSS | FSR.
//!
//! cargo run --release --bin upscaletest -- [--backend dlss|fsr|both] [--output 2560x1440]
//!     [--mode quality] [--frames 64] [--out DIR] [--sweep] [--preset N]
//!
//! DLSS: REBUILD_STREAMLINE_DIR (sl.interposer.dll, sl.common.dll, sl.dlss.dll) and REBUILD_DLSS_DLL
//! (nvngx_dlss.dll; registry `upscaler.dlss_dll`). FSR: REBUILD_FIDELITYFX_DIR (holding
//! amd_fidelityfx_loader_dx12.dll and amd_fidelityfx_upscaler_dx12.dll). PNGs go to `--out`
//! (default: %TEMP%\rebuild-upscaletest), never into the repository.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use rebuild_upscale::d3d::{self, Gpu, Texture};
use rebuild_upscale::dlss::Dlss;
use rebuild_upscale::fsr::Fsr;
use rebuild_upscale::image::{self, bad_pixels, bilinear, mean_abs_diff, psnr};
use rebuild_upscale::scene::{Frame, Scene};
use rebuild_upscale::{jitter, jitter_phases, Inputs, Mode, Signs, Size, Upscaler};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const MEASURED: u32 = 8;
const SS: u32 = 4;

type Backend = Box<dyn Upscaler<Resource = ID3D12Resource, CommandList = ID3D12GraphicsCommandList>>;

struct Args {
    dlss: bool,
    fsr: bool,
    output: Size,
    mode: Mode,
    frames: u32,
    out: PathBuf,
    sweep: bool,
    preset: u32,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        dlss: true,
        fsr: false,
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
            "--backend" => match v()?.as_str() {
                "dlss" => (a.dlss, a.fsr) = (true, false),
                "fsr" => (a.dlss, a.fsr) = (false, true),
                "both" => (a.dlss, a.fsr) = (true, true),
                _ => return Err("--backend dlss|fsr|both".into()),
            },
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
    if a.sweep && a.dlss && a.fsr {
        return Err("--sweep takes one backend".into());
    }
    Ok(a)
}

fn env_path(name: &str) -> Result<PathBuf, String> {
    std::env::var_os(name).map(PathBuf::from).ok_or_else(|| format!("set {name}"))
}

struct Bench {
    gpu: Gpu,
    backends: Vec<(&'static str, Backend, Texture)>,
    render: Size,
    output: Size,
    mode: Mode,
    colour: Texture,
    depth: Texture,
    motion: Texture,
    queries: ID3D12QueryHeap,
    stamps: ID3D12Resource,
    tick_ms: f64,
    truths: HashMap<(bool, u32), Vec<[f32; 4]>>,
}

#[derive(Default, Debug, Clone)]
struct Stats {
    psnr: f64,
    stability: f64,
    nan: usize,
    black_frames: usize,
}

const SRV: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0);

impl Bench {
    fn truth(&mut self, scene: Scene, frame: u32) -> Vec<[f32; 4]> {
        let key = (scene.moving, if scene.moving { frame } else { 0 });
        let output = self.output;
        self.truths.entry(key).or_insert_with(|| scene.truth(frame, output, SS)).clone()
    }

    fn inputs<'a>(&self, colour: &'a ID3D12Resource, depth: &'a ID3D12Resource, motion: &'a ID3D12Resource, out: &'a ID3D12Resource, j: [f32; 2], reset: bool) -> Inputs<'a, ID3D12Resource> {
        Inputs {
            colour,
            depth,
            motion,
            exposure: None,
            output: out,
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
        }
    }

    /// One frame: upload once, every backend upscales the same inputs; returns each output.
    fn step(&mut self, f: &Frame, j: [f32; 2], reset: bool) -> Result<Vec<Vec<[f32; 4]>>, String> {
        let g = &self.gpu;
        g.begin();
        let _s1 = g.upload(&mut self.colour, &d3d::rgba16f(&f.colour));
        let _s2 = g.upload(&mut self.depth, &d3d::r32f(&f.depth));
        let _s3 = g.upload(&mut self.motion, &d3d::rg16f(&f.motion));
        let mut backs = Vec::new();
        for k in 0..self.backends.len() {
            let out = self.backends[k].2.resource.clone();
            let i = self.inputs(&self.colour.resource, &self.depth.resource, &self.motion.resource, &out, j, reset);
            let (name, b, _) = &mut self.backends[k];
            b.evaluate(&g.list, self.mode, &i).map_err(|e| format!("{name}: {e}"))?;
        }
        for (_, _, out) in self.backends.iter_mut() {
            backs.push(g.copy_out(out));
        }
        self.gpu.finish()?;
        Ok(self.backends.iter().zip(&backs).map(|((_, _, t), b)| d3d::from_rgba16f(&Gpu::read(t, b))).collect())
    }

    /// GPU time of backend `k` alone, `n` evaluations back to back on the inputs already uploaded.
    fn time(&mut self, k: usize, n: usize) -> Result<(f64, f64), String> {
        let mut ms = Vec::with_capacity(n);
        for _ in 0..n {
            let g = &self.gpu;
            g.begin();
            let out = self.backends[k].2.resource.clone();
            let i = self.inputs(&self.colour.resource, &self.depth.resource, &self.motion.resource, &out, [0.0, 0.0], false);
            unsafe { g.list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0) };
            self.backends[k].1.evaluate(&g.list, self.mode, &i)?;
            unsafe {
                g.list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                g.list.ResolveQueryData(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &self.stamps, 0);
            }
            self.gpu.finish()?;
            unsafe {
                let mut m: *mut std::ffi::c_void = std::ptr::null_mut();
                self.stamps.Map(0, None, Some(&mut m)).map_err(|e| e.to_string())?;
                let t = std::slice::from_raw_parts(m as *const u64, 2);
                ms.push(t[1].wrapping_sub(t[0]) as f64 * self.tick_ms);
                self.stamps.Unmap(0, None);
            }
        }
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Ok((ms[n / 2], ms[n * 9 / 10]))
    }

    /// A whole sequence from a reset: per-frame checks on every frame, quality on the last MEASURED.
    /// Returns one Stats per backend, then the bilinear baseline.
    fn sequence(&mut self, scene: Scene, frames: u32, keep: Option<&PathBuf>) -> Result<Vec<Stats>, String> {
        let phases = jitter_phases(self.render, self.output);
        let n = self.backends.len();
        let mut st = vec![Stats::default(); n + 1];
        let mut prev: Option<Vec<Vec<[f32; 4]>>> = None;
        for frame in 0..frames {
            let j = jitter(frame, phases);
            let f = scene.render(frame, self.render, j);
            let mut outs = self.step(&f, j, frame == 0)?;
            for (k, out) in outs.iter().enumerate() {
                let (nan, black) = bad_pixels(out);
                st[k].nan += nan;
                if black == out.len() {
                    st[k].black_frames += 1;
                }
            }
            if frame + MEASURED < frames {
                continue;
            }
            outs.push(bilinear(&f.colour, self.render, self.output));
            let truth = self.truth(scene, frame);
            for (k, out) in outs.iter().enumerate() {
                st[k].psnr += psnr(out, &truth) / MEASURED as f64;
                if let Some(p) = &prev {
                    st[k].stability += mean_abs_diff(out, &p[k]) / (MEASURED - 1) as f64;
                }
            }
            if frame + 1 == frames {
                if let Some(dir) = keep {
                    let tag = if scene.moving { "moving" } else { "static" };
                    let w = |name: &str, size: Size, px: &[[f32; 4]]| std::fs::write(dir.join(format!("{tag}-{name}.png")), image::png(size, px)).map_err(|e| e.to_string());
                    w("input", self.render, &f.colour)?;
                    w("truth", self.output, &truth)?;
                    w("bilinear", self.output, &outs[n])?;
                    let mut row: Vec<&Vec<[f32; 4]>> = vec![&truth, &outs[n]];
                    let mut label = String::from("truth-bilinear");
                    for (k, (name, _, _)) in self.backends.iter().enumerate() {
                        w(name, self.output, &outs[k])?;
                        row.push(&outs[k]);
                        label += &format!("-{name}");
                    }
                    let (size, px) = side_by_side(&row, self.output, 2);
                    w(&format!("side-by-side-{label}"), size, &px)?;
                }
            }
            prev = Some(outs);
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
                px[(y * out.width + ox + x) as usize] = img[((y0 + y / zoom) * size.width + x0 + x / zoom) as usize];
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

fn sign(v: f32) -> &'static str {
    if v > 0.0 {
        "+"
    } else {
        "-"
    }
}

fn run(a: Args) -> Result<(), String> {
    std::fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;
    println!("upscaletest — the upscaler backends on a synthetic scene, standalone");

    // Streamline must be initialised before the device exists
    let mut dlss = if a.dlss {
        let logs = std::env::var_os("REBUILD_SL_LOGS").map(PathBuf::from);
        Some(Dlss::init(&env_path("REBUILD_STREAMLINE_DIR")?, &env_path("REBUILD_DLSS_DLL")?, logs.as_deref())?)
    } else {
        None
    };
    let gpu = Gpu::new()?;
    let mut backends: Vec<(&'static str, Backend)> = Vec::new();
    if let Some(mut d) = dlss.take() {
        d.attach(&gpu.device, gpu.luid())?;
        d.preset = a.preset;
        backends.push(("dlss", Box::new(d)));
    }
    if a.fsr {
        let mut f = Fsr::load(&env_path("REBUILD_FIDELITYFX_DIR")?)?;
        f.attach(&gpu.device, a.output)?;
        println!("fsr       providers in the DLL: {}", f.providers.join(", "));
        let phases = f.sdk_phases(Mode::Quality.render_size(a.output).width, a.output.width)?;
        let sdk: Vec<String> = (0..4).map(|i| f.sdk_jitter(i, phases).map(|j| format!("({:+.4}, {:+.4})", j[0], j[1]))).collect::<Result<_, _>>()?;
        let ours: Vec<String> = (0..4u32).map(|i| jitter(i, phases as u32)).map(|j| format!("({:+.4}, {:+.4})", j[0], j[1])).collect();
        println!("fsr       jitter phases {phases} (ours {}); FSR's first four {} · ours {}", jitter_phases(Mode::Quality.render_size(a.output), a.output), sdk.join(" "), ours.join(" "));
        backends.push(("fsr", Box::new(f)));
    }
    for (_, b) in &backends {
        println!("backend   {}", b.name());
    }
    for m in Mode::ALL {
        let sizes: Vec<String> = backends.iter().map(|(n, b)| b.render_size(a.output, m).map(|s| format!("{n} {}x{}", s.width, s.height))).collect::<Result<_, _>>()?;
        let ours = m.render_size(a.output);
        println!("mode      {:<18} render {} (ratio rule {}x{})", m.name(), sizes.join(", "), ours.width, ours.height);
    }
    // one render size for every backend, so they see identical inputs: the first backend's
    let render = backends[0].1.render_size(a.output, a.mode)?;
    println!("run       output {}x{}, {} → render {}x{}, {} frames, truth {SS}x{SS} supersampled, measured on the last {MEASURED}", a.output.width, a.output.height, a.mode.name(), render.width, render.height, a.frames);
    println!("out       {}", a.out.display());

    let colour = gpu.texture(render, DXGI_FORMAT_R16G16B16A16_FLOAT, false, SRV);
    let depth = gpu.texture(render, DXGI_FORMAT_R32_FLOAT, false, SRV);
    let motion = gpu.texture(render, DXGI_FORMAT_R16G16_FLOAT, false, SRV);
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
    let backends = backends.into_iter().map(|(n, b)| (n, b, gpu.texture(a.output, DXGI_FORMAT_R16G16B16A16_FLOAT, true, D3D12_RESOURCE_STATE_UNORDERED_ACCESS))).collect();
    let mut b = Bench { gpu, backends, render, output: a.output, mode: a.mode, colour, depth, motion, queries, stamps, tick_ms, truths: HashMap::new() };

    if a.sweep {
        let name = b.backends[0].0;
        println!();
        println!("sweep     {name}: jitter and motion-vector signs (moving scene, {} frames each); (+,+,+,+) is the layer's convention passed through", a.frames);
        println!("          jitter x  jitter y  motion x  motion y   PSNR dB   frame-to-frame");
        let mut best = (f64::MIN, Signs::LAYER);
        for code in 0..16u32 {
            let s = |bit: u32| if code & (1 << bit) != 0 { -1.0 } else { 1.0 };
            let signs = Signs { jitter: [s(0), s(1)], motion: [s(2), s(3)] };
            b.backends[0].1.set_signs(signs);
            let st = b.sequence(Scene { moving: true, depth_reversed: false }, a.frames, None)?;
            println!(
                "          {:>8}  {:>8}  {:>8}  {:>8}   {:>7.2}   {:>9.2}",
                sign(signs.jitter[0]),
                sign(signs.jitter[1]),
                sign(signs.motion[0]),
                sign(signs.motion[1]),
                st[0].psnr,
                st[0].stability
            );
            if st[0].psnr > best.0 {
                best = (st[0].psnr, signs);
            }
        }
        println!("best      {name} {:?} at {:.2} dB", best.1, best.0);
        return Ok(());
    }

    let t = Instant::now();
    let mut all_ok = true;
    let names: Vec<&str> = b.backends.iter().map(|x| x.0).collect();
    for moving in [true, false] {
        let st = b.sequence(Scene { moving, depth_reversed: false }, a.frames, Some(&a.out))?;
        let base = st.last().unwrap().clone();
        println!();
        println!("{}  ({} frames; quality on the last {MEASURED})", if moving { "moving" } else { "static" }, a.frames);
        println!("          {:<9} {:>10} {:>12} {:>14} {:>8} {:>13}", "", "PSNR dB", "vs bilinear", "frame-to-frame", "NaN px", "black frames");
        for (k, s) in st.iter().enumerate() {
            let name = names.get(k).copied().unwrap_or("bilinear");
            println!("          {:<9} {:>10.2} {:>+12.2} {:>14.3} {:>8} {:>13}", name, s.psnr, s.psnr - base.psnr, s.stability, s.nan, s.black_frames);
            if k < names.len() {
                all_ok &= s.nan == 0 && s.black_frames == 0 && s.psnr > base.psnr;
                if !moving {
                    all_ok &= s.stability < base.stability;
                }
            }
        }
        println!("          (frame-to-frame = mean |Δ| between consecutive outputs, 0-255; {})", if moving { "the scene moves: not a stability number" } else { "static camera: the temporal-stability number" });
    }
    println!();
    for k in 0..b.backends.len() {
        let (median, p90) = b.time(k, 200)?;
        println!("timing    {:<5} upscale alone, 200 evaluations back to back: median {median:.3} ms, p90 {p90:.3} ms — {}", names[k], b.backends[k].1.name());
    }
    println!();
    println!(
        "result    {} ({:.1} s)",
        if all_ok { "PASS: no NaN, no black frame; every backend beats bilinear on PSNR and on static stability" } else { "FAIL" },
        t.elapsed().as_secs_f64()
    );
    if !all_ok {
        return Err("checks failed".into());
    }
    Ok(())
}
