//! Build 3: load the title through the Windows loader (prepared images) and run its startup, by hand.
//!
//! REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin runtitle            # load and bind only
//! REBUILD_GAME_DIR=<the title's folder> cargo run --release --bin runtitle -- --run   # and run its startup
//!
//! The run goes on past `D3D12XboxCreateDevice` (the first render call) until `--seconds N` (default 60).
//! `--sample N` samples the title's main thread every N seconds (opt-in). The launch itself is shared with
//! `rebuild.exe --config` (`../launch.rs`); this takes its settings from the environment only.

#[path = "../args.rs"]
mod args;
#[path = "../crash.rs"]
mod crash;
#[path = "../launch.rs"]
mod launch;
#[path = "../patch.rs"]
mod patch;
#[path = "../profile.rs"]
mod profile;
#[path = "../sampler.rs"]
mod sampler;
#[path = "../watch.rs"]
mod watch;

use std::path::PathBuf;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = match args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            println!("FAILED    {e} (runtitle [--run] [--seconds N] [--sample N])");
            std::process::exit(2);
        }
    };
    if a.config.is_some() {
        println!("FAILED    runtitle takes no settings file: run rebuild --config <rebuild.toml>");
        std::process::exit(2);
    }
    let Some(game) = std::env::var_os("REBUILD_GAME_DIR").map(PathBuf::from) else {
        println!("FAILED    set REBUILD_GAME_DIR to the title's folder (it is only read)");
        std::process::exit(1);
    };
    launch::run(launch::Options { game, run: a.run, seconds: Some(a.seconds.unwrap_or(60)), sample: a.sample, bcpack_tables: None });
}
