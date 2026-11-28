//! titlectl: the client of a run's live control channel (`REBUILD_INPUT_PIPE`, `crates/runtime/src/control.rs`),
//! and a driver that steps the title's menus by looking at each capture.
//!
//! ```text
//! titlectl <pipe> <command...>                 one command (ping | frame | press <what> | cap | exit); prints the answer
//! titlectl <pipe> run <steps> [--out DIR]      the driver: steps joined by commas (below); small copies of captures in DIR
//! titlectl small <png> <out.png> [width]       a smaller copy (box filter; default 1280 wide) for reading by eye
//! titlectl diff <a.png> <b.png>                the driver's screen distance between two captures
//! ```
//!
//! The pipe is waited for (`--wait S`, default 180 s: the title creates it with its input, ~5 s into a run).
//!
//! Driver steps (`<what>` is the input script's: `A`, `Down`, `RDown/3000ms`, `A+B/4f`):
//!
//! - `<what>`: press, then wait out the hold and 400 ms more;
//! - `<what>~` / `<what>~N`: press, then wait (N s at most, default 60) until the screen differs from the capture
//!   taken just before the press, and then holds still for two captures in a row;
//! - `~` / `~N`: the same wait with no press (a video, a loading screen);
//! - `settle` / `settleN`: wait until two captures in a row agree;
//! - `cap:<label>`: capture; a small copy goes to `DIR/<nn>-<label>.png` and its path is printed;
//! - `sleep:<s>`, `exit` (ends the run through the pipe).
//!
//! **Screen distance.** Each capture is reduced to a 160x90 grey thumbnail (box average); the distance is the share
//! of thumbnail cells whose grey moved by more than 12 of 255. A page change moves 10 %-90 %; a focus bar moving a
//! line, ~1 %; a ticked checkbox can be below the change threshold (0.3 %): tick boxes with plain presses and look.
//! Full-size captures the driver took only to compare are deleted; `cap:` keeps its capture.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TW: usize = 160;
const TH: usize = 90;
const CELL_DELTA: u8 = 12;
const CHANGED: f64 = 0.003;
const STILL: f64 = 0.002;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(argv) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("titlectl: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(mut argv: Vec<String>) -> Result<(), String> {
    let wait = take_opt(&mut argv, "--wait")?.map(|s| s.parse::<u64>().map_err(|_| format!("--wait {s}"))).transpose()?.unwrap_or(180);
    let out = take_opt(&mut argv, "--out")?.map(PathBuf::from);
    let usage = "titlectl <pipe> <command...> | <pipe> run <steps> [--out DIR] | small <png> <out> [width] | diff <a> <b>";
    match argv.first().map(String::as_str) {
        None => Err(usage.into()),
        Some("small") => {
            let (src, dst) = (argv.get(1).ok_or(usage)?, argv.get(2).ok_or(usage)?);
            let width = argv.get(3).map(|w| w.parse::<u32>().map_err(|_| format!("width {w}"))).transpose()?.unwrap_or(1280);
            small(Path::new(src), Path::new(dst), width)
        }
        Some("diff") => {
            let a = thumb(Path::new(argv.get(1).ok_or(usage)?))?;
            let b = thumb(Path::new(argv.get(2).ok_or(usage)?))?;
            println!("{:.4}", distance(&a, &b));
            Ok(())
        }
        Some(pipe) => {
            let mut c = Client::connect(pipe, Duration::from_secs(wait))?;
            match argv.get(1).map(String::as_str) {
                None => Err(usage.into()),
                Some("run") => {
                    let steps = argv.get(2).ok_or(usage)?;
                    let out = out.unwrap_or_else(|| PathBuf::from("titlectl-out"));
                    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
                    // numbering goes on from the copies already there (one folder over several invocations)
                    let n = std::fs::read_dir(&out).map_or(0, |d| d.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "png")).count() as u32);
                    Driver { c, out, n, last: None }.run(steps)
                }
                Some(_) => {
                    let answer = c.ask(&argv[1..].join(" "))?;
                    println!("{answer}");
                    if answer.starts_with("ok") {
                        Ok(())
                    } else {
                        Err(answer)
                    }
                }
            }
        }
    }
}

