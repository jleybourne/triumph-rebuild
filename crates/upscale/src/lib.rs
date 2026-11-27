//! One upscaler interface, two backends (plan decision 5).
//!
//! The layer finds the title's temporal upscale pass by its bindings and hands its inputs to an
//! [`Upscaler`]: NVIDIA → DLSS through Streamline ([`dlss`]), AMD → FSR through the FidelityFX SDK.
//! Every input is described here in **one convention, the layer's own**; each backend converts to its
//! SDK's convention inside `evaluate`, so the title-side code never changes per vendor. The per-SDK
//! conversions were measured, not assumed: `upscaletest --sweep` (see `agents/upscale.md`, P5).
//!
//! The layer's convention, in short:
//!
//! ```text
//! jitter     render pixels; the offset of this frame's sample position from the pixel centre;
//!            +x right, +y DOWN (texture space). Range [-0.5, 0.5].
//! motion     stored value × mv_scale = render pixels from the CURRENT sample position to where the
//!            same surface point was in the PREVIOUS frame (previous − current); +x right, +y down.
//!            Not jittered (computed from the unjittered projections).
//! depth      device depth in [0, 1]; `depth_reversed` says whether near is 1 (reversed-Z) or 0.
//! exposure   optional 1x1 R32_FLOAT texture; without it the colour is taken as pre-exposed by
//!            `pre_exposure` (1.0 = as is).
//! reset      true on a cut: the history is discarded and this frame is upscaled on its own.
//! ```

pub mod image;
pub mod scene;

#[cfg(windows)]
pub mod d3d;
#[cfg(windows)]
pub mod dlss;
#[cfg(windows)]
pub mod fsr;

/// How a backend maps the layer's convention onto its SDK's: each factor multiplies one axis.
/// Measured per SDK with `upscaletest --sweep` (`agents/upscale.md`, P5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Signs {
    /// Multiplies the layer's jitter (+y down) before it goes to the SDK.
    pub jitter: [f32; 2],
    /// Multiplies the layer's motion scale before it goes to the SDK.
    pub motion: [f32; 2],
}

impl Signs {
    /// The layer's convention passed through unchanged (what the sweep calls (+,+,+,+)).
    pub const LAYER: Signs = Signs { jitter: [1.0, 1.0], motion: [1.0, 1.0] };
    /// Measured on the RTX 4070 Ti (2560x1440 quality, 16 sign combinations): Streamline's
    /// `jitterOffset` is the NEGATED sample offset on both axes (31.58 dB against 27.16 dB passed
    /// through); its motion vectors are previous − current, +y down, like the layer's (every flipped
    /// motion sign falls to 18-20 dB).
    pub const STREAMLINE: Signs = Signs { jitter: [-1.0, -1.0], motion: [1.0, 1.0] };
    /// Measured the same way for FidelityFX FSR 3.1.6: the same mapping as Streamline (33.00 dB against
    /// 27.77 dB passed through; every flipped motion sign falls to 20-22 dB). FSR's own jitter query
    /// returns exactly the layer's Halton values, so FSR's `jitterOffset` is likewise the negated
    /// sample offset.
    pub const FIDELITYFX: Signs = Signs { jitter: [-1.0, -1.0], motion: [1.0, 1.0] };
}

/// The file version of a DLL ("major.minor.build.revision"), for the frame line's `upscaler` field.
#[cfg(windows)]
pub fn file_version(path: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO};
    let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let n = GetFileVersionInfoSizeW(PCWSTR(w.as_ptr()), None);
        if n == 0 {
            return None;
        }
        let mut buf = vec![0u8; n as usize];
        GetFileVersionInfoW(PCWSTR(w.as_ptr()), 0, n, buf.as_mut_ptr().cast()).ok()?;
        let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let root: Vec<u16> = "\\".encode_utf16().chain(Some(0)).collect();
        if !VerQueryValueW(buf.as_ptr().cast(), PCWSTR(root.as_ptr()), &mut p, &mut len).as_bool() || p.is_null() {
            return None;
        }
        let f = &*(p as *const VS_FIXEDFILEINFO);
        Some(format!("{}.{}.{}.{}", f.dwFileVersionMS >> 16, f.dwFileVersionMS & 0xffff, f.dwFileVersionLS >> 16, f.dwFileVersionLS & 0xffff))
    }
}

/// The typed format to view a resource as when its own format is typeless. The title's textures
/// arrive typeless on the host (`rebuild-d3d12x` keeps the typed view format apart), and both SDKs
/// build views from the format they are given, so the backends name the typed one.
#[cfg(windows)]
pub fn typed(f: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
    use windows::Win32::Graphics::Dxgi::Common::*;
    match f {
        DXGI_FORMAT_R32G32B32A32_TYPELESS => DXGI_FORMAT_R32G32B32A32_FLOAT,
        DXGI_FORMAT_R16G16B16A16_TYPELESS => DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R16G16_TYPELESS => DXGI_FORMAT_R16G16_FLOAT,
        DXGI_FORMAT_R32_TYPELESS | DXGI_FORMAT_D32_FLOAT => DXGI_FORMAT_R32_FLOAT,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_R16_TYPELESS => DXGI_FORMAT_R16_FLOAT,
        other => other,
    }
}

