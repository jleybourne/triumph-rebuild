//! Session folder names, shared by the launcher (which makes the folder for each launch) and the layer
//! (which makes one itself when started without the launcher).
//!
//! `<sessions>/<UTC stamp>-<profile>`, e.g. `20261127T153012Z-series-x`, with `-2`, `-3`… if taken.

use std::path::{Path, PathBuf};

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Howard Hinnant's algorithm).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `20261127T153012Z`: the folder-name stamp (sorts by time, no characters a path refuses).
pub fn stamp(secs: u64) -> String {
    let (y, m, d) = civil((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

/// A profile name as a folder-name part: letters, digits, `-` and `_`; anything else becomes `-`.
pub fn folder_part(profile: &str) -> String {
    let s: String = profile.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(40).collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "default".into()
    } else {
        s
    }
}

/// Make a new session folder under `sessions` (created if absent) for `profile`, started at `now`.
pub fn new_dir(sessions: &Path, profile: &str, now: u64) -> Result<PathBuf, String> {
    std::fs::create_dir_all(sessions).map_err(|e| format!("{}: {e}", sessions.display()))?;
    let base = format!("{}-{}", stamp(now), folder_part(profile));
    let mut dir = sessions.join(&base);
    let mut n = 2;
    while dir.exists() {
        dir = sessions.join(format!("{base}-{n}"));
        n += 1;
    }
    std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_folder_never_reuses_a_taken_name() {
        let root = std::env::temp_dir().join(format!("rebuild-config-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let a = new_dir(&root, "series x", 0).unwrap();
        let b = new_dir(&root, "series x", 0).unwrap();
        assert_eq!(a.file_name().unwrap(), "19700101T000000Z-series-x");
        assert_eq!(b.file_name().unwrap(), "19700101T000000Z-series-x-2");
        let _ = std::fs::remove_dir_all(&root);
    }
}
