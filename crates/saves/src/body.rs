//! The bodies after the ASAV header, for the two kinds real saves use (`docs/research/save-format.md` §4):
//!
//! - kind 3, the player profile (container named after the gamertag): `count`, a word (3 in every save
//!   seen), `joaat(entries)`, then `count` pairs of (id, value) u32s, zero to the end of the body;
//! - kind 2, `SettingsProfileSettings`: JSON text filling the body exactly, keys `"$0x<joaat of the
//!   member name>"` plus `"$v"` (a section's version), `"$x"`/`"$y"` (a vector), `"$data"` (a hashed
//!   string).
//!
//! Both are edited without changing the body's length: the title loads a save into a buffer sized by its
//! caller (`0x1428d3310` takes the size, `0x1428d3564` refuses a bigger save), so the editor never grows
//! one. A JSON value that gets shorter is padded with spaces after it; one that gets longer takes the room
//! from its own line's indentation, or is refused.

use crate::format::{joaat, Rejected};
use crate::json::Json;

fn no(s: impl Into<String>) -> Rejected {
    Rejected(s.into())
}

// The player profile (kind 3).

/// The profile body: entries of (id, value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The body's second word: 3 in every save seen.
    pub version: u32,
    pub entries: Vec<(u32, u32)>,
    /// The body's length, kept.
    pub len: usize,
}

impl Profile {
    /// Strict: the count must fit, the hash must match its entries, and everything after them be zero.
    pub fn parse(body: &[u8]) -> Result<Profile, Rejected> {
        if body.len() < 12 {
            return Err(no("profile body shorter than its 12-byte head"));
        }
        let w = |i: usize| u32::from_le_bytes(body[i..i + 4].try_into().unwrap());
        let count = w(0) as usize;
        let end = count.checked_mul(8).and_then(|n| n.checked_add(12)).filter(|&e| e <= body.len()).ok_or_else(|| no("profile count runs past the body"))?;
        let pairs = &body[12..end];
        if joaat(pairs, 0) != w(8) {
            return Err(no(format!("profile entries hash {:#010x}, the head says {:#010x}", joaat(pairs, 0), w(8))));
        }
        if body[end..].iter().any(|&b| b != 0) {
            return Err(no("profile: bytes after the entries"));
        }
        let entries = (0..count).map(|i| (w(12 + i * 8), w(16 + i * 8))).collect();
        Ok(Profile { version: w(4), entries, len: body.len() })
    }

    pub fn to_body(&self) -> Result<Vec<u8>, Rejected> {
        let need = 12 + self.entries.len() * 8;
        if need > self.len {
            return Err(no("profile entries no longer fit the body"));
        }
        let mut pairs = Vec::with_capacity(self.entries.len() * 8);
        for (id, v) in &self.entries {
            pairs.extend_from_slice(&id.to_le_bytes());
            pairs.extend_from_slice(&v.to_le_bytes());
        }
        let mut b = Vec::with_capacity(self.len);
        b.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.version.to_le_bytes());
        b.extend_from_slice(&joaat(&pairs, 0).to_le_bytes());
        b.extend_from_slice(&pairs);
        b.resize(self.len, 0);
        Ok(b)
    }

    /// Sets the value of entry `id` (entries are never added or removed).
    pub fn set(&mut self, id: u32, value: u32) -> Result<(), Rejected> {
        let e = self.entries.iter_mut().find(|e| e.0 == id).ok_or_else(|| no(format!("no profile entry {id:#x}")))?;
        e.1 = value;
        Ok(())
    }
}

// The settings profile (kind 2): JSON.

