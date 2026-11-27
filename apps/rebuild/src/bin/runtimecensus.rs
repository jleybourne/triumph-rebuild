//! Build 3: of the imports the title needs (`agents/loader-imports.csv`, from lane `loader`), how many
//! bind straight to the host, how many we override, and how many are console-only.
//!
//! cargo run --release --bin runtimecensus [-- path/to/loader-imports.csv]

use std::collections::BTreeMap;

use rebuild_runtime::host::Host;
use rebuild_runtime::exports::console_export;
use rebuild_runtime::modules::override_export;
use rebuild_runtime::{classify, override_reason, Tier};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "agents/loader-imports.csv".into());
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            println!("FAILED    {path}: {e}");
            std::process::exit(1);
        }
    };
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
    let col = |n: &str| header.iter().position(|h| *h == n).unwrap_or_else(|| panic!("no column {n}"));
    let (cd, cf, cr) = (col("dll"), col("function"), col("resolved_by"));

    let mut host = Host::default();
    let mut tiers: BTreeMap<Tier, usize> = BTreeMap::new();
    let mut by_dll: BTreeMap<String, BTreeMap<Tier, usize>> = BTreeMap::new();
    let mut overrides: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut console_functions = Vec::new();
    let mut title_supplied = 0usize;
    let mut built = 0usize;
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() <= cr {
            continue;
        }
        if f[cr] != "missing" {
            title_supplied += 1;
            continue;
        }
        let (dll, func) = (f[cd], f[cf]);
        let (name, ordinal) = match func.strip_prefix('#') {
            Some(o) => (None, o.parse::<u16>().ok()),
            None => (Some(func), None),
        };
        let has_dll = host.dll(dll).is_some();
        let has_fn = host.function(dll, name, ordinal).is_some();
        let tier = classify(func, has_dll, has_fn);
        *tiers.entry(tier).or_default() += 1;
        *by_dll.entry(dll.to_ascii_lowercase()).or_default().entry(tier).or_default() += 1;
        let ours_built = match tier {
            Tier::ConsoleDll | Tier::ConsoleFunction => name.and_then(|n| console_export(dll, n)).is_some(),
            Tier::Override => name.and_then(override_export).is_some(),
            Tier::Host => false,
        };
        if ours_built {
            built += 1;
        }
        if tier == Tier::Override {
            *overrides.entry(override_reason(func).unwrap()).or_default() += 1;
        }
        if tier == Tier::ConsoleFunction {
            console_functions.push(format!("{dll}!{func}"));
        }
    }
    let total: usize = tiers.values().sum();
    let pct = |n: usize| 100.0 * n as f64 / total.max(1) as f64;
    println!("runtimecensus — Build 3: what the title's {total} missing imports bind to ({title_supplied} more are supplied by the title's own DLLs)");
    println!();
    for (t, n) in &tiers {
        let what = match t {
            Tier::Host => "host: bound straight to the host's own function, no wrapper",
            Tier::Override => "override: the host has it, the meaning differs here (ours)",
            Tier::ConsoleFunction => "console function: the host DLL exists, the function does not (ours)",
            Tier::ConsoleDll => "console DLL: the whole DLL is console-only (ours)",
        };
        println!("{n:>5}  {:>5.1} %  {what}", pct(*n));
    }
    let ours = total - tiers.get(&Tier::Host).copied().unwrap_or(0);
    println!();
    println!("ours, built: {built} of {ours} (bound to our implementations; the rest are still discovery entries)");
    println!();
    println!("overrides by reason: {}", overrides.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" · "));
    println!();
    println!("console-only DLLs (functions):");
    for (dll, t) in &by_dll {
        if let Some(n) = t.get(&Tier::ConsoleDll) {
            println!("  {n:>4}  {dll}");
        }
    }
    if !console_functions.is_empty() {
        println!();
        println!("console functions in host DLLs ({}):", console_functions.len());
        for f in &console_functions {
            println!("        {f}");
        }
    }
}
