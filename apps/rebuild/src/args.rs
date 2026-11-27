//! The arguments of `rebuild.exe` and `runtitle`.
//!
//! ```text
//! rebuild  --config <rebuild.toml> [--seconds N] [--sample N]
//! runtitle [--run] [--seconds N] [--sample N]
//! ```

use std::path::PathBuf;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    pub config: Option<PathBuf>,
    /// runtitle: run the title's startup (without it, load and bind only)
    pub run: bool,
    /// stop the run after this many seconds (a watchdog ends the process 15 s later)
    pub seconds: Option<u64>,
    /// sample the title's main thread every N seconds (it suspends a thread: opt-in only)
    pub sample: Option<u64>,
}

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, name: &str) -> Result<String, String> {
    it.next().cloned().ok_or_else(|| format!("{name} needs a value"))
}

fn number<'a>(it: &mut impl Iterator<Item = &'a String>, name: &str) -> Result<u64, String> {
    let v = value(it, name)?;
    v.parse().map_err(|_| format!("{name} {v}: not a whole number"))
}

pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => a.config = Some(PathBuf::from(value(&mut it, "--config")?)),
            "--run" => a.run = true,
            "--seconds" => a.seconds = Some(number(&mut it, "--seconds")?),
            "--sample" => a.sample = Some(number(&mut it, "--sample")?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn the_layer_and_runtitle_arguments_parse() {
        assert_eq!(p("").unwrap(), Args::default());
        let a = p(r"--config C:\home\rebuild.toml --seconds 90").unwrap();
        assert_eq!(a.config, Some(PathBuf::from(r"C:\home\rebuild.toml")));
        assert_eq!(a.seconds, Some(90));
        assert!(!a.run);
        let r = p("--run --sample 2").unwrap();
        assert!(r.run);
        assert_eq!(r.sample, Some(2));
        assert_eq!(r.seconds, None);
    }

    #[test]
    fn bad_arguments_are_refused_with_a_reason() {
        assert_eq!(p("--config").unwrap_err(), "--config needs a value");
        assert_eq!(p("--seconds soon").unwrap_err(), "--seconds soon: not a whole number");
        assert_eq!(p("--fast").unwrap_err(), "unknown argument --fast");
    }
}