/// Member names whose joaat is a key in a real `SettingsProfileSettings` (each checked by its hash; a
/// 32-bit match of a meaningful word is the evidence: see the research notes for where each came from).
pub const NAMES: &[(u32, &str)] = &[
    (0x0459_f938, "tips"),
    (0x112b_5803, "versionMinor"),
    (0x112c_08e2, "gpsAudio"),
    (0x1141_d443, "versionMajor"),
    (0x125b_4637, "ammo"),
    (0x1423_d321, "invertMove"),
    (0x17d8_2a47, "display"),
    (0x1abb_5826, "objective"),
    (0x1acd_b89c, "vehiclePromptMode"),
    (0x2202_fbe1, "autoLevel"),
    (0x2368_3a0d, "controls"),
    (0x2558_4db6, "camera"),
    (0x2af4_b66c, "colorblindMode"),
    (0x3300_c23e, "musicLevel"),
    (0x3301_e662, "vibration"),
    (0x33bc_3e71, "subtitlesBackground"),
    (0x36d6_0978, "reticleMessages"),
    (0x36f4_10b7, "uiSfxLevel"),
    (0x4906_91e6, "cameraBob"),
    (0x4b62_d3b2, "language"),
    (0x525f_bf2c, "radarSize"),
    (0x541e_3249, "tickerFeed"),
    (0x5577_59b4, "invertLook"),
    (0x55c1_89fe, "lookAcceleration"),
    (0x6a51_0681, "fov"),
    (0x6b34_fe60, "general"),
    (0x74ae_cfec, "help"),
    (0x7595_3d64, "radarMode"),
    (0x7db4_a7fa, "altimeter"),
    (0x8f12_d59c, "thirdPerson"),
    (0x92d0_8faf, "radarBlipSize"),
    (0x944f_a9d9, "hud"),
    (0x9c00_2b5e, "cinematic"),
    (0x9c7d_4584, "vehiclefov"),
    (0xa49d_0765, "money"),
    (0xa534_1017, "targeting"),
    (0xb80c_6ae8, "audio"),
    (0xbf1a_0b77, "aimMode"),
    (0xbfa8_9537, "sharpenIntensity"),
    (0xc63c_7f4b, "dialogueLevel"),
    (0xc7fb_ada1, "compassMode"),
    (0xc899_52df, "subtitles"),
    (0xcb20_b040, "accessibility"),
    (0xd10b_92fb, "aimReticleOpacity"),
    (0xd7cb_96b5, "prompts"),
    (0xe72b_7dcd, "holdAssist"),
    (0xe76c_da01, "firstPerson"),
    (0xee19_0b4e, "sfxLevel"),
    (0xf1a2_e99b, "gps"),
    (0xf664_2366, "invertMap"),
    (0xf6c5_b953, "mapBlipSize"),
    (0xf971_3d90, "aimAcceleration"),
    (0xfb38_7d8b, "controlPresets"),
];

/// The readable form of a key: its name if known, `#<hash>` if not, the literal for `$v`, `$x`, ….
pub fn key_name(key: &str) -> String {
    match key.strip_prefix("$0x").and_then(|h| u32::from_str_radix(h, 16).ok()) {
        Some(h) => NAMES.iter().find(|n| n.0 == h).map_or_else(|| format!("#{h:08x}"), |n| n.1.to_string()),
        None => key.to_string(),
    }
}

/// A value's JSON type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int,
    Float,
    Str,
}

/// One value in the settings: where it is, what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// The raw keys from the root.
    pub path: Vec<String>,
    pub kind: Kind,
    /// Byte range of the value's text in the body.
    pub at: std::ops::Range<usize>,
}

impl Leaf {
    /// `controls.thirdPerson.lookAcceleration`, unknown members as `#hash`.
    pub fn name(&self) -> String {
        self.path.iter().map(|k| key_name(k)).collect::<Vec<_>>().join(".")
    }

    /// Whether the editor offers it: section versions and the hashed language string stay as they are.
    pub fn editable(&self) -> bool {
        let last = self.path.last().map(String::as_str).unwrap_or("");
        !(matches!(last, "$v" | "$data" | "$0x1141d443" | "$0x112b5803") || self.kind == Kind::Str)
    }
}

/// The settings body: its text and every value in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub text: String,
    pub leaves: Vec<Leaf>,
}

struct Scan<'a> {
    b: &'a [u8],
    at: usize,
    leaves: Vec<Leaf>,
}