fn take_opt(argv: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(i) = argv.iter().position(|a| a == name) else { return Ok(None) };
    if i + 1 >= argv.len() {
        return Err(format!("{name} needs a value"));
    }
    let v = argv.remove(i + 1);
    argv.remove(i);
    Ok(Some(v))
}

struct Client {
    w: std::fs::File,
    r: BufReader<std::fs::File>,
}

impl Client {
    fn connect(pipe: &str, wait: Duration) -> Result<Client, String> {
        let name = if pipe.starts_with(r"\\.\pipe\") { pipe.to_string() } else { format!(r"\\.\pipe\{pipe}") };
        let t = Instant::now();
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
                Ok(f) => {
                    let w = f.try_clone().map_err(|e| e.to_string())?;
                    return Ok(Client { w, r: BufReader::new(f) });
                }
                Err(e) if t.elapsed() >= wait => return Err(format!("{name}: {e} (after {} s)", wait.as_secs())),
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    fn ask(&mut self, cmd: &str) -> Result<String, String> {
        self.w.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| format!("send '{cmd}': {e}"))?;
        let mut s = String::new();
        let n = self.r.read_line(&mut s).map_err(|e| format!("answer to '{cmd}': {e}"))?;
        if n == 0 {
            return Err(format!("the run closed the pipe at '{cmd}'"));
        }
        Ok(s.trim().to_string())
    }

    fn cap(&mut self) -> Result<PathBuf, String> {
        let a = self.ask("cap")?;
        a.strip_prefix("ok ").map(PathBuf::from).ok_or(a)
    }
}

struct Driver {
    c: Client,
    out: PathBuf,
    n: u32,
    /// The latest capture's thumbnail.
    last: Option<Vec<u8>>,
}

fn stamp(c: &mut Client) -> String {
    c.ask("frame").ok().and_then(|a| a.strip_prefix("ok ").map(str::to_string)).map_or_else(String::new, |s| {
        let mut it = s.split_whitespace();
        format!("[frame {} {} s] ", it.next().unwrap_or("?"), it.next().unwrap_or("?"))
    })
}

impl Driver {
    fn run(&mut self, steps: &str) -> Result<(), String> {
        for step in steps.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let at = stamp(&mut self.c);
            println!("{at}{step}");
            self.step(step)?;
        }
        Ok(())
    }

    /// A capture's thumbnail; the full-size file deleted unless `keep`.
    fn look(&mut self, keep: bool) -> Result<(Vec<u8>, PathBuf), String> {
        let p = self.c.cap()?;
        let t = thumb(&p)?;
        if !keep {
            let _ = std::fs::remove_file(&p);
        }
        self.last = Some(t.clone());
        Ok((t, p))
    }

