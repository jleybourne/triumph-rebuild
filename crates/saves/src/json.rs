//! A small strict JSON reader and writer for the export files: objects keep their key order, numbers
//! keep their text (so a float or a large integer reads back exactly), duplicate keys are refused, and
//! nothing follows the top value. No dependency, and nothing it does not need.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The number's text, as written.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }
    pub fn num(n: impl std::fmt::Display) -> Json {
        Json::Num(n.to_string())
    }

    /// Pretty text, two-space indent, one trailing newline.
    pub fn pretty(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, 0);
        s.push('\n');
        s
    }

    fn write(&self, s: &mut String, depth: usize) {
        let pad = |s: &mut String, d: usize| s.extend(std::iter::repeat_n("  ", d));
        match self {
            Json::Null => s.push_str("null"),
            Json::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => s.push_str(n),
            Json::Str(t) => quote(s, t),
            Json::Arr(a) if a.is_empty() => s.push_str("[]"),
            Json::Arr(a) => {
                s.push_str("[\n");
                for (i, v) in a.iter().enumerate() {
                    pad(s, depth + 1);
                    v.write(s, depth + 1);
                    s.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                pad(s, depth);
                s.push(']');
            }
            Json::Obj(o) if o.is_empty() => s.push_str("{}"),
            Json::Obj(o) => {
                s.push_str("{\n");
                for (i, (k, v)) in o.iter().enumerate() {
                    pad(s, depth + 1);
                    quote(s, k);
                    s.push_str(": ");
                    v.write(s, depth + 1);
                    s.push_str(if i + 1 < o.len() { ",\n" } else { "\n" });
                }
                pad(s, depth);
                s.push('}');
            }
        }
    }

    pub fn parse(text: &str) -> Result<Json, String> {
        let mut p = Parser { b: text.as_bytes(), at: 0 };
        let v = p.value(0)?;
        p.ws();
        if p.at != p.b.len() {
            return Err(format!("text after the JSON value at byte {}", p.at));
        }
        Ok(v)
    }
}

fn quote(s: &mut String, t: &str) {
    s.push('"');
    for c in t.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(s, "\\u{:04x}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.at < self.b.len() && matches!(self.b[self.at], b' ' | b'\t' | b'\n' | b'\r') {
            self.at += 1;
        }
    }

    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at byte {}", self.at))
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.b[self.at..].starts_with(lit.as_bytes()) {
            self.at += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 32 {
            return self.err("nested too deep");
        }
        self.ws();
        let Some(&c) = self.b.get(self.at) else { return self.err("unexpected end") };
        match c {
            b'n' if self.eat("null") => Ok(Json::Null),
            b't' if self.eat("true") => Ok(Json::Bool(true)),
            b'f' if self.eat("false") => Ok(Json::Bool(false)),
            b'"' => Ok(Json::Str(self.string()?)),
            b'[' => {
                self.at += 1;
                let mut a = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat("]") {
                        return Ok(Json::Arr(a));
                    }
                    if !self.eat(",") {
                        return self.err("expected , or ]");
                    }
                }
            }
            b'{' => {
                self.at += 1;
                let mut o: Vec<(String, Json)> = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Ok(Json::Obj(o));
                }
                loop {
                    self.ws();
                    if self.b.get(self.at) != Some(&b'"') {
                        return self.err("expected a key");
                    }
                    let k = self.string()?;
                    if o.iter().any(|(e, _)| *e == k) {
                        return Err(format!("key {k:?} given twice"));
                    }
                    self.ws();
                    if !self.eat(":") {
                        return self.err("expected :");
                    }
                    let v = self.value(depth + 1)?;
                    o.push((k, v));
                    self.ws();
                    if self.eat("}") {
                        return Ok(Json::Obj(o));
                    }
                    if !self.eat(",") {
                        return self.err("expected , or }");
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = self.at;
                self.at += 1;
                while self.at < self.b.len() && matches!(self.b[self.at], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    self.at += 1;
                }
                let t = std::str::from_utf8(&self.b[start..self.at]).unwrap();
                if t.parse::<f64>().is_err() {
                    return Err(format!("bad number {t:?}"));
                }
                Ok(Json::Num(t.to_string()))
            }
            _ => self.err("unexpected character"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.at) else { return self.err("unterminated string") };
            match c {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    let Some(&e) = self.b.get(self.at + 1) else { return self.err("bad escape") };
                    self.at += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xd800..0xdc00).contains(&hi) {
                                if !self.eat("\\u") {
                                    return self.err("lone surrogate");
                                }
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return self.err("bad surrogate pair");
                                }
                                char::from_u32(0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00))
                            } else {
                                char::from_u32(hi)
                            };
                            out.push(ch.ok_or_else(|| format!("bad \\u escape at byte {}", self.at))?);
                        }
                        _ => return self.err("bad escape"),
                    }
                }
                c if c < 0x20 => return self.err("control character in a string"),
                _ => {
                    // one UTF-8 character (the input is a &str, so it is well formed)
                    let rest = std::str::from_utf8(&self.b[self.at..]).map_err(|_| "not UTF-8".to_string())?;
                    let ch = rest.chars().next().unwrap();
                    out.push(ch);
                    self.at += ch.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let t = self.b.get(self.at..self.at + 4).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u32::from_str_radix(h, 16).ok());
        match t {
            Some(v) => {
                self.at += 4;
                Ok(v)
            }
            None => self.err("bad \\u escape"),
        }
    }
}