impl Scan<'_> {
    fn ws(&mut self) {
        while self.at < self.b.len() && matches!(self.b[self.at], b' ' | b'\t' | b'\r' | b'\n') {
            self.at += 1;
        }
    }

    fn key(&mut self) -> Result<String, Rejected> {
        if self.b.get(self.at) != Some(&b'"') {
            return Err(no(format!("settings: expected a key at {}", self.at)));
        }
        let start = self.at + 1;
        let end = start + self.b[start..].iter().position(|&c| c == b'"').ok_or_else(|| no("settings: unterminated key"))?;
        let k = std::str::from_utf8(&self.b[start..end]).map_err(|_| no("settings: key not UTF-8"))?;
        if k.contains('\\') {
            return Err(no("settings: escaped key"));
        }
        self.at = end + 1;
        Ok(k.to_string())
    }

    fn value(&mut self, path: &mut Vec<String>) -> Result<(), Rejected> {
        self.ws();
        let start = self.at;
        match self.b.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                self.ws();
                if self.b.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    let k = self.key()?;
                    self.ws();
                    if self.b.get(self.at) != Some(&b':') {
                        return Err(no("settings: expected :"));
                    }
                    self.at += 1;
                    path.push(k);
                    self.value(path)?;
                    path.pop();
                    self.ws();
                    match self.b.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(());
                        }
                        _ => return Err(no("settings: expected , or }")),
                    }
                }
            }
            Some(b'[') => Err(no("settings: arrays are not in the format seen")),
            Some(b'"') => {
                self.key()?;
                self.leaves.push(Leaf { path: path.clone(), kind: Kind::Str, at: start..self.at });
                Ok(())
            }
            Some(b't') | Some(b'f') => {
                let lit: &[u8] = if self.b[self.at] == b't' { b"true" } else { b"false" };
                if !self.b[self.at..].starts_with(lit) {
                    return Err(no("settings: bad literal"));
                }
                self.at += lit.len();
                self.leaves.push(Leaf { path: path.clone(), kind: Kind::Bool, at: start..self.at });
                Ok(())
            }
            Some(c) if c.is_ascii_digit() || *c == b'-' => {
                while self.at < self.b.len() && matches!(self.b[self.at], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                    self.at += 1;
                }
                let t = &self.b[start..self.at];
                let kind = if t.iter().any(|c| matches!(c, b'.' | b'e' | b'E')) { Kind::Float } else { Kind::Int };
                self.leaves.push(Leaf { path: path.clone(), kind, at: start..self.at });
                Ok(())
            }
            _ => Err(no(format!("settings: unexpected byte at {}", self.at))),
        }
    }
}

impl Settings {
    /// The body as settings: strict JSON (no NUL, nothing after it) with one object at the top.
    pub fn parse(body: &[u8]) -> Result<Settings, Rejected> {
        let text = std::str::from_utf8(body).map_err(|_| no("settings body is not UTF-8 text"))?;
        Json::parse(text).map_err(|e| no(format!("settings body is not JSON: {e}")))?;
        let mut s = Scan { b: body, at: 0, leaves: Vec::new() };
        s.ws();
        if s.b.get(s.at) != Some(&b'{') {
            return Err(no("settings body is not a JSON object"));
        }
        s.value(&mut Vec::new())?;
        s.ws();
        if s.at != body.len() {
            return Err(no("settings: bytes after the object"));
        }
        Ok(Settings { text: text.to_string(), leaves: s.leaves })
    }

    pub fn value(&self, i: usize) -> &str {
        &self.text[self.leaves[i].at.clone()]
    }

    /// The leaf at `name` (as [`Leaf::name`] spells it).
    pub fn find(&self, name: &str) -> Option<usize> {
        self.leaves.iter().position(|l| l.name() == name)
    }

