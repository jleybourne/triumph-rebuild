//! One decoded save as readable JSON, and back (the owner's "export a section, import it back").
//!
//! The export names every decoded field; the body after the ASAV header is hex, since its layout is not
//! decoded yet. The import reads against the same schema (every key required, unknown keys refused),
//! rebuilds both blobs, and then:
//!
//! - unsigned kinds: recomputes the checksums (the `checksums` object in the file is informational and
//!   ignored), and refuses a result the title would not load;
//! - signed kinds: refuses any change at all, since the signature cannot be remade.
//!
//! Exporting and importing with nothing edited gives back the same bytes (test).

use crate::format::{check_name, kind_word, Editable, Header, Inner, Save, INNER_SIZE, TAG_BYTES};
use crate::json::{f32_json, from_hex, hex32, hex64, to_hex, Fields, Json};

/// The export's `format` value.
pub const FORMAT: &str = "rebuild-save-section";
pub const VERSION: u32 = 1;

pub fn export(container: &str, s: &Save) -> String {
    let h = &s.header;
    let i = &s.inner;
    Json::obj(vec![
        ("format", Json::str(FORMAT)),
        ("version", Json::num(VERSION)),
        ("container", Json::str(container)),
        (
            "note",
            Json::str(if i.signed() {
                "Signed save: read-only. Any change here is refused on import."
            } else {
                "Edit the fields or body_hex, then import: checksums are recomputed. The body's layout is not decoded yet."
            }),
        ),
        (
            "header",
            Json::obj(vec![
                ("version", Json::num(h.version)),
                ("stamp", hex64(h.stamp)),
                ("inner_154", Json::num(h.inner_154)),
                ("inner_18", Json::num(h.inner_18)),
                ("inner_1c", Json::num(h.inner_1c)),
                ("name", Json::str(&h.name)),
            ]),
        ),
        (
            "asav",
            Json::obj(vec![
                ("format", Json::num(i.format)),
                ("kind", Json::num(i.kind)),
                ("kind_word", Json::num(i.kind_word)),
                ("flags", hex32(i.flags)),
                ("header_size", Json::num(i.header_size)),
                ("value_18", Json::num(i.value_18)),
                ("value_1c", Json::num(i.value_1c)),
                ("stamp", hex64(i.stamp)),
                ("name", Json::str(&i.name)),
                ("tag", Json::str(&i.tag)),
                ("value_154", Json::num(i.value_154)),
                ("value_158", f32_json(i.value_158)),
                ("value_15c", Json::num(i.value_15c)),
                ("value_164", f32_json(i.value_164)),
                ("size", Json::num(i.size)),
            ]),
        ),
        ("body_hex", Json::str(to_hex(&s.body))),
        (
            "checksums",
            Json::obj(vec![
                ("data_size", Json::num(h.data_size)),
                ("data_hash", hex32(h.data_hash)),
                ("header_hash", hex32(h.header_hash)),
                ("asav_hash", hex32(i.hash)),
            ]),
        ),
    ])
    .pretty()
}

