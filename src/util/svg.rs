//! Pictures drawn as SVG and turned into PNGs with `resvg`, for Discord messages that show
//! more than an embed can: the movie wheel's rounds and the control panel's status.
//!
//! A feature builds its picture with [`Svg`] (pure, tested like any other string) and turns
//! it into a PNG with [`to_png`].
//!
//! The Inter font is built into the binary, so pictures look the same on every server.
//! The server's own fonts are loaded too, so names with emoji or other scripts still show
//! when the server has a font for them (for example `fonts-noto-color-emoji`). Characters
//! Inter doesn't have are given their font here, in a `<tspan>` (see [`markup`]): resvg's
//! own fallback redraws the whole text in the other font and gives up when the two
//! drawings don't line up, which leaves those characters blank.
//!
//! SVG has no text layout: every position is computed by the caller. Text widths are
//! estimated from the number of characters ([`text_width`]), which is close enough for
//! Inter at these sizes.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Context as _;
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{self, fontdb};
use skrifa::MetadataProvider as _;
use tracing::warn;

/// Pictures are rendered at twice their size in SVG units, for sharp text.
const SCALE: f32 = 2.0;
pub const FONT: &str = "Inter";

// Colors, close to Discord's dark theme.
pub const BG: &str = "#232428";
pub const ROW: &str = "#313338";
pub const LINE: &str = "#3a3c42";
pub const TEXT: &str = "#f2f3f5";
pub const MUTED: &str = "#b5bac1";
pub const DIM: &str = "#80848e";
pub const GREEN: &str = "#23a55a";
pub const RED: &str = "#f23f43";
pub const AMBER: &str = "#f0b232";
pub const ACCENT: &str = "#5865f2";

static FONTS: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    db.load_font_data(include_bytes!("../../assets/fonts/Inter-Regular.otf").to_vec());
    db.load_font_data(include_bytes!("../../assets/fonts/Inter-SemiBold.otf").to_vec());
    db.load_font_data(include_bytes!("../../assets/fonts/Inter-Bold.otf").to_vec());
    // Fallbacks for characters Inter doesn't have, like emoji.
    db.load_system_fonts();
    Arc::new(db)
});

/// Reads the fonts, which [`to_png`] otherwise does on its first call.
pub fn load_fonts() {
    LazyLock::force(&FONTS);
}

thread_local! {
    /// Set while a picture is built again without [`markup`]'s fonts.
    static PLAIN: Cell<bool> = const { Cell::new(false) };
}

/// Builds a picture with `build` and renders it as a PNG. Call it off the async threads:
/// drawing takes some CPU, and picking fonts reads font files. resvg can panic on text it
/// can't lay out; the picture is then built again with every font choice left to resvg.
pub fn to_png(build: impl Fn() -> String) -> anyhow::Result<Vec<u8>> {
    let svg = build();
    if let Ok(result) = std::panic::catch_unwind(|| png(&svg)) {
        return result;
    }
    PLAIN.set(true);
    let svg = build();
    PLAIN.set(false);
    // If it panics again, return an error so the caller can show something else.
    std::panic::catch_unwind(|| png(&svg))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("resvg panicked drawing the picture")))
}

/// Renders SVG text to PNG bytes.
pub fn png(svg: &str) -> anyhow::Result<Vec<u8>> {
    let options = usvg::Options {
        font_family: FONT.to_string(),
        fontdb: FONTS.clone(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).context("reading the picture's SVG")?;
    let size = tree
        .size()
        .to_int_size()
        .scale_by(SCALE)
        .context("empty picture")?;
    let mut pixmap = Pixmap::new(size.width(), size.height()).context("picture too big")?;
    resvg::render(
        &tree,
        Transform::from_scale(SCALE, SCALE),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

/// About how wide Inter draws `text` at `size`.
pub fn text_width(text: &str, size: f32) -> f32 {
    text.chars().map(columns).sum::<usize>() as f32 * size * 0.6
}

/// How many letters wide a character is drawn, about: emoji take two.
pub fn columns(c: char) -> usize {
    if is_emoji(c) { 2 } else { 1 }
}

/// Text as SVG: simplified (see [`simplify`]), escaped, and with characters Inter doesn't
/// have in a `<tspan>` with a font that has them.
pub fn markup(text: &str) -> String {
    let text = simplify(text);
    if PLAIN.get() {
        esc(&text)
    } else {
        font_runs(&text, fallback_font)
    }
}

/// Emoji sequences resvg can't draw: skin tones, variation selectors and joiners are left
/// out, and flags become their country's letters (🇳🇱 is drawn as NL). The emoji themselves
/// stay.
fn simplify(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\u{1F1E6}'..='\u{1F1FF}' => char::from_u32(c as u32 - 0x1F1E6 + 'A' as u32),
            '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{20E3}' => None,
            '\u{E0020}'..='\u{E007F}' => None,
            c => Some(c),
        })
        .collect()
}

/// A font for some characters: its family, and the weight of its face that has them.
type Font = (String, u16);

/// Wraps runs of characters that `font_for` gives a font in a `<tspan>` with that font.
///
/// A letter with a combining mark after it is left to resvg's own fallback, which can draw
/// it: resvg panics on such a pair inside a `<tspan>` with its own font.
fn font_runs(text: &str, font_for: impl Fn(char) -> Option<Font>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    // The font of the open `<tspan>`, if one is open.
    let mut current: Option<Font> = None;
    for (i, &c) in chars.iter().enumerate() {
        let combined = is_mark(c) || chars.get(i + 1).is_some_and(|&next| is_mark(next));
        let font = if combined { None } else { font_for(c) };
        if font != current {
            if current.is_some() {
                out.push_str("</tspan>");
            }
            if let Some((family, weight)) = &font {
                out.push_str(&format!(
                    "<tspan font-family=\"{}\" font-weight=\"{weight}\">",
                    esc(family)
                ));
            }
            current = font;
        }
        out.push_str(&esc(c.encode_utf8(&mut [0; 4])));
    }
    if current.is_some() {
        out.push_str("</tspan>");
    }
    out
}

/// Combining marks, drawn on the character before them.
fn is_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}')
}

