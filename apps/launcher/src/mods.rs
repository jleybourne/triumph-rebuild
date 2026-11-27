//! U4: mods. A mod is a folder `mods/<name>/` whose files replace title files by relative path. The
//! order is the `[[mods]]` list in `rebuild.toml`; later mods win. Conflicts (one path provided by two
//! or more enabled mods) come from lane `vfs`'s `ModSet::conflicts`, case-insensitive like the console.

use rebuild_config::Mod;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub enabled: bool,
    /// the folder exists (a listed mod whose folder was removed stays listed, greyed out)
    pub present: bool,
}

/// The mod folders under `mods_dir`, sorted by name.
pub fn scan(mods_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(mods_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    v.sort_by_key(|s| s.to_lowercase());
    v
}

/// The list the tab shows: the configured order first, then new folders appended, disabled.
pub fn merge(configured: &[Mod], found: &[String]) -> Vec<Entry> {
    let mut out: Vec<Entry> = configured
        .iter()
        .map(|m| Entry { name: m.name.clone(), enabled: m.enabled, present: found.iter().any(|f| f.eq_ignore_ascii_case(&m.name)) })
        .collect();
    for f in found {
        if !out.iter().any(|e| e.name.eq_ignore_ascii_case(f)) {
            out.push(Entry { name: f.clone(), enabled: false, present: true });
        }
    }
    out
}

/// Back to the config's `[[mods]]` list (load order).
pub fn to_config(entries: &[Entry]) -> Vec<Mod> {
    entries.iter().map(|e| Mod { name: e.name.clone(), enabled: e.enabled }).collect()
}

pub fn move_up(entries: &mut [Entry], i: usize) {
    if i > 0 && i < entries.len() {
        entries.swap(i - 1, i);
    }
}

pub fn move_down(entries: &mut [Entry], i: usize) {
    if i + 1 < entries.len() {
        entries.swap(i, i + 1);
    }
}

pub use rebuild_vfs::Conflict;

/// The mods as lane `vfs`'s `ModSet`, in this list's order (present folders only).
pub fn mod_set(mods_dir: &Path, entries: &[Entry]) -> rebuild_vfs::ModSet {
    let found = rebuild_vfs::ModSet::discover(mods_dir).unwrap_or_default();
    let mods = entries
        .iter()
        .filter(|e| e.present)
        .filter_map(|e| {
            let m = found.mods.iter().find(|m| m.name.eq_ignore_ascii_case(&e.name))?;
            Some(rebuild_vfs::Mod { name: e.name.clone(), root: m.root.clone(), enabled: e.enabled })
        })
        .collect();
    rebuild_vfs::ModSet { mods }
}

/// Paths provided by more than one enabled, present mod (`rebuild_vfs::ModSet::conflicts`).
pub fn conflicts(mods_dir: &Path, entries: &[Entry]) -> Vec<Conflict> {
    mod_set(mods_dir, entries).conflicts().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &Path, m: &str, rel: &str) {
        let p = root.join(m).join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, m).unwrap();
    }

    fn setup() -> std::path::PathBuf {
        let d = crate::temp_dir("mods");
        put(&d, "hud", "ui/hud.xml");
        put(&d, "hud", "ui/fonts/a.ttf");
        put(&d, "fov", "camera/fov.xml");
        put(&d, "fov", "UI/HUD.xml"); // the same file, other case
        put(&d, "colors", "ui/hud.xml");
        put(&d, "colors", "grade.lut");
        d
    }

    #[test]
    fn scan_and_merge_keep_the_configured_order() {
        let d = setup();
        assert_eq!(scan(&d), vec!["colors", "fov", "hud"]);
        let cfg = vec![Mod { name: "hud".into(), enabled: true }, Mod { name: "gone".into(), enabled: true }];
        let e = merge(&cfg, &scan(&d));
        let names: Vec<_> = e.iter().map(|e| (e.name.as_str(), e.enabled, e.present)).collect();
        assert_eq!(names, vec![("hud", true, true), ("gone", true, false), ("colors", false, true), ("fov", false, true)]);
        assert_eq!(to_config(&e).len(), 4);
    }

    #[test]
    fn conflicts_are_case_insensitive_and_the_later_mod_wins() {
        let d = setup();
        let mut e = merge(&[], &scan(&d)); // colors, fov, hud: all disabled
        assert!(conflicts(&d, &e).is_empty(), "disabled mods never conflict");
        for x in e.iter_mut() {
            x.enabled = true;
        }
        let c = conflicts(&d, &e);
        assert_eq!(c.len(), 1, "{c:#?}");
        assert_eq!(c[0].path.to_lowercase(), "ui/hud.xml");
        assert_eq!(c[0].providers, vec!["colors", "fov", "hud"]);
        assert_eq!(c[0].winner, "hud");

        // reorder: hud to the top, so fov now wins
        move_up(&mut e, 2);
        move_up(&mut e, 1);
        assert_eq!(e[0].name, "hud");
        assert_eq!(conflicts(&d, &e)[0].winner, "fov");
        move_down(&mut e, 0);
        assert_eq!(e[1].name, "hud");

        // disable fov: two providers left, the later wins
        e.iter_mut().find(|x| x.name == "fov").unwrap().enabled = false;
        let c = conflicts(&d, &e);
        assert_eq!(c[0].providers.len(), 2);
        assert_eq!(c[0].winner, "hud");

        // disable hud: one provider, no conflict
        e.iter_mut().find(|x| x.name == "hud").unwrap().enabled = false;
        assert!(conflicts(&d, &e).is_empty());
    }

    #[test]
    fn a_missing_folder_is_never_walked() {
        let d = setup();
        let e = vec![Entry { name: "gone".into(), enabled: true, present: false }];
        assert!(conflicts(&d, &e).is_empty());
        move_up(&mut [], 0);
        move_down(&mut [], 0);
    }
}
