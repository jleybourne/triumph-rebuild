//! Every icon the launcher draws, from Phosphor (MIT, `egui-phosphor`). Only these can be used
//! (`use ui::icons as icon`), and a test checks each has a glyph in the embedded font.

#![allow(dead_code)]

use egui_phosphor::regular as p;

pub const ARROWS_CLOCKWISE: &str = p::ARROWS_CLOCKWISE;
pub const ARROW_COUNTER_CLOCKWISE: &str = p::ARROW_COUNTER_CLOCKWISE;
pub const CARET_DOUBLE_LEFT: &str = p::CARET_DOUBLE_LEFT;
pub const CARET_UP: &str = p::CARET_UP;
pub const CARET_DOWN: &str = p::CARET_DOWN;
pub const CARET_DOUBLE_RIGHT: &str = p::CARET_DOUBLE_RIGHT;
pub const CARET_UP_DOWN: &str = p::CARET_UP_DOWN;
pub const CHECK: &str = p::CHECK;
pub const CHECK_CIRCLE: &str = p::CHECK_CIRCLE;
pub const CIRCLE_NOTCH: &str = p::CIRCLE_NOTCH;
pub const CORNERS_IN: &str = p::CORNERS_IN;
pub const CPU: &str = p::CPU;
pub const DOTS_SIX_VERTICAL: &str = p::DOTS_SIX_VERTICAL;
pub const DOWNLOAD_SIMPLE: &str = p::DOWNLOAD_SIMPLE;
pub const FILE_TEXT: &str = p::FILE_TEXT;
pub const FLASK: &str = p::FLASK;
pub const FLOPPY_DISK: &str = p::FLOPPY_DISK;
pub const FOLDER_OPEN: &str = p::FOLDER_OPEN;
pub const GAME_CONTROLLER: &str = p::GAME_CONTROLLER;
pub const GAUGE: &str = p::GAUGE;
pub const GEAR_SIX: &str = p::GEAR_SIX;
pub const GRAPHICS_CARD: &str = p::GRAPHICS_CARD;
pub const INFO: &str = p::INFO;
pub const KEYBOARD: &str = p::KEYBOARD;
pub const LIGHTNING: &str = p::LIGHTNING;
pub const MINUS: &str = p::MINUS;
pub const MONITOR: &str = p::MONITOR;
pub const PLAY: &str = p::PLAY;
pub const PLUS: &str = p::PLUS;
pub const PULSE: &str = p::PULSE;
pub const PUZZLE_PIECE: &str = p::PUZZLE_PIECE;
pub const ROCKET_LAUNCH: &str = p::ROCKET_LAUNCH;
pub const SHIELD_CHECK: &str = p::SHIELD_CHECK;
pub const SLIDERS_HORIZONTAL: &str = p::SLIDERS_HORIZONTAL;
pub const SPARKLE: &str = p::SPARKLE;
pub const SQUARE: &str = p::SQUARE;
pub const SQUARES_FOUR: &str = p::SQUARES_FOUR;
pub const STACK: &str = p::STACK;
pub const STOP: &str = p::STOP;
pub const TERMINAL_WINDOW: &str = p::TERMINAL_WINDOW;
pub const TRASH: &str = p::TRASH;
pub const UPLOAD_SIMPLE: &str = p::UPLOAD_SIMPLE;
pub const USER_CIRCLE: &str = p::USER_CIRCLE;
pub const WARNING: &str = p::WARNING;
pub const WARNING_CIRCLE: &str = p::WARNING_CIRCLE;
pub const WRENCH: &str = p::WRENCH;
pub const X: &str = p::X;
pub const X_CIRCLE: &str = p::X_CIRCLE;
pub const SUN: &str = p::SUN;
pub const MOON: &str = p::MOON;
pub const MAGNIFYING_GLASS: &str = p::MAGNIFYING_GLASS;
pub const CAMERA: &str = p::CAMERA;
pub const COPY: &str = p::COPY;
pub const ARROW_SQUARE_OUT: &str = p::ARROW_SQUARE_OUT;
pub const BUG: &str = p::BUG;
pub const CHART_LINE: &str = p::CHART_LINE;
pub const TIMER: &str = p::TIMER;
pub const LIST_CHECKS: &str = p::LIST_CHECKS;
pub const EXPORT: &str = p::EXPORT;

/// Icons that appear inside text (pills, fix lines), drawn with the text font: Inter must not have a
/// glyph of its own at these code points. Every other icon is drawn alone with `theme::icon`.
pub const INLINE: &[&str] = &[WARNING, CHECK, X_CIRCLE, WRENCH, SHIELD_CHECK, WARNING_CIRCLE];