/// Reading an object against a schema: every key must be taken exactly once; `finish` refuses any left.
pub struct Fields {
    what: String,
    pairs: Vec<(String, Json)>,
}

impl Fields {
    pub fn of(v: Json, what: &str) -> Result<Fields, String> {
        match v {
            Json::Obj(pairs) => Ok(Fields { what: what.into(), pairs }),
            _ => Err(format!("{what}: expected an object")),
        }
    }

    pub fn take(&mut self, key: &str) -> Result<Json, String> {
        let i = self.pairs.iter().position(|(k, _)| k == key).ok_or_else(|| format!("{}: missing {key:?}", self.what))?;
        Ok(self.pairs.remove(i).1)
    }

    pub fn string(&mut self, key: &str) -> Result<String, String> {
        match self.take(key)? {
            Json::Str(s) => Ok(s),
            _ => Err(format!("{}.{key}: expected a string", self.what)),
        }
    }

    pub fn u32(&mut self, key: &str) -> Result<u32, String> {
        match self.take(key)? {
            Json::Num(n) => n.parse::<u32>().map_err(|_| format!("{}.{key}: expected an integer 0-4294967295, got {n}", self.what)),
            _ => Err(format!("{}.{key}: expected a number", self.what)),
        }
    }

    pub fn i64(&mut self, key: &str) -> Result<i64, String> {
        match self.take(key)? {
            Json::Num(n) => n.parse::<i64>().map_err(|_| format!("{}.{key}: expected an integer, got {n}", self.what)),
            _ => Err(format!("{}.{key}: expected a number", self.what)),
        }
    }

    /// A value written as `"0x…"` hex text (64-bit stamps, hashes): exact, unlike a JSON number.
    pub fn hex64(&mut self, key: &str) -> Result<u64, String> {
        let s = self.string(key)?;
        s.strip_prefix("0x").and_then(|h| u64::from_str_radix(h, 16).ok()).ok_or_else(|| format!("{}.{key}: expected 0x-prefixed hex, got {s:?}", self.what))
    }

    pub fn hex32(&mut self, key: &str) -> Result<u32, String> {
        let v = self.hex64(key)?;
        u32::try_from(v).map_err(|_| format!("{}.{key}: more than 32 bits", self.what))
    }

