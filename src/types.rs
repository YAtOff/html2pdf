//! Shared data model for html2pdf.
//!
//! Pipeline: HTML text -> `Vec<Block>` (styled tree, see `dom.rs`)
//!        -> `Doc` (laid-out pages, see `layout.rs`)
//!        -> PDF bytes (see `pdf.rs`).
//! All layout coordinates are in PDF points (1/72 inch), origin at the
//! TOP-LEFT of the page, y growing DOWNWARDS. `pdf.rs` flips to PDF's
//! bottom-left origin when emitting the file.

/// Which embedded font face to use.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FontKey {
    SansRegular,
    SansBold,
    SansItalic,
    SansBoldItalic,
    MonoRegular,
    MonoBold,
}

impl FontKey {
    /// Pick the face matching a style. `mono` wins over bold/italic variants
    /// (we only ship regular + bold mono).
    pub fn from_style(style: &Style) -> FontKey {
        if style.mono {
            if style.bold { FontKey::MonoBold } else { FontKey::MonoRegular }
        } else {
            match (style.bold, style.italic) {
                (false, false) => FontKey::SansRegular,
                (true, false) => FontKey::SansBold,
                (false, true) => FontKey::SansItalic,
                (true, true) => FontKey::SansBoldItalic,
            }
        }
    }
}

/// Inline character styling.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
    pub underline: bool,
    /// URI target if this run is inside an `<a href="...">`.
    pub link: Option<String>,
}

/// A piece of inline content.
#[derive(Clone, Debug)]
pub enum Inline {
    /// A visible text run. `dom.rs` guarantees: HTML entities are decoded,
    /// whitespace runs are collapsed to single spaces (U+0020), and U+00A0
    /// (from `&nbsp;`) is preserved as a non-breaking character.
    Text { text: String, style: Style },
    /// `<br>` — forced line break.
    Break,
}

/// One table row; each cell is inline content.
pub type TableRow = Vec<Vec<Inline>>;

/// A block-level content node.
#[derive(Clone, Debug)]
pub enum Block {
    Paragraph { inlines: Vec<Inline> },
    /// level 1..=6
    Heading { level: u8, inlines: Vec<Inline> },
    List { ordered: bool, start: i64, items: Vec<Vec<Block>> },
    /// `<pre>` content: raw text, entities decoded, tabs expanded to 4 spaces,
    /// line breaks preserved; never re-wrapped except for hard overflow.
    Preformatted { text: String },
    Blockquote { children: Vec<Block> },
    /// `<hr>`
    Rule,
    Table { rows: Vec<TableRow>, header_rows: usize },
}

/// A single glyph placed on a page.
#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    /// Glyph id inside the font face.
    pub gid: u32,
    /// Horizontal advance in points.
    pub x_advance: f64,
    /// The Unicode character this glyph represents (for ToUnicode mapping).
    pub unicode: char,
}

/// Color as additive RGB, each component 0.0..=1.0.
pub type Color = (f32, f32, f32);

/// One drawing operation on a page (top-left origin, y grows down).
#[derive(Clone, Debug)]
pub enum Draw {
    /// A run of glyphs sharing font, size and color. `x`, `y` is the position
    /// of the FIRST glyph's origin (baseline start).
    Text { x: f64, y: f64, size: f64, font: FontKey, color: Color, glyphs: Vec<Glyph> },
    /// Filled axis-aligned rectangle.
    Rect { x: f64, y: f64, w: f64, h: f64, color: Color },
}

/// A laid-out page.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub items: Vec<Draw>,
    /// Clickable link rectangles (top-left origin, y grows down), with URI.
    pub links: Vec<(f64, f64, f64, f64, String)>,
}

/// A fully laid-out document ready to be serialized by `pdf.rs`.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    pub title: Option<String>,
    pub pages: Vec<Page>,
}

/// Page geometry settings derived from the Python-level options.
#[derive(Copy, Clone, Debug)]
pub struct PageSettings {
    pub width: f64,
    pub height: f64,
    pub margin: f64,
    pub base_font_size: f64,
}

impl PageSettings {
    pub fn content_width(&self) -> f64 {
        self.width - 2.0 * self.margin
    }
}
