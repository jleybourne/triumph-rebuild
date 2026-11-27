//! `rebuild.exe`, the layer the launcher starts with `--config <rebuild.toml>`.
//!
//! Not built yet (C2): the title runner is the `runtitle` binary, started by hand with its guards. This reads
//! and checks the settings it is given, reports them, and exits with 3 ("the layer does not run the title yet"),
//! so a launch shows as not started instead of passing silently. 2 = no `--config`, or a file that does not parse.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.iter().position(|a| a == "--config").and_then(|i| args.get(i + 1)) else {
        eprintln!("rebuild: usage: rebuild --config <rebuild.toml>");
        std::process::exit(2);
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rebuild: {path}: {e}");
            std::process::exit(2);
        }
    };
    let config = match rebuild_config::Config::parse(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rebuild: {path}: {e}");
            std::process::exit(2);
        }
    };
    for p in &config.problems {
        eprintln!("rebuild: {path}: {p}");
    }
    println!(
        "rebuild: settings from {path}: profile {}, game {}, {}x{}, upscaler {} {}",
        config.profile,
        config.get("game.dir"),
        config.int("display.width"),
        config.int("display.height"),
        config.choice("upscaler.backend"),
        config.upscaler_mode()
    );
    eprintln!("rebuild: the layer does not run the title yet (C2); runtitle is the runner");
    std::process::exit(3);
}