pub const ALL: &[(&str, &str)] = &[
    ("SUN", SUN),
    ("MOON", MOON),
    ("MAGNIFYING_GLASS", MAGNIFYING_GLASS),
    ("CAMERA", CAMERA),
    ("COPY", COPY),
    ("ARROW_SQUARE_OUT", ARROW_SQUARE_OUT),
    ("BUG", BUG),
    ("CHART_LINE", CHART_LINE),
    ("TIMER", TIMER),
    ("LIST_CHECKS", LIST_CHECKS),
    ("EXPORT", EXPORT),
    ("ARROWS_CLOCKWISE", ARROWS_CLOCKWISE),
    ("ARROW_COUNTER_CLOCKWISE", ARROW_COUNTER_CLOCKWISE),
    ("CARET_DOUBLE_LEFT", CARET_DOUBLE_LEFT),
    ("CARET_UP", CARET_UP),
    ("CARET_DOWN", CARET_DOWN),
    ("CARET_DOUBLE_RIGHT", CARET_DOUBLE_RIGHT),
    ("CARET_UP_DOWN", CARET_UP_DOWN),
    ("CHECK", CHECK),
    ("CHECK_CIRCLE", CHECK_CIRCLE),
    ("CIRCLE_NOTCH", CIRCLE_NOTCH),
    ("CORNERS_IN", CORNERS_IN),
    ("CPU", CPU),
    ("DOTS_SIX_VERTICAL", DOTS_SIX_VERTICAL),
    ("DOWNLOAD_SIMPLE", DOWNLOAD_SIMPLE),
    ("FILE_TEXT", FILE_TEXT),
    ("FLASK", FLASK),
    ("FLOPPY_DISK", FLOPPY_DISK),
    ("FOLDER_OPEN", FOLDER_OPEN),
    ("GAME_CONTROLLER", GAME_CONTROLLER),
    ("GAUGE", GAUGE),
    ("GEAR_SIX", GEAR_SIX),
    ("GRAPHICS_CARD", GRAPHICS_CARD),
    ("INFO", INFO),
    ("KEYBOARD", KEYBOARD),
    ("LIGHTNING", LIGHTNING),
    ("MINUS", MINUS),
    ("MONITOR", MONITOR),
    ("PLAY", PLAY),
    ("PLUS", PLUS),
    ("PULSE", PULSE),
    ("PUZZLE_PIECE", PUZZLE_PIECE),
    ("ROCKET_LAUNCH", ROCKET_LAUNCH),
    ("SHIELD_CHECK", SHIELD_CHECK),
    ("SLIDERS_HORIZONTAL", SLIDERS_HORIZONTAL),
    ("SPARKLE", SPARKLE),
    ("SQUARE", SQUARE),
    ("SQUARES_FOUR", SQUARES_FOUR),
    ("STACK", STACK),
    ("STOP", STOP),
    ("TERMINAL_WINDOW", TERMINAL_WINDOW),
    ("TRASH", TRASH),
    ("UPLOAD_SIMPLE", UPLOAD_SIMPLE),
    ("USER_CIRCLE", USER_CIRCLE),
    ("WARNING", WARNING),
    ("WARNING_CIRCLE", WARNING_CIRCLE),
    ("WRENCH", WRENCH),
    ("X", X),
    ("X_CIRCLE", X_CIRCLE),
];

#[cfg(test)]
mod tests {
    /// Every icon has a glyph in Phosphor; and the icons used inside text are not shadowed by Inter
    /// (which has private-use alternates of its own: U+E000, U+E038, U+E094, U+E128, U+E12A, U+E140).
    #[test]
    fn every_icon_has_a_glyph_and_inter_does_not_shadow_it() {
        let ph = egui_phosphor::Variant::Regular.font_data();
        let ph = ttf_parser::Face::parse(&ph.font, 0).expect("phosphor parses");
        let inters: Vec<(&str, &[u8])> = vec![
            ("Regular", include_bytes!("../../assets/fonts/Inter-Regular.ttf")),
            ("Medium", include_bytes!("../../assets/fonts/Inter-Medium.ttf")),
            ("SemiBold", include_bytes!("../../assets/fonts/Inter-SemiBold.ttf")),
            ("Bold", include_bytes!("../../assets/fonts/Inter-Bold.ttf")),
        ];
        let inters: Vec<(&str, ttf_parser::Face)> = inters.into_iter().map(|(n, b)| (n, ttf_parser::Face::parse(b, 0).unwrap())).collect();
        let mut bad = Vec::new();
        for (name, s) in super::ALL {
            let c = s.chars().next().unwrap();
            assert_eq!(s.chars().count(), 1, "{name}");
            if ph.glyph_index(c).is_none() {
                bad.push(format!("{name} U+{:04X}: not in Phosphor", c as u32));
            }
            for (n, f) in &inters {
                if super::INLINE.contains(s) && f.glyph_index(c).is_some() {
                    bad.push(format!("{name} U+{:04X}: shadowed by Inter {n}", c as u32));
                }
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }
}
