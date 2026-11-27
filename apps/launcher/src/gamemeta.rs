//! What the title's package says about itself, for the Play hero: its display name and its splash and
//! logo images, from `MicrosoftGame.config` in the game folder. Read-only, at run time; nothing about the
//! title is written in this code, and no image is ever copied.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Visuals {
    pub name: Option<String>,
    pub publisher: Option<String>,
    pub splash: Option<PathBuf>,
    pub logo: Option<PathBuf>,
}

/// The value of `attr="..."` inside the first `<tag ...>` element.
fn attr(text: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = text.match_indices(&open).map(|(i, _)| i).find(|&i| {
        text[i + open.len()..].starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>')
    })?;
    let end = start + text[start..].find('>')?;
    let el = &text[start..end];
    let key = format!(" {attr}=\"");
    let i = el.find(&key)? + key.len();
    let j = el[i..].find('"')?;
    let v = el[i..i + j].trim();
    (!v.is_empty()).then(|| v.replace("&amp;", "&").replace("&quot;", "\"").replace("&apos;", "'"))
}

/// Read `MicrosoftGame.config` (any case) in the game folder.
pub fn read(game_dir: &Path) -> Visuals {
    let Some(cfg) = std::fs::read_dir(game_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("MicrosoftGame.config"))
    else {
        return Visuals::default();
    };
    let Ok(text) = std::fs::read_to_string(cfg.path()) else { return Visuals::default() };
    parse(&text, game_dir)
}

pub fn parse(text: &str, game_dir: &Path) -> Visuals {
    let file = |a: &str| {
        attr(text, "ShellVisuals", a)
            .filter(|f| !f.contains("..") && Path::new(f).is_relative())
            .map(|f| game_dir.join(f))
            .filter(|p| p.is_file())
    };
    Visuals {
        name: attr(text, "ShellVisuals", "DefaultDisplayName"),
        publisher: attr(text, "ShellVisuals", "PublisherDisplayName"),
        splash: file("SplashScreenImage"),
        logo: file("Square480x480Logo").or_else(|| file("Square150x150Logo")),
    }
}

/// An RGBA8 image.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decode a PNG (8-bit grey, grey+alpha, RGB, RGBA or palette) to RGBA8. Files over 32 MB or larger than
/// 8192 px a side are refused.
pub fn load_png(path: &Path) -> Result<Image, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > 32 << 20 {
        return Err("image too large".into());
    }
    let mut dec = png::Decoder::new(std::fs::File::open(path).map_err(|e| e.to_string())?);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = dec.read_info().map_err(|e| e.to_string())?;
    let (w, h) = r.info().size();
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err(format!("{w}x{h}: unsupported size"));
    }
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let px = (w * h) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => buf[..px * 3].chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf[..px * 2].chunks(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => buf[..px].iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::Indexed => return Err("palette not expanded".into()),
    };
    Ok(Image { width: w, height: h, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_visuals_are_read_and_paths_stay_in_the_folder() {
        let d = crate::temp_dir("meta");
        std::fs::write(d.join("splash.png"), b"x").unwrap();
        std::fs::write(d.join("logo.png"), b"x").unwrap();
        let cfg = r#"<Game><ShellVisualsX DefaultDisplayName="wrong" /><ShellVisuals DefaultDisplayName="Sample &amp; Title" PublisherDisplayName="Pub" SplashScreenImage="splash.png" Square480x480Logo="..\logo.png" Square150x150Logo="logo.png" /></Game>"#;
        std::fs::write(d.join("microsoftgame.config"), cfg).unwrap();
        let v = read(&d);
        assert_eq!(v.name.as_deref(), Some("Sample & Title"));
        assert_eq!(v.publisher.as_deref(), Some("Pub"));
        assert_eq!(v.splash, Some(d.join("splash.png")));
        assert_eq!(v.logo, Some(d.join("logo.png")), "a path leaving the folder is ignored");
        assert_eq!(read(&d.join("missing")), Visuals::default());
    }

    #[test]
    fn png_decodes_to_rgba() {
        let d = crate::temp_dir("png");
        let p = d.join("a.png");
        let mut e = png::Encoder::new(std::fs::File::create(&p).unwrap(), 2, 1);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().unwrap().write_image_data(&[255, 0, 0, 0, 0, 255]).unwrap();
        let img = load_png(&p).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
        let before = crate::hash_file(&p).unwrap();
        let _ = load_png(&p);
        assert_eq!(crate::hash_file(&p).unwrap(), before);
    }
}