    /// Sets leaf `i` from `input` (checked against its type; a float keeps its digits after the point),
    /// keeping the body's length.
    pub fn set(&mut self, i: usize, input: &str) -> Result<(), Rejected> {
        let leaf = self.leaves.get(i).cloned().ok_or_else(|| no("no such setting"))?;
        if !leaf.editable() {
            return Err(no(format!("{} is not editable", leaf.name())));
        }
        let input = input.trim();
        let new = match leaf.kind {
            Kind::Bool => match input {
                "true" | "false" => input.to_string(),
                _ => return Err(no(format!("{}: true or false", leaf.name()))),
            },
            Kind::Int => input.parse::<i64>().map(|v| v.to_string()).map_err(|_| no(format!("{}: a whole number", leaf.name())))?,
            Kind::Float => {
                let v: f64 = input.parse().map_err(|_| no(format!("{}: a number", leaf.name())))?;
                if !v.is_finite() {
                    return Err(no(format!("{}: a finite number", leaf.name())));
                }
                let old = self.value(i);
                let digits = old.split_once('.').map_or(0, |(_, d)| d.len());
                format!("{v:.digits$}")
            }
            Kind::Str => return Err(no("text values are not editable")),
        };
        let old_len = leaf.at.len();
        let mut text = self.text.clone();
        if new.len() <= old_len {
            let padded = format!("{new}{}", " ".repeat(old_len - new.len()));
            text.replace_range(leaf.at.clone(), &padded);
        } else {
            // take the room from the line's indentation (tabs before the key)
            let extra = new.len() - old_len;
            let line = text[..leaf.at.start].rfind('\n').map_or(0, |n| n + 1);
            let tabs = text[line..].bytes().take_while(|&c| c == b'\t').count();
            if tabs < extra {
                return Err(no(format!("{}: no room for a value {extra} bytes longer", leaf.name())));
            }
            text.replace_range(leaf.at.clone(), &new);
            text.replace_range(line..line + extra, "");
        }
        let back = Settings::parse(text.as_bytes())?;
        let same_shape = back.leaves.len() == self.leaves.len() && back.leaves.iter().zip(&self.leaves).all(|(a, b)| a.path == b.path && a.kind == b.kind);
        if !same_shape || back.text.len() != self.text.len() || back.value(i) != new {
            return Err(no("the edit would change more than the value"));
        }
        *self = back;
        Ok(())
    }

    pub fn to_body(&self) -> Vec<u8> {
        self.text.as_bytes().to_vec()
    }
}

/// What a save's body is, as far as this editor decodes it.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Profile(Profile),
    Settings(Settings),
    /// Not decoded (another kind, or not in the form seen).
    Raw,
}

