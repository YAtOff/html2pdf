//! Embedded DejaVu font faces: loading, metrics and glyph mapping.
//!
//! The TTF files in `assets/fonts/` are compiled into the extension with
//! `include_bytes!`, so the package needs no fontconfig, no filesystem fonts
//! and no other system libraries. DejaVu covers Latin, Cyrillic and Greek.

use crate::types::FontKey;
use std::collections::BTreeMap;

static SANS: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");
static SANS_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Bold.ttf");
static SANS_ITALIC: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Oblique.ttf");
static SANS_BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-BoldOblique.ttf");
static MONO: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono.ttf");
static MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono-Bold.ttf");

/// Font metrics in em units: multiply by the font size in points.
#[derive(Clone, Copy, Debug)]
pub struct DescriptorEm {
    pub units_per_em: u16,
    pub ascender: f64,
    pub descender: f64, // negative
    pub cap_height: f64,
    pub bbox: [f64; 4], // x_min, y_min, x_max, y_max
    pub italic_angle: f64,
    pub is_bold: bool,
    pub is_italic: bool,
    pub is_mono: bool,
}

struct FaceData {
    data: &'static [u8],
    face: ttf_parser::Face<'static>,
    /// glyph id -> first Unicode character seen using it (for ToUnicode).
    used: BTreeMap<u32, char>,
}

/// The six embedded faces plus the set of glyphs used by the current document.
pub struct FontStore {
    faces: BTreeMap<FontKey, FaceData>,
}

impl FontStore {
    pub fn new() -> FontStore {
        let defs = [
            (FontKey::SansRegular, SANS),
            (FontKey::SansBold, SANS_BOLD),
            (FontKey::SansItalic, SANS_ITALIC),
            (FontKey::SansBoldItalic, SANS_BOLD_ITALIC),
            (FontKey::MonoRegular, MONO),
            (FontKey::MonoBold, MONO_BOLD),
        ];
        let mut faces = BTreeMap::new();
        for (key, bytes) in defs {
            let face = ttf_parser::Face::parse(bytes, 0)
                .unwrap_or_else(|e| panic!("embedded font {key:?} is corrupt: {e:?}"));
            faces.insert(key, FaceData { data: bytes, face, used: BTreeMap::new() });
        }
        FontStore { faces }
    }

    /// Map a character to a glyph id in the given face.
    pub fn glyph_index(&self, key: FontKey, c: char) -> Option<u32> {
        self.faces[&key].face.glyph_index(c).map(|g| u32::from(g.0))
    }

    /// Glyph advance in em units (a fraction of the font size).
    pub fn advance_em(&self, key: FontKey, gid: u32) -> f64 {
        let face = &self.faces[&key].face;
        face.glyph_hor_advance(ttf_parser::GlyphId(gid as u16))
            .map_or(0.6, |a| f64::from(a) / f64::from(face.units_per_em()))
    }

    /// (ascent, descent) in em units; descent is negative.
    pub fn line_metrics_em(&self, key: FontKey) -> (f64, f64) {
        let face = &self.faces[&key].face;
        let upem = f64::from(face.units_per_em());
        (f64::from(face.ascender()) / upem, f64::from(face.descender()) / upem)
    }

    /// Descriptor metrics for the PDF font dictionary.
    pub fn descriptor(&self, key: FontKey) -> DescriptorEm {
        let face = &self.faces[&key].face;
        let upem = f64::from(face.units_per_em());
        let b = face.global_bounding_box();
        DescriptorEm {
            units_per_em: face.units_per_em(),
            ascender: f64::from(face.ascender()) / upem,
            descender: f64::from(face.descender()) / upem,
            cap_height: face.capital_height().map_or(0.7, |h| f64::from(h) / upem),
            bbox: [
                f64::from(b.x_min) / upem,
                f64::from(b.y_min) / upem,
                f64::from(b.x_max) / upem,
                f64::from(b.y_max) / upem,
            ],
            italic_angle: f64::from(face.italic_angle()),
            is_bold: matches!(key, FontKey::SansBold | FontKey::SansBoldItalic | FontKey::MonoBold),
            is_italic: matches!(key, FontKey::SansItalic | FontKey::SansBoldItalic),
            is_mono: matches!(key, FontKey::MonoRegular | FontKey::MonoBold),
        }
    }

    /// Record glyph usage (drives subsetting and ToUnicode generation).
    pub fn mark_used(&mut self, key: FontKey, gid: u32, c: char) {
        if let Some(face) = self.faces.get_mut(&key) {
            face.used.entry(gid).or_insert(c);
        }
    }

    /// Raw font file bytes for embedding/subsetting.
    pub fn font_data(&self, key: FontKey) -> &'static [u8] {
        self.faces[&key].data
    }

    /// Which faces have at least one used glyph.
    pub fn used_keys(&self) -> Vec<FontKey> {
        self.faces
            .iter()
            .filter(|(_, f)| !f.used.is_empty())
            .map(|(k, _)| *k)
            .collect()
    }

    /// Sorted glyph ids used in a face.
    pub fn used_glyphs(&self, key: FontKey) -> Vec<u32> {
        self.faces[&key].used.keys().copied().collect()
    }

    /// (glyph id, Unicode char) pairs for the ToUnicode CMap.
    pub fn unicode_map(&self, key: FontKey) -> Vec<(u32, char)> {
        self.faces[&key].used.iter().map(|(g, c)| (*g, *c)).collect()
    }
}

/// PostScript name of an embedded face (also used as the PDF BaseFont name).
pub fn postscript_name(key: FontKey) -> &'static str {
    match key {
        FontKey::SansRegular => "DejaVuSans",
        FontKey::SansBold => "DejaVuSans-Bold",
        FontKey::SansItalic => "DejaVuSans-Oblique",
        FontKey::SansBoldItalic => "DejaVuSans-BoldOblique",
        FontKey::MonoRegular => "DejaVuSansMono",
        FontKey::MonoBold => "DejaVuSansMono-Bold",
    }
}

impl Default for FontStore {
    fn default() -> Self {
        Self::new()
    }
}