/// Symbols and pictographs, which look best in an emoji font.
pub fn is_emoji(c: char) -> bool {
    matches!(c, '\u{2600}'..='\u{27BF}' | '\u{2B00}'..='\u{2BFF}' | '\u{1F000}'..='\u{1FAFF}')
}

/// The font to draw `c` with when Inter doesn't have it: an emoji font for emoji, otherwise
/// preferably a sans-serif one, in the weight closest to the semibold names. `None` when
/// Inter has it, or no font does. Remembered per character, since looking through the
/// fonts reads their files.
///
/// The weight matters: a family's bold face often lacks characters its regular face has,
/// and resvg would pick the bold face for bold text.
fn fallback_font(c: char) -> Option<Font> {
    static KNOWN: LazyLock<Mutex<HashMap<char, Option<Font>>>> = LazyLock::new(Default::default);
    if c.is_ascii() {
        return None;
    }
    if let Some(font) = KNOWN.lock().expect("font cache poisoned").get(&c) {
        return font.clone();
    }

    let db = &**FONTS;
    let family = |face: &fontdb::FaceInfo| {
        let (name, _) = face.families.first()?;
        Some((name.clone(), face.weight.0))
    };
    let font = if db
        .faces()
        .any(|face| family(face).is_some_and(|(name, _)| name == FONT) && has_char(db, face.id, c))
    {
        None
    } else {
        let found = db
            .faces()
            .filter(|face| has_char(db, face.id, c))
            .filter_map(family)
            .min_by_key(|(name, weight)| {
                let kind = match name {
                    n if is_emoji(c) && n.contains("Emoji") => 0,
                    n if n.starts_with("Noto Sans") => 1,
                    n if n.contains("Sans") => 2,
                    _ => 3,
                };
                (kind, weight.abs_diff(600))
            });
        // Once per character, thanks to the cache below.
        if found.is_none() && !c.is_control() && !is_mark(c) {
            warn!(
                "no installed font has {c} (U+{:04X}); it shows as a box. See the README for fonts.",
                c as u32
            );
        }
        found
    };
    KNOWN
        .lock()
        .expect("font cache poisoned")
        .insert(c, font.clone());
    font
}

fn has_char(db: &fontdb::Database, face: fontdb::ID, c: char) -> bool {
    db.with_face_data(face, |data, index| {
        let font = skrifa::FontRef::from_index(data, index).ok()?;
        font.charmap().map(c)
    })
    .flatten()
    .is_some()
}