/// The decoder for a save of `kind`.
pub fn decode(kind: u32, body: &[u8]) -> Body {
    match kind {
        3 => Profile::parse(body).map_or(Body::Raw, Body::Profile),
        2 => Settings::parse(body).map_or(Body::Raw, Body::Settings),
        _ => Body::Raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile body built as a real one is laid out (synthetic values).
    pub fn profile_body() -> Vec<u8> {
        let entries = [(0xbeu32, 0x1cu32), (0xc8, 0xc00), (0x4b0, 1), (0x461, 3)];
        let p = Profile { version: 3, entries: entries.to_vec(), len: 0x66c };
        p.to_body().unwrap()
    }

    /// A settings body in the real one's style: tabs, CRLF, `$v` versions, floats with six decimals.
    pub fn settings_text() -> String {
        let t = "{\r\n\t\"$0x8c1c3948\": {\r\n\t\t\"$0x1141d443\": 6,\r\n\t\t\"$0x112b5803\": 0,\r\n\t\t\"$0xb80c6ae8\": {\r\n\t\t\t\"$v\": 2.0,\r\n\t\t\t\"$0x3300c23e\": 10,\r\n\t\t\t\"$0xc89952df\": false\r\n\t\t},\r\n\t\t\"$0x17d82a47\": {\r\n\t\t\t\"$0xbfa89537\": 1.000000,\r\n\t\t\t\"$0xf11f9fe5\": {\r\n\t\t\t\t\"$x\": 1.000000,\r\n\t\t\t\t\"$y\": 1.000000\r\n\t\t\t}\r\n\t\t},\r\n\t\t\"$0x6b34fe60\": {\r\n\t\t\t\"$0x4b62d3b2\": {\r\n\t\t\t\t\"$data\": \"0x08659ed8\"\r\n\t\t\t}\r\n\t\t}\r\n\t}\r\n}";
        t.to_string()
    }

    #[test]
    fn profile_round_trip_and_edit() {
        let b = profile_body();
        let p = Profile::parse(&b).unwrap();
        assert_eq!(p.entries.len(), 4);
        assert_eq!(p.to_body().unwrap(), b);
        let mut q = p.clone();
        q.set(0x461, 7).unwrap();
        let nb = q.to_body().unwrap();
        assert_eq!(nb.len(), b.len());
        assert_eq!(Profile::parse(&nb).unwrap().entries[3], (0x461, 7));
        assert!(q.set(0x999, 1).is_err());
        let mut bad = b.clone();
        bad[20] ^= 1;
        assert!(Profile::parse(&bad).is_err());
        let mut tail = b.clone();
        tail[0x600] = 1;
        assert!(Profile::parse(&tail).is_err());
    }

    #[test]
    fn settings_names_and_values() {
        let s = Settings::parse(settings_text().as_bytes()).unwrap();
        let names: Vec<String> = s.leaves.iter().map(Leaf::name).collect();
        assert!(names.contains(&"#8c1c3948.audio.musicLevel".to_string()), "{names:?}");
        let i = s.find("#8c1c3948.display.sharpenIntensity").unwrap();
        assert_eq!((s.leaves[i].kind, s.value(i)), (Kind::Float, "1.000000"));
        assert!(!s.leaves[s.find("#8c1c3948.audio.$v").unwrap()].editable());
        assert!(!s.leaves[s.find("#8c1c3948.versionMajor").unwrap()].editable());
        assert!(!s.leaves[s.find("#8c1c3948.general.language.$data").unwrap()].editable());
        assert_eq!(s.to_body(), settings_text().as_bytes());
    }

    #[test]
    fn settings_edits_keep_the_length() {
        let base = Settings::parse(settings_text().as_bytes()).unwrap();
        let n = base.text.len();
        let mut s = base.clone();
        let m = s.find("#8c1c3948.audio.musicLevel").unwrap();
        s.set(m, "7").unwrap();
        assert_eq!((s.value(m), s.text.len()), ("7", n));
        s.set(m, "100").unwrap();
        assert_eq!((s.value(m), s.text.len()), ("100", n));
        let b = s.find("#8c1c3948.audio.subtitles").unwrap();
        s.set(b, "true").unwrap();
        assert_eq!(s.value(b), "true");
        s.set(b, "false").unwrap();
        let f = s.find("#8c1c3948.display.sharpenIntensity").unwrap();
        s.set(f, "0.5").unwrap();
        assert_eq!(s.value(f), "0.500000");
        assert_eq!(s.text.len(), n);
        // every value parses back as the JSON the title reads
        assert!(Json::parse(&s.text).is_ok());
        // refusals
        assert!(s.set(b, "yes").is_err());
        assert!(s.set(m, "1.5").is_err());
        assert!(s.set(f, "inf").is_err());
        assert!(s.set(s.find("#8c1c3948.audio.$v").unwrap(), "3.0").is_err());
        // a value too long for its line's indentation
        assert!(s.set(m, "1000000").is_err());
        assert_eq!(s.text.len(), n);
    }

    #[test]
    fn settings_parser_is_strict() {
        assert!(Settings::parse(b"{\"a\": [1]}").is_err());
        assert!(Settings::parse(b"{\"a\": 1}\0").is_err());
        assert!(Settings::parse(b"{\"a\": 1} {").is_err());
        assert!(Settings::parse(b"[1]").is_err());
        assert!(matches!(decode(2, settings_text().as_bytes()), Body::Settings(_)));
        assert!(matches!(decode(3, &profile_body()), Body::Profile(_)));
        assert!(matches!(decode(4, &profile_body()), Body::Raw));
    }
}