/// The save `text` describes, ready to write in place of `current` (the container's save now), for
/// container `container`. Refused with a reason, never half applied.
pub fn import(text: &str, container: &str, current: &Save) -> Result<Save, String> {
    let mut f = Fields::of(Json::parse(text)?, "section")?;
    if f.string("format")? != FORMAT {
        return Err(format!("not a save section export (format must be {FORMAT:?})"));
    }
    let v = f.u32("version")?;
    if v != VERSION {
        return Err(format!("section export version {v}, this editor reads {VERSION}"));
    }
    let named = f.string("container")?;
    if named != container {
        return Err(format!("this export is of container {named:?}, not {container:?}"));
    }
    f.string("note")?;
    let mut hf = f.object("header")?;
    let mut af = f.object("asav")?;
    let body = from_hex(&f.string("body_hex")?)?;
    let mut cf = f.object("checksums")?;
    let sums = (cf.u32("data_size")?, cf.hex32("data_hash")?, cf.hex32("header_hash")?, cf.hex32("asav_hash")?);
    cf.finish()?;
    f.finish()?;

    let header = Header {
        version: hf.u32("version")?,
        stamp: hf.hex64("stamp")?,
        inner_154: hf.u32("inner_154")?,
        inner_18: hf.u32("inner_18")?,
        inner_1c: hf.u32("inner_1c")?,
        name: hf.string("name")?,
        data_size: sums.0,
        data_hash: sums.1,
        header_hash: sums.2,
    };
    hf.finish()?;
    let inner = Inner {
        format: af.u32("format")?,
        kind: af.u32("kind")?,
        kind_word: af.u32("kind_word")?,
        flags: af.hex32("flags")?,
        header_size: af.u32("header_size")?,
        value_18: af.u32("value_18")?,
        value_1c: af.u32("value_1c")?,
        stamp: af.hex64("stamp")?,
        name: af.string("name")?,
        tag: af.string("tag")?,
        value_154: af.u32("value_154")?,
        value_158: af.f32_bits("value_158")?,
        value_15c: af.u32("value_15c")?,
        value_164: af.f32_bits("value_164")?,
        size: af.u32("size")?,
        hash: sums.3,
    };
    af.finish()?;

    check_name(&header.name).map_err(|e| format!("header.name: {e}"))?;
    check_name(&inner.name).map_err(|e| format!("asav.name: {e}"))?;
    if inner.tag.len() >= TAG_BYTES || inner.tag.contains('\0') {
        return Err("asav.tag: at most 42 bytes, no NUL".into());
    }
    if kind_word(inner.kind).is_none() {
        return Err(format!("asav.kind {} is not one the title knows (1-9)", inner.kind));
    }
    if body.len() != current.body.len() {
        return Err(format!("body_hex is {} bytes; the save's body is {} (the editor never grows or shrinks a save)", body.len(), current.body.len()));
    }
    if inner.kind != current.inner.kind {
        return Err("asav.kind cannot change".into());
    }

    let mut s = Save { header, inner, body };
    if current.inner.signed() {
        if s != *current {
            return Err("this save is signed: the title would refuse any change, so none is imported".into());
        }
        return Ok(s);
    }
    if let Editable::No(why) = current.editable() {
        return Err(why);
    }
    s.reseal().map_err(|e| e.0)?;
    let p = s.problems();
    if !p.is_empty() {
        return Err(format!("the title would not load the result: {}", p.join("; ")));
    }
    debug_assert_eq!(s.data().len(), INNER_SIZE + current.body.len());
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{build, joaat};

    fn sample() -> Save {
        let mut s = build(5, "Profile Settings", 0x11_2233_4455, "v1.0", &(0..96u8).collect::<Vec<_>>()).unwrap();
        s.inner.value_158 = 0.1f32.to_bits();
        s.inner.value_164 = f32::NAN.to_bits();
        s.reseal().unwrap();
        s
    }

    #[test]
    fn export_then_import_is_byte_identical() {
        let s = sample();
        let text = export("SettingsProfileSettings", &s);
        let back = import(&text, "SettingsProfileSettings", &s).unwrap();
        assert_eq!(back.header.to_bytes(), s.header.to_bytes());
        assert_eq!(back.data(), s.data());
    }

    #[test]
    fn an_edit_is_resealed() {
        let s = sample();
        let text = export("C", &s).replace("\"name\": \"Profile Settings\"", "\"name\": \"Mine\"");
        let back = import(&text, "C", &s).unwrap();
        assert_eq!(back.header.name, "Mine");
        assert_eq!(back.inner.name, "Mine");
        assert!(back.problems().is_empty());
        assert_ne!(back.header.header_hash, s.header.header_hash);
        let body = to_hex(&s.body);
        let mut edited = body.clone();
        edited.replace_range(0..2, "ff");
        let back = import(&export("C", &s).replace(&body, &edited), "C", &s).unwrap();
        assert_eq!(back.body[0], 0xff);
        assert_eq!(back.header.data_hash, joaat(&back.data(), 0));
    }

    #[test]
    fn tampered_or_invalid_imports_are_refused() {
        let s = sample();
        let text = export("C", &s);
        let cases = [
            text.replace("\"kind\": 5", "\"kind\": 12"),
            text.replace("\"format\": \"rebuild-save-section\"", "\"format\": \"x\""),
            text.replace("\"version\": 1,\n  \"container\"", "\"version\": 2,\n  \"container\""),
            text.replace("\"header\": {", "\"header\": {\n    \"extra\": 1,"),
            text.replace("\"body_hex\"", "\"unknown\": 0,\n  \"body_hex\""),
            text.replace(&to_hex(&s.body), &to_hex(&s.body[1..])),
            text.replace("\"tag\": \"v1.0\"", &format!("\"tag\": \"{}\"", "x".repeat(43))),
            text.replace("\"name\": \"Profile Settings\"", &format!("\"name\": \"{}\"", "y".repeat(128))),
            text.replace("\"kind_word\": 1", "\"kind_word\": 2"),
            text[..text.len() / 2].to_string(),
        ];
        for (n, t) in cases.iter().enumerate() {
            assert!(import(t, "C", &s).is_err(), "case {n} was accepted");
        }
        assert!(import(&text, "Other", &s).is_err());
    }

    #[test]
    fn signed_saves_import_only_unchanged() {
        let mut s = sample();
        s.inner.kind = 9;
        s.inner.kind_word = 1;
        s.inner.hash = s.inner.compute_hash();
        s.header.data_hash = joaat(&s.data(), 0);
        s.header.header_hash = s.header.compute_hash();
        let text = export("C", &s);
        assert_eq!(import(&text, "C", &s).unwrap(), s);
        let edited = text.replace("\"name\": \"Profile Settings\"", "\"name\": \"Mine\"");
        assert!(import(&edited, "C", &s).is_err());
    }
}