    /// A 32-bit float as a JSON number (read with f32's own parser, so it comes back to the same bits),
    /// or `"bits:0x…"` for one JSON cannot write (infinities, NaNs).
    pub fn f32_bits(&mut self, key: &str) -> Result<u32, String> {
        match self.take(key)? {
            Json::Num(n) => n.parse::<f32>().map(f32::to_bits).map_err(|_| format!("{}.{key}: bad float {n}", self.what)),
            Json::Str(s) => s
                .strip_prefix("bits:0x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("{}.{key}: expected a number or \"bits:0x…\"", self.what)),
            _ => Err(format!("{}.{key}: expected a number", self.what)),
        }
    }

    pub fn object(&mut self, key: &str) -> Result<Fields, String> {
        let what = format!("{}.{key}", self.what);
        Fields::of(self.take(key)?, &what)
    }

    pub fn array(&mut self, key: &str) -> Result<Vec<Json>, String> {
        match self.take(key)? {
            Json::Arr(a) => Ok(a),
            _ => Err(format!("{}.{key}: expected an array", self.what)),
        }
    }

    /// Refuses any key the schema did not take.
    pub fn finish(self) -> Result<(), String> {
        match self.pairs.first() {
            None => Ok(()),
            Some((k, _)) => Err(format!("{}: unknown field {k:?}", self.what)),
        }
    }
}

/// A float's JSON form: its shortest round-trip text, or `"bits:0x…"`.
pub fn f32_json(bits: u32) -> Json {
    let f = f32::from_bits(bits);
    if f.is_finite() {
        // Rust prints the shortest text that parses back to the same f32 ("1", "0.1", "-0", "1e-45")
        Json::Num(format!("{f:?}"))
    } else {
        Json::Str(format!("bits:{bits:#010x}"))
    }
}

pub fn hex32(v: u32) -> Json {
    Json::Str(format!("{v:#010x}"))
}

pub fn hex64(v: u64) -> Json {
    Json::Str(format!("{v:#018x}"))
}

pub fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

pub fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if s.len() % 2 != 0 {
        return Err("hex: odd number of digits".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("hex: bad digits {:?}", &s[i..i + 2])))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let v = Json::obj(vec![
            ("a", Json::num(1)),
            ("b", Json::str("x \"y\" \\ \n ✓ \u{1}")),
            ("c", Json::Arr(vec![Json::Bool(true), Json::Null, Json::num(-2.5)])),
            ("d", Json::Obj(vec![])),
        ]);
        assert_eq!(Json::parse(&v.pretty()).unwrap(), v);
    }

    #[test]
    fn strict() {
        assert!(Json::parse(r#"{"a":1,"a":2}"#).is_err());
        assert!(Json::parse(r#"{"a":1} x"#).is_err());
        assert!(Json::parse(r#"{"a":01x}"#).is_err());
        assert!(Json::parse("\"\\ud800\"").is_err());
        assert_eq!(Json::parse("\"\\ud83d\\ude00\"").unwrap(), Json::str("😀"));
        let mut f = Fields::of(Json::parse(r#"{"a":1,"b":2}"#).unwrap(), "t").unwrap();
        assert_eq!(f.u32("a").unwrap(), 1);
        assert!(f.finish().unwrap_err().contains("unknown field \"b\""));
    }

    #[test]
    fn floats_keep_their_bits() {
        for bits in [0u32, 0x8000_0000, 0x3dcc_cccd, 0x0000_0001, 0x7f7f_ffff, 0x7fc0_0000, 0xff80_0000, 0x4049_0fdb] {
            let j = Json::obj(vec![("f", f32_json(bits))]);
            let mut f = Fields::of(Json::parse(&j.pretty()).unwrap(), "t").unwrap();
            assert_eq!(f.f32_bits("f").unwrap(), bits, "{bits:#x}");
        }
    }

    #[test]
    fn hex() {
        assert_eq!(from_hex(&to_hex(&[0, 1, 0xab, 0xff])).unwrap(), vec![0, 1, 0xab, 0xff]);
        assert!(from_hex("abc").is_err());
        assert!(from_hex("zz").is_err());
    }
}
