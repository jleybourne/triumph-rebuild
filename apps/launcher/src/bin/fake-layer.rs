//! A stand-in for `rebuild.exe` in the launch tests: it takes `--config <path>` like the layer, reads
//! it with `rebuild_config`, and appends frame lines to `REBUILD_FRAME_LOG` (standard output when that
//! is unset, as a proof binary prints), one every 20 ms. No GPU.
//!
//! `--frames N` (default 5) sets how many lines; `--exit N` the exit code.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let config = arg("--config").expect("--config <path>");
    let frames: u32 = arg("--frames").and_then(|s| s.parse().ok()).unwrap_or(5);
    let code: i32 = arg("--exit").and_then(|s| s.parse().ok()).unwrap_or(0);

    let text = std::fs::read_to_string(&config).expect("read the config");
    let c = rebuild_config::Config::parse(&text).expect("parse the config");
    let mut out: Box<dyn Write> = match std::env::var_os("REBUILD_FRAME_LOG") {
        Some(log) => Box::new(std::fs::OpenOptions::new().create(true).append(true).open(log).expect("open the log")),
        None => Box::new(std::io::stdout()),
    };
    writeln!(out, "config profile={} problems={} width={}", c.profile, c.problems.len(), c.int("display.width")).unwrap();
    out.flush().unwrap();
    for i in 0..frames {
        std::thread::sleep(std::time::Duration::from_millis(20));
        writeln!(out, "frame {i} | fps 30.0 | ms 33.3 | draws 4500 | dropped 0").unwrap();
        out.flush().unwrap();
    }
    std::process::exit(code);
}