    fn step(&mut self, step: &str) -> Result<(), String> {
        if step == "exit" {
            let _ = self.c.ask("exit");
            return Ok(());
        }
        if let Some(s) = step.strip_prefix("sleep:") {
            let s: f64 = s.parse().map_err(|_| format!("sleep '{s}'"))?;
            std::thread::sleep(Duration::from_secs_f64(s));
            return Ok(());
        }
        if let Some(label) = step.strip_prefix("cap:") {
            let (_, p) = self.look(true)?;
            self.n += 1;
            let dst = self.out.join(format!("{:02}-{label}.png", self.n));
            small(&p, &dst, 1280)?;
            println!("          {} (full: {})", dst.display(), p.display());
            return Ok(());
        }
        if let Some(n) = step.strip_prefix("settle") {
            let limit = secs(n, 30)?;
            return self.settle(limit).map(|_| ());
        }
        let (what, wait) = match step.split_once('~') {
            Some((w, n)) => (w, Some(secs(n, 60)?)),
            None => (step, None),
        };
        let before = match wait {
            Some(_) => Some(self.look(false)?.0),
            None => None,
        };
        let mut hold = Duration::ZERO;
        if !what.is_empty() {
            let a = self.c.ask(&format!("press {what}"))?;
            if !a.starts_with("ok") {
                return Err(format!("press {what}: {a}"));
            }
            hold = hold_of(what);
        }
        std::thread::sleep(hold + Duration::from_millis(400));
        let (Some(before), Some(limit)) = (before, wait) else { return Ok(()) };
        let t = Instant::now();
        loop {
            let (now, _) = self.look(false)?;
            let d = distance(&before, &now);
            if d > CHANGED {
                println!("          changed ({d:.3}) after {:.1} s", t.elapsed().as_secs_f64());
                let left = limit.saturating_sub(t.elapsed()).max(Duration::from_secs(5));
                self.settle(left)?;
                return Ok(());
            }
            if t.elapsed() > limit {
                println!("          no change within {} s (distance {d:.4}); going on", limit.as_secs());
                return Ok(());
            }
        }
    }

    fn settle(&mut self, limit: Duration) -> Result<bool, String> {
        let t = Instant::now();
        let mut prev = match self.last.clone() {
            Some(l) => l,
            None => self.look(false)?.0,
        };
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let (now, _) = self.look(false)?;
            let d = distance(&prev, &now);
            if d < STILL {
                return Ok(true);
            }
            if t.elapsed() > limit {
                println!("          not still within {} s (last distance {d:.4}); going on", limit.as_secs());
                return Ok(false);
            }
            prev = now;
        }
    }
}

fn secs(n: &str, default: u64) -> Result<Duration, String> {
    if n.is_empty() {
        return Ok(Duration::from_secs(default));
    }
    n.parse::<u64>().map(Duration::from_secs).map_err(|_| format!("seconds '{n}'"))
}

/// How long `<what>` holds its press (the input script's default is 200 ms; frames taken at 30 fps).
fn hold_of(what: &str) -> Duration {
    let Some((_, h)) = what.split_once('/') else { return Duration::from_millis(200) };
    let h = h.trim().to_ascii_lowercase();
    if let Some(ms) = h.strip_suffix("ms").and_then(|v| v.parse::<u64>().ok()) {
        Duration::from_millis(ms)
    } else if let Some(f) = h.strip_suffix('f').and_then(|v| v.parse::<u64>().ok()) {
        Duration::from_millis(f * 34)
    } else {
        Duration::from_millis(200)
    }
}

fn read_rgba(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut d = png::Decoder::new(BufReader::new(f));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
    let ch = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        _ => 1,
    };
    let mut rgba = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    for y in 0..info.height as usize {
        let row = &buf[y * info.line_size..];
        for x in 0..info.width as usize {
            let p = &row[x * ch..x * ch + ch];
            rgba.extend_from_slice(&match ch {
                4 => [p[0], p[1], p[2], 255],
                3 => [p[0], p[1], p[2], 255],
                _ => [p[0], p[0], p[0], 255],
            });
        }
    }
    Ok((info.width, info.height, rgba))
}

/// Box-average `rgba` (w x h) to `tw` x `th`.
fn shrink(w: u32, h: u32, rgba: &[u8], tw: usize, th: usize) -> Vec<[f32; 3]> {
    let (w, h) = (w as usize, h as usize);
    let mut sum = vec![[0f32; 3]; tw * th];
    let mut cnt = vec![0u32; tw * th];
    for y in 0..h {
        let ty = y * th / h;
        for x in 0..w {
            let tx = x * tw / w;
            let p = &rgba[(y * w + x) * 4..];
            let s = &mut sum[ty * tw + tx];
            s[0] += f32::from(p[0]);
            s[1] += f32::from(p[1]);
            s[2] += f32::from(p[2]);
            cnt[ty * tw + tx] += 1;
        }
    }
    sum.iter().zip(&cnt).map(|(s, &c)| if c == 0 { [0.0; 3] } else { [s[0] / c as f32, s[1] / c as f32, s[2] / c as f32] }).collect()
}

