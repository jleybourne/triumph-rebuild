//! Reading what a run measured: the frame line (one line a second in `frame-line.log`) and the run's
//! `summary.toml`. A field that isn't there stays `None` and the launcher shows "—": nothing is ever
//! estimated or filled in.
//!
//! The frame line is `|`-separated segments, each `<key> <number> [unit]`, in any order; unknown
//! segments are ignored. The keys read here:
//!
//! ```text
//! t 12s | fps 30.8 | ms 32.5 | gpu 21.0 ms | draws 4,512 | dropped 0 | upload 1.9 ms | xlate 2.4 ms
//!       | upscale 1.1 ms | wait 6.0 ms | 1707x960 -> 2560x1440 dlss quality
//! ```
//!
//! `ms` is the whole frame; `gpu`, `upload` (the upload ring), `xlate` (list translation), `upscale` and
//! `wait` (waiting on the GPU or pacing) are its parts. The resolution segment is `internal -> output`,
//! or just the output, followed by the upscaler.

use std::path::Path;

/// One frame line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    pub fps: Option<f64>,
    pub ms: Option<f64>,
    pub gpu_ms: Option<f64>,
    pub draws: Option<u64>,
    pub dropped: Option<u64>,
    pub upload_ms: Option<f64>,
    pub xlate_ms: Option<f64>,
    pub upscale_ms: Option<f64>,
    pub wait_ms: Option<f64>,
    pub internal: Option<(u32, u32)>,
    pub output: Option<(u32, u32)>,
    pub upscaler: Option<String>,
}

fn number(s: &str) -> Option<f64> {
    let t: String = s.chars().filter(|c| *c != ',').collect();
    let t = t.trim_end_matches("ms").trim_end_matches('s').trim_end_matches('%');
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn resolution(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(['x', 'X'])?;
    let w = a.parse().ok()?;
    let h = b.parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Parse one frame line; `None` when it carries none of the fields (a startup message).
pub fn parse(line: &str) -> Option<Frame> {
    let mut f = Frame::default();
    let mut any = false;
    for seg in line.split('|') {
        let words: Vec<&str> = seg.split(|c: char| c.is_whitespace() || c == '=').filter(|w| !w.is_empty()).collect();
        let Some(first) = words.first() else { continue };
        if let Some(r) = resolution(first) {
            any = true;
            let arrow = words.iter().position(|w| *w == "->" || *w == "→");
            match arrow.and_then(|i| words.get(i + 1)).and_then(|w| resolution(w)) {
                Some(out) => {
                    f.internal = Some(r);
                    f.output = Some(out);
                }
                None => f.output = Some(r),
            }
            let rest: Vec<&str> = words.iter().skip(arrow.map(|i| i + 2).unwrap_or(1)).copied().collect();
            if !rest.is_empty() {
                f.upscaler = Some(rest.join(" "));
            }
            continue;
        }
        let v = words.get(1).and_then(|w| number(w));
        let key = first.to_ascii_lowercase();
        let slot = match key.as_str() {
            "fps" => &mut f.fps,
            "ms" => &mut f.ms,
            "gpu" => &mut f.gpu_ms,
            "upload" => &mut f.upload_ms,
            "xlate" | "translate" | "lists" => &mut f.xlate_ms,
            "upscale" | "upscaler" => &mut f.upscale_ms,
            "wait" | "waiting" => &mut f.wait_ms,
            "draws" => {
                if let Some(v) = v {
                    f.draws = Some(v as u64);
                    any = true;
                }
                continue;
            }
            "dropped" => {
                if let Some(v) = v {
                    f.dropped = Some(v as u64);
                    any = true;
                }
                continue;
            }
            _ => continue,
        };
        if let Some(v) = v {
            *slot = Some(v);
            any = true;
        }
    }
    any.then_some(f)
}

/// The run's summary, as the layer writes it to `<session>/summary.toml`:
///
/// ```toml
/// [summary]
/// avg_fps = 30.1        # mean over the measured window
/// low1_fps = 27.0       # 1 % low
/// frame_ms = 33.2       # mean frame time
/// gpu_ms = 20.1
/// upload_ms = 1.9
/// frames = 1800
/// seconds = 60
/// measured_from = "after_loading"   # or "title_start"
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub avg_fps: Option<f64>,
    pub low1_fps: Option<f64>,
    pub frame_ms: Option<f64>,
    pub gpu_ms: Option<f64>,
    pub upload_ms: Option<f64>,
    pub frames: Option<u64>,
    pub seconds: Option<f64>,
    pub measured_from: Option<String>,
    /// true when this was read from the frame line, not from `summary.toml`
    pub from_frame_line: bool,
}

impl Summary {
    pub fn parse(text: &str) -> Option<Summary> {
        let doc: toml::Table = text.parse().ok()?;
        let s = doc.get("summary")?.as_table()?;
        let f = |k: &str| s.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))).filter(|v| v.is_finite());
        Some(Summary {
            avg_fps: f("avg_fps"),
            low1_fps: f("low1_fps"),
            frame_ms: f("frame_ms"),
            gpu_ms: f("gpu_ms"),
            upload_ms: f("upload_ms"),
            frames: s.get("frames").and_then(|v| v.as_integer()).map(|i| i.max(0) as u64),
            seconds: f("seconds"),
            measured_from: s.get("measured_from").and_then(|v| v.as_str()).map(String::from),
            from_frame_line: false,
        })
    }

    pub fn read(path: &Path) -> Option<Summary> {
        Summary::parse(&std::fs::read_to_string(path).ok()?)
    }

    /// Means of what the frame lines carry (fps, frame, GPU and upload ms), when there is no
    /// `summary.toml`. The 1 % low needs per-frame times, so it stays empty.
    pub fn from_lines<S: AsRef<str>>(lines: &[S]) -> Option<Summary> {
        let frames: Vec<Frame> = lines.iter().filter_map(|l| parse(l.as_ref())).collect();
        let mean = |get: &dyn Fn(&Frame) -> Option<f64>| {
            let v: Vec<f64> = frames.iter().filter_map(get).collect();
            (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
        };
        let s = Summary {
            avg_fps: mean(&|f| f.fps),
            frame_ms: mean(&|f| f.ms),
            gpu_ms: mean(&|f| f.gpu_ms),
            upload_ms: mean(&|f| f.upload_ms),
            seconds: (!frames.is_empty()).then_some(frames.len() as f64),
            from_frame_line: true,
            ..Default::default()
        };
        s.avg_fps.is_some().then_some(s)
    }
}