/// A width and height in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Size {
        Size { width, height }
    }
    pub fn pixels(self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Quality mode: the ratio of output size to render size on each axis.
///
/// The ratios are the ones both SDKs publish for their named modes (FSR 3.1 and DLSS agree except
/// that DLSS's Balanced is 1/0.58 ≈ 1.72; the backend asks its SDK for the exact render size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Anti-aliasing only, render size = output size (DLAA / FSR native AA).
    NativeAa,
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
}

impl Mode {
    pub const ALL: [Mode; 5] = [Mode::NativeAa, Mode::Quality, Mode::Balanced, Mode::Performance, Mode::UltraPerformance];

    /// Output / render, per axis.
    pub fn ratio(self) -> f32 {
        match self {
            Mode::NativeAa => 1.0,
            Mode::Quality => 1.5,
            Mode::Balanced => 1.7,
            Mode::Performance => 2.0,
            Mode::UltraPerformance => 3.0,
        }
    }

    /// The render size for an output size: each axis divided by the ratio, rounded down, at least 1.
    pub fn render_size(self, output: Size) -> Size {
        let r = self.ratio();
        let f = |v: u32| ((v as f32 / r) as u32).max(1);
        Size::new(f(output.width), f(output.height))
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::NativeAa => "native-aa",
            Mode::Quality => "quality",
            Mode::Balanced => "balanced",
            Mode::Performance => "performance",
            Mode::UltraPerformance => "ultra-performance",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == s)
    }
}

/// Element `index` (1-based) of the Halton sequence in `base`, in [0, 1).
pub fn halton(mut index: u32, base: u32) -> f32 {
    let mut f = 1.0f32;
    let mut r = 0.0f32;
    while index > 0 {
        f /= base as f32;
        r += f * (index % base) as f32;
        index /= base;
    }
    r
}

/// How many jitter phases to cycle for a render/output pair: `ceil(8 × (output/render)²)` on the
/// width (FSR's rule; DLSS asks for at least 8 per render-to-output pixel area, which is the same).
pub fn jitter_phases(render: Size, output: Size) -> u32 {
    let r = output.width as f32 / render.width as f32;
    (8.0 * r * r).ceil() as u32
}

/// The jitter for frame `frame` (0-based) in the layer's convention: Halton(2, 3) centred on the
/// pixel, render pixels, +y down.
pub fn jitter(frame: u32, phases: u32) -> [f32; 2] {
    let i = frame % phases.max(1) + 1;
    [halton(i, 2) - 0.5, halton(i, 3) - 0.5]
}

/// Everything an upscaler needs for one frame. `R` is the GPU resource type (`ID3D12Resource` in the
/// layer; `()` in unit tests of the checks).
pub struct Inputs<'a, R> {
    /// Render-size colour, the jittered frame. LDR or HDR (see `hdr`); linear.
    pub colour: &'a R,
    /// Render-size device depth (R32_FLOAT or a typeless/D32 depth resource).
    pub depth: &'a R,
    /// Render-size motion vectors, two channels (R16G16_FLOAT in the title).
    pub motion: &'a R,
    /// Optional 1x1 R32_FLOAT exposure. `None`: `pre_exposure` is used.
    pub exposure: Option<&'a R>,
    /// Output-size target, unordered-access capable.
    pub output: &'a R,
    /// Render size actually used this frame (the top-left sub-rectangle of the inputs).
    pub render: Size,
    /// Output size (the top-left sub-rectangle of `output`).
    pub output_size: Size,
    /// Sample offset from the pixel centre in render pixels, +y down. See the crate docs.
    pub jitter: [f32; 2],
    /// Multiplies the stored motion vector to give render pixels (previous − current), +y down.
    /// 1.0 when the buffer already holds render pixels; (W, H) when it holds UV units;
    /// (W/2, −H/2) when it holds clip-space (NDC) deltas.
    pub motion_scale: [f32; 2],
    /// True when near is 1.0 (reversed-Z).
    pub depth_reversed: bool,
    /// True on a cut: discard the history.
    pub reset: bool,
    /// True when `colour` is HDR (values above 1.0).
    pub hdr: bool,
    /// The factor the colour was already multiplied by (1.0 = none).
    pub pre_exposure: f32,
    /// Milliseconds since the previous frame (FSR uses it; DLSS ignores it).
    pub frame_ms: f32,
    /// Camera near and far plane distances and vertical field of view in radians.
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
}