/// The capture's 160x90 grey thumbnail.
fn thumb(path: &Path) -> Result<Vec<u8>, String> {
    let (w, h, rgba) = read_rgba(path)?;
    Ok(grey(&shrink(w, h, &rgba, TW, TH)))
}

fn grey(px: &[[f32; 3]]) -> Vec<u8> {
    px.iter().map(|p| (0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]).round().clamp(0.0, 255.0) as u8).collect()
}

/// The share of thumbnail cells whose grey moved by more than [`CELL_DELTA`].
fn distance(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 1.0;
    }
    a.iter().zip(b).filter(|(x, y)| x.abs_diff(**y) > CELL_DELTA).count() as f64 / a.len() as f64
}

fn small(src: &Path, dst: &Path, width: u32) -> Result<(), String> {
    let (w, h, rgba) = read_rgba(src)?;
    let tw = width.min(w).max(1) as usize;
    let th = ((h as u64 * tw as u64) / w as u64).max(1) as usize;
    let px = shrink(w, h, &rgba, tw, th);
    let mut bytes = Vec::with_capacity(tw * th * 3);
    for p in &px {
        bytes.extend(p.iter().map(|v| v.round().clamp(0.0, 255.0) as u8));
    }
    let f = std::fs::File::create(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), tw as u32, th as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&f(x, y));
            }
        }
        let file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&px).unwrap();
    }

    #[test]
    fn a_page_change_is_far_a_small_mark_is_near_and_small_copies_keep_shape() {
        let dir = std::env::temp_dir().join(format!("titlectl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a.png"), dir.join("b.png"), dir.join("c.png"));
        // a page: dark with a light band; another page: band elsewhere; the first with a 12x12 mark
        write(&a, 640, 360, |_, y| if (100..140).contains(&y) { [200, 200, 200, 255] } else { [20, 20, 30, 255] });
        write(&b, 640, 360, |_, y| if (220..260).contains(&y) { [200, 200, 200, 255] } else { [20, 20, 30, 255] });
        write(&c, 640, 360, |x, y| if (100..140).contains(&y) || ((300..312).contains(&x) && (300..312).contains(&y)) { [200, 200, 200, 255] } else { [20, 20, 30, 255] });
        let (ta, tb, tc) = (thumb(&a).unwrap(), thumb(&b).unwrap(), thumb(&c).unwrap());
        assert_eq!(ta.len(), TW * TH);
        assert_eq!(distance(&ta, &ta), 0.0);
        assert!(distance(&ta, &tb) > 0.15, "{}", distance(&ta, &tb));
        let mark = distance(&ta, &tc);
        assert!(mark > 0.0 && mark < CHANGED, "{mark}");
        let s = dir.join("s.png");
        small(&a, &s, 320).unwrap();
        let (w, h, _) = read_rgba(&s).unwrap();
        assert_eq!((w, h), (320, 180));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn holds_and_options() {
        assert_eq!(hold_of("A"), Duration::from_millis(200));
        assert_eq!(hold_of("RDown/3000ms"), Duration::from_millis(3000));
        assert_eq!(hold_of("A/3f"), Duration::from_millis(102));
        let mut v: Vec<String> = ["p", "run", "A", "--out", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(take_opt(&mut v, "--out").unwrap().as_deref(), Some("d"));
        assert_eq!(v, ["p", "run", "A"]);
        assert!(take_opt(&mut vec!["--out".to_string()], "--out").is_err());
        assert_eq!(secs("", 60).unwrap(), Duration::from_secs(60));
        assert_eq!(secs("5", 60).unwrap(), Duration::from_secs(5));
    }
}
