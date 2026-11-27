//! `rebuild.exe`, the layer the launcher starts: `rebuild --config <rebuild.toml> [--seconds N] [--sample N]`.
//!
//! C2: the handoff is the settings file itself. The layer reads it, validates it against the registry, and
//! sets the `REBUILD_*` variables the runtime reads (`rebuild_config::runtime`: a key the file sets wins
//! over a variable it was started with; a default does not replace one), before anything of the runtime
//! starts. Then it starts the title as `runtitle` does (`launch.rs`, shared), with no deadline unless
//! `--seconds` is given.
//!
//! The session folder is `REBUILD_SESSION_DIR` (the launcher makes one per launch); started without it,
//! the layer makes `<the file's folder>/sessions/<stamp>-<profile>` itself, so the crash report and the
//! summary always have a place.
//!
//! Exit codes: 2 = bad arguments, or a settings file that does not parse; 1 = no game folder, or the title
//! did not load; otherwise the run's own (0 when it ends normally).

mod args;
mod crash;
mod launch;
mod patch;
mod sampler;
mod watch;

use std::path::{Path, PathBuf};

use rebuild_config::Config;

const USAGE: &str = "rebuild --config <rebuild.toml> [--seconds N] [--sample N]";

/// The settings file: a file that is not there is all defaults (as the launcher treats it); one that does
/// not read or parse is an error.
fn settings(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("settings  {} does not exist: every setting at its default", path.display());
            Config::parse("")
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Where the sessions go for a run started without the launcher: beside the settings file.
fn sessions_beside(config: &Path) -> PathBuf {
    config.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("sessions")
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = match args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rebuild: {e}\nusage: {USAGE}");
            std::process::exit(2);
        }
    };
    let Some(path) = a.config else {
        eprintln!("rebuild: no settings file\nusage: {USAGE}");
        std::process::exit(2);
    };
    let config = match settings(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rebuild: {e}");
            std::process::exit(2);
        }
    };
    for p in &config.problems {
        println!("settings  {}: {p}", path.display());
    }

    // the settings, as the runtime reads them: set in this process before any of it starts
    let entries = config.runtime_env();
    let changes = rebuild_config::runtime::resolve(&entries, |v| std::env::var(v).ok());
    for (var, value) in &changes {
        match value {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }
    println!("settings  {} (profile {}): {} variables from the file", path.display(), config.profile, changes.len());
    for e in &entries {
        let applied = changes.iter().any(|(v, _)| *v == e.var);
        println!("          {}{}", rebuild_config::runtime::describe(e), if applied { "" } else { "  [the environment's value kept]" });
    }

    if std::env::var_os("REBUILD_SESSION_DIR").is_none() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        match rebuild_config::session::new_dir(&sessions_beside(&path), &config.profile, now) {
            Ok(dir) => std::env::set_var("REBUILD_SESSION_DIR", &dir),
            Err(e) => println!("session   none: {e} (the crash report goes to the console only)"),
        }
    }
    if let Some(dir) = std::env::var_os("REBUILD_SESSION_DIR") {
        println!("session   {}", Path::new(&dir).display());
    }

    let Some(game) = std::env::var_os("REBUILD_GAME_DIR").filter(|g| !g.is_empty()).map(PathBuf::from) else {
        launch::fail(1, "no game folder: set game.dir (Game folder) in the settings, or REBUILD_GAME_DIR");
    };
    launch::run(launch::Options { game, run: true, seconds: a.seconds, sample: a.sample, bcpack_tables: config.bcpack_tables() });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_settings_file_is_all_defaults_and_a_broken_one_is_refused() {
        let d = std::env::temp_dir().join(format!("rebuild-layer-settings-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let c = settings(&d.join("absent.toml")).unwrap();
        assert_eq!(c.changed().count(), 0);
        let bad = d.join("bad.toml");
        std::fs::write(&bad, "[settings\n").unwrap();
        assert!(settings(&bad).is_err());
        let good = d.join("rebuild.toml");
        std::fs::write(&good, "profile = \"p\"\n[settings]\n\"diagnostics.warp\" = true\n").unwrap();
        let c = settings(&good).unwrap();
        assert!(c.bool("diagnostics.warp"));
        assert_eq!(c.profile, "p");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sessions_go_beside_the_settings_file() {
        assert_eq!(sessions_beside(Path::new(r"C:\home\rebuild.toml")), PathBuf::from(r"C:\home\sessions"));
        assert_eq!(sessions_beside(Path::new("rebuild.toml")), PathBuf::from(r".\sessions"));
    }
}