/// Why a frame's inputs were refused before reaching an SDK.
#[derive(Debug, PartialEq)]
pub enum Refused {
    RenderLargerThanOutput,
    ZeroSize,
    JitterOutOfRange([f32; 2]),
    NotFinite(&'static str),
}

impl<'a, R> Inputs<'a, R> {
    /// The checks every backend relies on; an SDK given any of these silently produces garbage.
    pub fn check(&self) -> Result<(), Refused> {
        if self.render.width == 0 || self.render.height == 0 || self.output_size.width == 0 || self.output_size.height == 0 {
            return Err(Refused::ZeroSize);
        }
        if self.render.width > self.output_size.width || self.render.height > self.output_size.height {
            return Err(Refused::RenderLargerThanOutput);
        }
        for (name, v) in [
            ("jitter", self.jitter[0]),
            ("jitter", self.jitter[1]),
            ("motion_scale", self.motion_scale[0]),
            ("motion_scale", self.motion_scale[1]),
            ("pre_exposure", self.pre_exposure),
            ("frame_ms", self.frame_ms),
        ] {
            if !v.is_finite() {
                return Err(Refused::NotFinite(name));
            }
        }
        if self.jitter.iter().any(|j| j.abs() > 0.5) {
            return Err(Refused::JitterOutOfRange(self.jitter));
        }
        Ok(())
    }
}

/// A temporal upscaler backend. One instance per view; it keeps the history between frames.
pub trait Upscaler {
    type Resource;
    type CommandList;

    /// The backend's name and version, for the frame line (`upscaler/mode`).
    fn name(&self) -> String;

    /// The render size the SDK wants for this output size and mode.
    fn render_size(&self, output: Size, mode: Mode) -> Result<Size, String>;

    /// Record the upscale into `list`. All inputs must be in shader-resource state and the output in
    /// unordered-access state; they are left in those states. The list's pipeline, root signature
    /// and descriptor heaps are NOT preserved: the caller rebinds after this call.
    fn evaluate(&mut self, list: &Self::CommandList, mode: Mode, inputs: &Inputs<Self::Resource>) -> Result<(), String>;

    /// Override the backend's sign mapping (the sweep; normally the measured default stays).
    fn set_signs(&mut self, signs: Signs);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(r: &()) -> Inputs<'_, ()> {
        Inputs {
            colour: r,
            depth: r,
            motion: r,
            exposure: None,
            output: r,
            render: Size::new(1280, 720),
            output_size: Size::new(1920, 1080),
            jitter: [0.25, -0.25],
            motion_scale: [1.0, 1.0],
            depth_reversed: false,
            reset: false,
            hdr: false,
            pre_exposure: 1.0,
            frame_ms: 16.7,
            near: 0.1,
            far: 1000.0,
            fov_y: 1.0,
        }
    }

    #[test]
    fn halton_matches_the_published_sequence() {
        let b2: Vec<f32> = (1..=4).map(|i| halton(i, 2)).collect();
        let b3: Vec<f32> = (1..=4).map(|i| halton(i, 3)).collect();
        assert_eq!(b2, [0.5, 0.25, 0.75, 0.125]);
        let want3 = [1.0 / 3.0, 2.0 / 3.0, 1.0 / 9.0, 4.0 / 9.0];
        for (a, b) in b3.iter().zip(want3) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn jitter_stays_inside_the_pixel_and_averages_to_the_centre() {
        let phases = jitter_phases(Size::new(1280, 720), Size::new(1920, 1080));
        assert_eq!(phases, 18); // 8 × 1.5²
        let mut sum = [0.0f32; 2];
        for f in 0..phases {
            let j = jitter(f, phases);
            assert!(j[0].abs() <= 0.5 && j[1].abs() <= 0.5);
            sum[0] += j[0];
            sum[1] += j[1];
        }
        assert!((sum[0] / phases as f32).abs() < 0.05 && (sum[1] / phases as f32).abs() < 0.05, "{sum:?}");
        assert_eq!(jitter(0, phases), jitter(phases, phases));
    }

    #[test]
    fn render_sizes_per_mode() {
        let out = Size::new(2560, 1440);
        assert_eq!(Mode::NativeAa.render_size(out), out);
        assert_eq!(Mode::Quality.render_size(out), Size::new(1706, 960));
        assert_eq!(Mode::Performance.render_size(out), Size::new(1280, 720));
        assert_eq!(Mode::UltraPerformance.render_size(out), Size::new(853, 480));
        for m in Mode::ALL {
            assert_eq!(Mode::parse(m.name()), Some(m));
        }
    }

    #[test]
    fn checks_refuse_what_breaks_an_sdk() {
        let r = ();
        assert_eq!(inputs(&r).check(), Ok(()));
        let mut i = inputs(&r);
        i.render = Size::new(2000, 720);
        assert_eq!(i.check(), Err(Refused::RenderLargerThanOutput));
        let mut i = inputs(&r);
        i.jitter = [0.75, 0.0];
        assert_eq!(i.check(), Err(Refused::JitterOutOfRange([0.75, 0.0])));
        let mut i = inputs(&r);
        i.motion_scale = [f32::NAN, 1.0];
        assert_eq!(i.check(), Err(Refused::NotFinite("motion_scale")));
        let mut i = inputs(&r);
        i.output_size = Size::new(0, 1080);
        assert_eq!(i.check(), Err(Refused::ZeroSize));
    }
}