/// "30.1" or "—".
pub fn show(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(v) => format!("{v:.decimals$}"),
        None => "—".into(),
    }
}

/// How far an fps is from 30: "+0.4", "−22.4", or "—".
pub fn vs_target(fps: Option<f64>, target: f64) -> String {
    match fps {
        Some(f) if f - target >= 0.0 => format!("+{:.1}", f - target),
        Some(f) => format!("−{:.1}", target - f),
        None => "—".into(),
    }
}

/// One row of the test matrix table.
#[derive(Debug, Clone, PartialEq)]
pub struct MatrixRow {
    pub profile: String,
    pub session: String,
    pub summary: Option<Summary>,
}

/// The matrix table as CSV. Empty cells where nothing was measured.
pub fn matrix_csv(rows: &[MatrixRow], target: f64) -> String {
    let cell = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_default();
    let mut s = String::from("profile,session,avg_fps,low1_fps,frame_ms,gpu_ms,upload_ms,vs_target_fps,source\n");
    for r in rows {
        let m = r.summary.clone().unwrap_or_default();
        let vs = m.avg_fps.map(|f| format!("{:.2}", f - target)).unwrap_or_default();
        let source = match &r.summary {
            None => "",
            Some(s) if s.from_frame_line => "frame-line.log",
            Some(_) => "summary.toml",
        };
        s += &format!(
            "{},{},{},{},{},{},{},{},{}\n",
            r.profile.replace(',', " "),
            r.session.replace(',', " "),
            cell(m.avg_fps),
            cell(m.low1_fps),
            cell(m.frame_ms),
            cell(m.gpu_ms),
            cell(m.upload_ms),
            vs,
            source
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line the layer writes (crates/runtime/src/framelog.rs formats exactly this sample in its own test).
    #[test]
    fn the_layers_frame_line_parses() {
        let f = parse(include_str!("../../../crates/runtime/src/frameline.sample").trim_end()).unwrap();
        assert_eq!(f.fps, Some(30.8));
        assert_eq!(f.ms, Some(32.5));
        assert_eq!(f.gpu_ms, Some(21.0));
        assert_eq!(f.draws, Some(4512));
        assert_eq!(f.dropped, Some(0));
        assert_eq!(f.wait_ms, Some(6.0));
        assert_eq!((f.upload_ms, f.xlate_ms, f.upscale_ms), (None, None, None), "not measured yet: absent");
        assert_eq!(f.internal, Some((2560, 1440)));
        assert_eq!(f.output, Some((3840, 2160)));
        assert_eq!(f.upscaler, None);
    }

    #[test]
    fn a_full_frame_line_parses() {
        let f = parse("t  12s | fps 30.8 | ms 32.5 | gpu 21.0 ms | draws 4,512 | dropped 3 | upload 1.9 ms | xlate 2.4 ms | upscale 1.1 ms | wait 6.0 ms | 1707x960 -> 2560x1440 dlss quality").unwrap();
        assert_eq!(f.fps, Some(30.8));
        assert_eq!(f.ms, Some(32.5));
        assert_eq!(f.gpu_ms, Some(21.0));
        assert_eq!(f.draws, Some(4512));
        assert_eq!(f.dropped, Some(3));
        assert_eq!(f.upload_ms, Some(1.9));
        assert_eq!(f.xlate_ms, Some(2.4));
        assert_eq!(f.upscale_ms, Some(1.1));
        assert_eq!(f.wait_ms, Some(6.0));
        assert_eq!(f.internal, Some((1707, 960)));
        assert_eq!(f.output, Some((2560, 1440)));
        assert_eq!(f.upscaler.as_deref(), Some("dlss quality"));
    }

    #[test]
    fn missing_fields_stay_empty() {
        let f = parse("frame 3 | fps 30.0 | ms 33.3 | draws 4500 | dropped 0").unwrap();
        assert_eq!((f.fps, f.gpu_ms, f.upload_ms, f.internal, f.output), (Some(30.0), None, None, None, None));
        let f = parse("fps=7.6 ms=131.3 | 2560x1440").unwrap();
        assert_eq!(f.fps, Some(7.6));
        assert_eq!(f.output, Some((2560, 1440)));
        assert_eq!(f.internal, None);
        assert!(parse("config profile=x problems=0").is_none());
        assert!(parse("fps nan").is_none());
    }

    #[test]
    fn summary_parses_and_falls_back_to_the_frame_line() {
        let s = Summary::parse("[summary]\navg_fps = 30.1\nlow1_fps = 27\nframe_ms = 33.2\nframes = 1800\nmeasured_from = \"after_loading\"\n").unwrap();
        assert_eq!(s.avg_fps, Some(30.1));
        assert_eq!(s.low1_fps, Some(27.0));
        assert_eq!(s.gpu_ms, None);
        assert_eq!(s.frames, Some(1800));
        assert!(!s.from_frame_line);
        assert!(Summary::parse("nothing = 1").is_none());
        let f = Summary::from_lines(&["config ok", "fps 30.0 | ms 33.0", "fps 20.0 | ms 50.0 | upload 2.0 ms"]).unwrap();
        assert_eq!(f.avg_fps, Some(25.0));
        assert_eq!(f.frame_ms, Some(41.5));
        assert_eq!(f.upload_ms, Some(2.0));
        assert_eq!(f.low1_fps, None, "never estimated");
        assert!(f.from_frame_line);
        assert!(Summary::from_lines(&["config ok"]).is_none(), "no fps yet: nothing");
    }

    #[test]
    fn the_matrix_csv_leaves_unmeasured_cells_empty() {
        let rows = vec![
            MatrixRow { profile: "series-x".into(), session: "s1".into(), summary: Summary::parse("[summary]\navg_fps = 31.0\nlow1_fps = 28.5\n") },
            MatrixRow { profile: "low-pc".into(), session: "s2".into(), summary: None },
        ];
        let csv = matrix_csv(&rows, 30.0);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "profile,session,avg_fps,low1_fps,frame_ms,gpu_ms,upload_ms,vs_target_fps,source");
        assert_eq!(lines[1], "series-x,s1,31.00,28.50,,,,1.00,summary.toml");
        assert_eq!(lines[2], "low-pc,s2,,,,,,,");
        assert_eq!(show(None, 1), "—");
        assert_eq!(vs_target(Some(7.6), 30.0), "−22.4");
        assert_eq!(vs_target(Some(31.0), 30.0), "+1.0");
    }
}