/// Escapes text for SVG. Control characters XML doesn't allow (all but tab and line
/// breaks) are dropped, since resvg can't read a picture that has them.
pub fn esc(text: &str) -> String {
    let forbidden = |c: &char| {
        matches!(c, '\0'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}')
            || matches!(c, '\u{FFFE}' | '\u{FFFF}')
    };
    let text: String = text.chars().filter(|c| !forbidden(c)).collect();
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// How a piece of text looks.
#[derive(Clone, Copy)]
pub struct Style {
    pub size: f32,
    pub color: &'static str,
    pub weight: u16,
    pub anchor: &'static str,
    pub spaced: bool,
}

impl Style {
    pub fn new(size: f32, color: &'static str) -> Style {
        Style {
            size,
            color,
            weight: 400,
            anchor: "start",
            spaced: false,
        }
    }
    pub fn weight(mut self, weight: u16) -> Style {
        self.weight = weight;
        self
    }
    /// Right-aligned at `x`.
    pub fn end(mut self) -> Style {
        self.anchor = "end";
        self
    }
    /// Centered on `x`.
    pub fn middle(mut self) -> Style {
        self.anchor = "middle";
        self
    }
    /// Letter-spaced, for small capital labels.
    pub fn spaced(mut self) -> Style {
        self.spaced = true;
        self
    }
}

/// A picture being built, top to bottom. `y` is where the next part goes.
pub struct Svg {
    parts: Vec<String>,
    /// Width in SVG units; the height is wherever `y` ends.
    pub width: f32,
    pub y: f32,
}

impl Svg {
    pub fn new(width: f32) -> Svg {
        Svg {
            parts: Vec::new(),
            width,
            y: 0.0,
        }
    }

    /// Text that is escaped here.
    pub fn text(&mut self, x: f32, y: f32, text: &str, style: Style) {
        self.raw_text(x, y, &markup(text), style);
    }

    /// Text that may hold `<tspan>` markup; the caller escapes it.
    pub fn raw_text(&mut self, x: f32, y: f32, markup: &str, style: Style) {
        let spacing = if style.spaced {
            " letter-spacing=\"1.2\""
        } else {
            ""
        };
        self.parts.push(format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"{FONT}\" font-size=\"{}\" font-weight=\"{}\" fill=\"{}\" text-anchor=\"{}\"{spacing}>{markup}</text>",
            style.size, style.weight, style.color, style.anchor
        ));
    }

    /// A rectangle at `[x, y, width, height]`; `extra` holds more attributes.
    pub fn rect(&mut self, [x, y, w, h]: [f32; 4], fill: &str, radius: f32, extra: &str) {
        self.parts.push(format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"{radius}\" fill=\"{fill}\" {extra}/>"
        ));
    }

    /// Any other SVG element, like a `<path>` or `<circle>`. The caller escapes it.
    pub fn raw(&mut self, element: String) {
        self.parts.push(element);
    }

    /// The background of every other table row.
    pub fn stripe(&mut self, row: usize, height: f32) {
        if row.is_multiple_of(2) {
            self.rect([16.0, self.y, self.width - 32.0, height], ROW, 6.0, "");
        }
    }

    /// "STANDINGS"
    pub fn section(&mut self, title: &str) {
        self.y += 14.0;
        self.text(
            24.0,
            self.y,
            &title.to_uppercase(),
            Style::new(11.0, DIM).weight(700).spaced(),
        );
        self.y += 10.0;
    }

    /// A line of grey text where a section has nothing to show.
    pub fn note(&mut self, text: &str) {
        self.y += 20.0;
        self.text(24.0, self.y, text, Style::new(13.0, DIM));
        self.y += 8.0;
    }

    pub fn finish(self) -> String {
        let (width, height) = (self.width, self.y.ceil());
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\"><rect width=\"{width}\" height=\"{height}\" rx=\"14\" fill=\"{BG}\"/>{}</svg>",
            self.parts.concat()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_png() {
        let mut svg = Svg::new(200.0);
        svg.section("Title");
        svg.note("Hello <world> 👍");
        let png = to_png(|| {
            let mut svg = Svg::new(200.0);
            svg.y = 40.0;
            svg.text(10.0, 20.0, "Bob 🎬", Style::new(14.0, TEXT));
            svg.finish()
        })
        .unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!(svg.finish().contains("Hello &lt;world&gt;"));
    }

    #[test]
    fn escapes_and_drops_control_characters() {
        assert_eq!(esc("a\u{0}b\u{1B}c\td\n<"), "abc\td\n&lt;");
        assert!(png(&format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><text>{}</text></svg>", esc("x\u{7}"))).is_ok());
    }

    #[test]
    fn fonts_per_character() {
        let font_for = |c: char| match c {
            '𝗓' => Some(("Math".to_string(), 400)),
            '😀' | '👍' => Some(("Emoji".to_string(), 400)),
            _ => None,
        };
        assert_eq!(font_runs("Bob <3", font_for), "Bob &lt;3");
        assert_eq!(
            font_runs("𝗓𝗓oe", font_for),
            "<tspan font-family=\"Math\" font-weight=\"400\">𝗓𝗓</tspan>oe"
        );
        // A combining mark stays with its letter, outside the `<tspan>`.
        assert_eq!(
            font_runs("𝗓\u{301}𝗓", font_for),
            "𝗓\u{301}<tspan font-family=\"Math\" font-weight=\"400\">𝗓</tspan>"
        );
        assert_eq!(simplify("a👍🏽 👨‍👩‍👧 🇳🇱 1️⃣ ❤️"), "a👍 👨👩👧 NL 1 ❤");
        assert_eq!(fallback_font('a'), None);
        // Inter has these.
        assert_eq!(fallback_font('é'), None);
        assert_eq!(fallback_font('−'), None);
    }
}
