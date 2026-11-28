//! `savedump <path>…`: every save container at each path (a `.gsc`, a folder of them, or the owner's
//! older build's `saves` folder), decoded: both headers, the title's load checks, warnings, a
//! byte-identical round trip of the blobs and of the body, and the body's fields. Read only.

use rebuild_gdk::areas::xgamesave::{file, Stored};
use rebuild_saves::body::{self, Body};
use rebuild_saves::store;
use rebuild_saves::triumph;
use std::path::Path;

fn show(c: &Stored, from: &str, fields: bool) {
    println!("== {} ({:?}, {} blobs) from {from}", c.name, c.display, c.blobs.len());
    let Some(s) = store::save_of(c) else {
        println!("   not a save container");
        return;
    };
    let s = match s {
        Ok(s) => s,
        Err(e) => {
            println!("   REFUSED by the parser: {e}");
            return;
        }
    };
    let h = &s.header;
    let i = &s.inner;
    println!("   header  v{} size {} data {:#010x} header {:#010x} stamp {:#x} +18 {} +1c {} +20 {} name {:?}", h.version, h.data_size, h.data_hash, h.header_hash, h.stamp, h.inner_154, h.inner_18, h.inner_1c, h.name);
    println!(
        "   asav    format {} kind {} word {} flags {:#x} hsize {:#x} +18 {} +1c {} stamp {:#x} name {:?} tag {:?} +154 {} +158 {:#x} +15c {} +164 {:#x} size {} hash {:#010x}",
        i.format, i.kind, i.kind_word, i.flags, i.header_size, i.value_18, i.value_1c, i.stamp, i.name, i.tag, i.value_154, i.value_158, i.value_15c, i.value_164, i.size, i.hash
    );
    let p = s.problems();
    println!("   load checks: {}", if p.is_empty() { "pass".to_string() } else { p.join("; ") });
    for w in s.warnings() {
        println!("   warning: {w}");
    }
    let hb = c.blobs.iter().find(|b| b.0 == rebuild_saves::format::HEADER_BLOB).unwrap();
    let db = c.blobs.iter().find(|b| b.0 == rebuild_saves::format::DATA_BLOB).unwrap();
    println!("   round trip: header {} data {}", s.header.to_bytes() == hb.1, s.data() == db.1);
    match body::decode(i.kind, &s.body) {
        Body::Profile(pr) => {
            println!("   body: profile, word {}, {} entries, round trip {}", pr.version, pr.entries.len(), pr.to_body().ok().as_deref() == Some(&s.body[..]));
            if fields {
                for (id, v) in &pr.entries {
                    println!("      {id:#06x} = {v} ({v:#x})");
                }
            }
        }
        Body::Settings(st) => {
            let named = st.leaves.iter().filter(|l| !l.name().split('.').any(|k| k.starts_with('#'))).count();
            println!("   body: settings JSON, {} values ({} fully named), round trip {}", st.leaves.len(), named, st.to_body() == s.body);
            if fields {
                for (k, l) in st.leaves.iter().enumerate() {
                    println!("      {} = {} ({:?}{})", l.name(), st.value(k), l.kind, if l.editable() { "" } else { ", read-only" });
                }
            }
        }
        Body::Raw => println!("   body: {} bytes, not decoded", s.body.len()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fields = args.iter().any(|a| a == "--fields");
    for a in args.iter().filter(|a| !a.starts_with("--")) {
        let p = Path::new(a);
        if p.is_file() {
            match std::fs::read(p).map_err(|e| e.to_string()).and_then(|b| file::decode(&b).map_err(|e| e.to_string())) {
                Ok(c) => show(&c, a, fields),
                Err(e) => println!("{a}: {e}"),
            }
            continue;
        }
        let ours = store::list(p);
        if !ours.is_empty() {
            for e in ours {
                match store::read(&e.path) {
                    Ok(c) => show(&c, &e.path.display().to_string(), fields),
                    Err(err) => println!("{err}"),
                }
            }
            continue;
        }
        match triumph::scan(p) {
            Ok(found) if !found.is_empty() => {
                for f in found {
                    show(&f.stored, &f.version.display().to_string(), fields);
                }
            }
            Ok(_) => println!("{a}: no containers"),
            Err(e) => println!("{a}: {e}"),
        }
    }
}
