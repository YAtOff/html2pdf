//! Laid-out document -> PDF bytes.
//!
//! Layout coordinates use a top-left origin with y growing downwards; the PDF
//! coordinate system is bottom-left with y growing upwards, so every y is
//! flipped via `y_pdf = page_height - y_top_down` on the way out.
//!
//! Fonts are embedded as CIDFontType2 descendants of Type0 fonts with
//! `Identity-H` encoding: the content streams show 16-bit glyph ids directly,
//! and text extraction works through a generated ToUnicode CMap. Glyph ids are
//! renumbered by the subsetter, so the shown bytes, the `/W` widths array and
//! the ToUnicode pairs all go through the same remapping.

use crate::fonts::{postscript_name, FontStore};
use crate::types::{Doc, Draw, FontKey, Glyph, Page};
use pdf_writer::types::{
    ActionType, AnnotationType, CidFontType, FontFlags, SystemInfo, UnicodeCmap,
};
use pdf_writer::{Content, Filter, Name, Pdf, Rect, Ref, Str, TextStr};
use std::borrow::Cow;
use subsetter::{subset, GlyphRemapper};

/// Producer string for the document information dictionary. No timestamps are
/// written so that identical input yields identical output.
const PRODUCER: &str = "html2pdf 0.1.0 (Rust, pdf-writer; DejaVu fonts)";

/// The identity CIDSystemInfo shared by the descendant fonts and ToUnicode maps.
const IDENTITY_SYSINFO: SystemInfo<'static> = SystemInfo {
    registry: Str(b"Adobe"),
    ordering: Str(b"Identity"),
    supplement: 0,
};

/// Width assigned to glyphs without an explicit entry in the `/W` array
/// (covers composite-glyph components pulled in by the subsetter's closure).
const DEFAULT_WIDTH: f32 = 600.0;

/// Streams shorter than this (raw) are written uncompressed.
const MIN_COMPRESS_LEN: usize = 64;

/// Serialize a laid-out document to PDF bytes with subset embedded fonts.
pub fn write(doc: &Doc, store: &FontStore) -> Vec<u8> {
    let mut pdf = Pdf::new();

    // ---- Allocate object ids (bump allocator, deterministic layout) ----
    // catalog=1, pages=2, info=3, then per page: page + content + one ref per
    // link annotation, then per used font five refs, in `used_keys()` order.
    let mut next = Ref::new(1);
    let catalog_id = next.bump();
    let pages_id = next.bump();
    let info_id = next.bump();

    let page_refs: Vec<(Ref, Ref, Vec<Ref>)> = doc
        .pages
        .iter()
        .map(|page| {
            let page_id = next.bump();
            let content_id = next.bump();
            let annots = page.links.iter().map(|_| next.bump()).collect();
            (page_id, content_id, annots)
        })
        .collect();

    let plans: Vec<FontPlan> = store
        .used_keys()
        .iter()
        .enumerate()
        .map(|(index, &key)| build_font_plan(store, key, index, &mut next))
        .collect();

    // ---- Catalog, page tree, info ----
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.pages(pages_id)
        .kids(page_refs.iter().map(|(id, _, _)| *id))
        .count(doc.pages.len() as i32);

    {
        let mut info = pdf.document_info(info_id);
        info.producer(TextStr(PRODUCER));
        if let Some(title) = &doc.title {
            info.title(TextStr(title.as_str()));
        }
    }

    // ---- Pages and their content streams ----
    for (page, (page_id, content_id, annot_refs)) in doc.pages.iter().zip(&page_refs) {
        {
            let mut page_writer = pdf.page(*page_id);
            page_writer
                .parent(pages_id)
                .media_box(Rect::new(0.0, 0.0, page.width as f32, page.height as f32));
            if !plans.is_empty() {
                let mut resources = page_writer.resources();
                let mut fonts = resources.fonts();
                for plan in &plans {
                    fonts.pair(Name(plan.resource.as_slice()), plan.type0);
                }
            }
            page_writer.contents(*content_id);
            if !annot_refs.is_empty() {
                page_writer.annotations(annot_refs.iter().copied());
            }
        }

        let content = build_page_content(page, &plans);
        write_stream(&mut pdf, *content_id, content.as_slice());
    }

    // ---- Font objects ----
    for plan in &plans {
        let base = Name(plan.base_font.as_bytes());

        pdf.type0_font(plan.type0)
            .base_font(base)
            .encoding_predefined(Name(b"Identity-H"))
            .descendant_font(plan.cid)
            .to_unicode(plan.to_unicode);

        {
            let mut cid = pdf.cid_font(plan.cid);
            cid.subtype(CidFontType::Type2)
                .base_font(base)
                .system_info(IDENTITY_SYSINFO)
                .font_descriptor(plan.descriptor)
                .default_width(DEFAULT_WIDTH)
                .cid_to_gid_map_predefined(Name(b"Identity"));
            let mut widths = cid.widths();
            for (start, run) in plan.width_runs(store) {
                widths.consecutive(start, run);
            }
        }

        let d = store.descriptor(plan.key);
        pdf.font_descriptor(plan.descriptor)
            .name(base)
            .flags(FontFlags::SYMBOLIC)
            .bbox(Rect::new(
                (d.bbox[0] * 1000.0) as f32,
                (d.bbox[1] * 1000.0) as f32,
                (d.bbox[2] * 1000.0) as f32,
                (d.bbox[3] * 1000.0) as f32,
            ))
            .italic_angle(d.italic_angle as f32)
            .ascent((d.ascender * 1000.0) as f32)
            .descent((d.descender * 1000.0) as f32)
            .cap_height((d.cap_height * 1000.0) as f32)
            .stem_v(if d.is_bold { 120.0 } else { 80.0 })
            .font_file2(plan.font_file);

        let mut cmap = UnicodeCmap::new(Name(b"html2pdf-tounicode"), IDENTITY_SYSINFO);
        for (old_gid, ch) in store.unicode_map(plan.key) {
            if old_gid <= u16::MAX as u32 {
                cmap.pair(plan.output_gid(old_gid), ch);
            }
        }
        let cmap_bytes = cmap.finish();
        write_stream(&mut pdf, plan.to_unicode, cmap_bytes.as_slice());

        // Font program stream. `/Length1` (required on FontFile2 by ISO
        // 32000-1, Table 126) records the *uncompressed* font length.
        let font_raw = plan.embed.as_ref();
        if font_raw.len() < MIN_COMPRESS_LEN {
            pdf.stream(plan.font_file, font_raw)
                .pair(Name(b"Length1"), font_raw.len() as i32);
        } else {
            let compressed = miniz_oxide::deflate::compress_to_vec_zlib(font_raw, 6);
            pdf.stream(plan.font_file, &compressed)
                .filter(Filter::FlateDecode)
                .pair(Name(b"Length1"), font_raw.len() as i32);
        }
    }

    // ---- Link annotations ----
    for (page, (_, _, annot_refs)) in doc.pages.iter().zip(&page_refs) {
        for ((x, y_top, w, h, uri), &annot_id) in page.links.iter().zip(annot_refs) {
            let mut annot = pdf.annotation(annot_id);
            annot
                .subtype(AnnotationType::Link)
                .rect(Rect::new(
                    *x as f32,
                    (page.height - y_top - h) as f32,
                    (*x + *w) as f32,
                    (page.height - y_top) as f32,
                ))
                // Links are styled by the layout engine; suppress any border.
                .border(0.0, 0.0, 0.0, None);
            annot
                .action()
                .action_type(ActionType::Uri)
                .uri(Str(uri.as_bytes()));
        }
    }

    pdf.finish()
}

/// Everything needed to embed one used font face.
struct FontPlan {
    key: FontKey,
    /// Resource name shown in content streams ("F0".."F5").
    resource: [u8; 2],
    /// `/BaseFont` value: `AAAAAA+DejaVuSans` for subsets, the plain
    /// PostScript name when subsetting failed and the full font is embedded.
    base_font: String,
    type0: Ref,
    cid: Ref,
    descriptor: Ref,
    to_unicode: Ref,
    font_file: Ref,
    /// Present when subsetting succeeded; glyph ids are then remapped
    /// throughout. `None` means the full font is embedded and original glyph
    /// ids are used everywhere instead.
    remapper: Option<GlyphRemapper>,
    /// Final bytes for the FontFile2 stream.
    embed: Cow<'static, [u8]>,
}

impl FontPlan {
    /// The glyph id to show in PDFs for an original font glyph id.
    fn output_gid(&self, old: u32) -> u16 {
        if old > u16::MAX as u32 {
            return 0; // .notdef; unreachable for ttf-parser-sourced gids
        }
        match &self.remapper {
            Some(remapper) => remapper.get(old as u16).unwrap_or(0),
            None => old as u16,
        }
    }

    /// `(output gid, old gid)` pairs in ascending output-gid order, driving
    /// the `/W` array. In remapped mode this starts with `.notdef` at 0 and
    /// is consecutive by construction of `GlyphRemapper`.
    fn width_pairs(&self, store: &FontStore) -> Vec<(u16, u32)> {
        match &self.remapper {
            Some(remapper) => remapper
                .remapped_gids()
                .enumerate()
                .map(|(new, old)| (new as u16, u32::from(old)))
                .collect(),
            None => store
                .used_glyphs(self.key)
                .into_iter()
                .filter(|&old| old <= u16::MAX as u32)
                .map(|old| (old as u16, old))
                .collect(),
        }
    }

    /// Consecutive `/W` runs: `(first cid, widths in 1000 units per em)`.
    /// The width for each output gid always comes from the *original* glyph's
    /// advance, so subset renumbering can never mix metrics up.
    fn width_runs(&self, store: &FontStore) -> Vec<(u16, Vec<f32>)> {
        let mut runs: Vec<(u16, Vec<f32>)> = Vec::new();
        let mut last: Option<u16> = None;
        for (out, old) in self.width_pairs(store) {
            let width = (store.advance_em(self.key, old) * 1000.0) as f32;
            if last == Some(out.saturating_sub(1)) && out > 0 {
                runs.last_mut().map(|run| run.1.push(width));
            } else {
                runs.push((out, vec![width]));
            }
            last = Some(out);
        }
        runs
    }
}

/// Subset one face and allocate its object ids. On subsetter failure the full
/// font is embedded and original glyph ids are used (correct, just larger).
fn build_font_plan(store: &FontStore, key: FontKey, index: usize, next: &mut Ref) -> FontPlan {
    let mut remapper = GlyphRemapper::new(); // maps .notdef (gid 0) to 0
    for &gid in &store.used_glyphs(key) {
        if gid <= u16::MAX as u32 {
            remapper.remap(gid as u16);
        }
    }

    let (remapper, embed) = match subset(store.font_data(key), 0, &remapper) {
        Ok(subset_bytes) => (Some(remapper), Cow::Owned(subset_bytes)),
        Err(_) => (None, Cow::Borrowed(store.font_data(key))),
    };

    let mut tag = *b"AAAAAA";
    tag[5] += index as u8; // one distinct tag per face, in used_keys() order
    let tag = String::from_utf8_lossy(&tag);
    let base_font = if remapper.is_some() {
        format!("{tag}+{}", postscript_name(key))
    } else {
        postscript_name(key).to_string()
    };

    FontPlan {
        key,
        resource: [b'F', b'0' + index as u8],
        base_font,
        type0: next.bump(),
        cid: next.bump(),
        descriptor: next.bump(),
        to_unicode: next.bump(),
        font_file: next.bump(),
        remapper,
        embed,
    }
}

/// Build the content stream for one page (top-left -> bottom-left flip).
fn build_page_content(page: &Page, plans: &[FontPlan]) -> pdf_writer::Buf {
    let mut content = Content::new();
    for item in &page.items {
        match item {
            Draw::Rect { x, y, w, h, color } => {
                content.set_fill_rgb(color.0, color.1, color.2);
                content.rect(
                    *x as f32,
                    (page.height - y - h) as f32,
                    *w as f32,
                    *h as f32,
                );
                content.fill_nonzero();
            }
            Draw::Text { x, y, size, font, color, glyphs } => {
                let Some(plan) = plans.iter().find(|plan| plan.key == *font) else {
                    continue; // face never used per the store; nothing embedded
                };
                let gid_bytes = glyph_run_bytes(plan, glyphs);
                content.begin_text();
                content.set_fill_rgb(color.0, color.1, color.2);
                content.set_font(Name(plan.resource.as_slice()), *size as f32);
                content.set_text_matrix([
                    1.0,
                    0.0,
                    0.0,
                    1.0,
                    *x as f32,
                    (page.height - y) as f32,
                ]);
                content.show(Str(&gid_bytes));
                content.end_text();
            }
        }
    }
    content.finish()
}

/// Write `data` as a stream object, deflating it when worthwhile.
fn write_stream(pdf: &mut Pdf, id: Ref, data: &[u8]) {
    if data.len() < MIN_COMPRESS_LEN {
        pdf.stream(id, data);
    } else {
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(data, 6);
        pdf.stream(id, &compressed).filter(Filter::FlateDecode);
    }
}

/// The `Tj` payload for one text run: each glyph's output id as two
/// big-endian bytes. Glyphs in a run are contiguous; advances come from the
/// font's `/W` widths, matching the layout engine's metrics.
fn glyph_run_bytes(plan: &FontPlan, glyphs: &[Glyph]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(glyphs.len() * 2);
    for glyph in glyphs {
        bytes.extend_from_slice(&plan.output_gid(glyph.gid).to_be_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Glyph, Page};

    /// Mark 'H' and 'i' used in the regular sans face and build a one-page
    /// document drawing a rect, a two-glyph text run and two links.
    fn sample_doc(store: &mut FontStore) -> Doc {
        let h = store.glyph_index(FontKey::SansRegular, 'H').expect("H glyph");
        let i = store.glyph_index(FontKey::SansRegular, 'i').expect("i glyph");
        store.mark_used(FontKey::SansRegular, h, 'H');
        store.mark_used(FontKey::SansRegular, i, 'i');

        let page = Page {
            width: 612.0,
            height: 792.0,
            items: vec![
                Draw::Rect {
                    x: 50.0,
                    y: 60.0,
                    w: 200.0,
                    h: 2.0,
                    color: (0.5, 0.5, 0.5),
                },
                Draw::Text {
                    x: 72.0,
                    y: 100.0,
                    size: 12.0,
                    font: FontKey::SansRegular,
                    color: (0.0, 0.0, 0.0),
                    glyphs: vec![
                        Glyph { gid: h, x_advance: 8.4, unicode: 'H' },
                        Glyph { gid: i, x_advance: 3.3, unicode: 'i' },
                    ],
                },
            ],
            links: vec![
                (60.0, 90.0, 80.0, 12.0, "https://example.com/".to_string()),
                (610.0, 700.0, 90.0, 12.0, "https://example.org/x".to_string()),
            ],
        };

        Doc { title: Some("Test Document".to_string()), pages: vec![page] }
    }

    #[test]
    fn emits_pdf_skeleton_with_compressed_streams() {
        let mut store = FontStore::new();
        let doc = sample_doc(&mut store);
        let bytes = write(&doc, &store);

        assert!(bytes.starts_with(b"%PDF"), "must start with %PDF header");
        let tail = &bytes[bytes.len().saturating_sub(64)..];
        assert!(tail.windows(5).any(|w| w == b"%%EOF"), "must end with %%EOF");
        assert!(
            bytes.windows(12).any(|w| w == b"/FlateDecode"),
            "streams must be deflated"
        );
        assert!(bytes.windows(11).any(|w| w == b"/Identity-H"));
        assert!(bytes.windows(10).any(|w| w == b"/FontFile2"));
        // FontFile2 must declare its uncompressed length (ISO 32000-1, T.126).
        assert!(bytes.windows(8).any(|w| w == b"/Length1"));
        assert!(bytes.windows(5).any(|w| w == b"/Link"));
        assert!(bytes.windows(4).any(|w| w == b"/URI"));
        assert!(bytes.windows(22).any(|w| w == b"(https://example.com/)"));
        // Page count and info dict.
        assert!(bytes.windows(8).any(|w| w == b"/Count 1"));
        assert!(bytes.windows(15).any(|w| w == b"(Test Document)"));
    }

    #[test]
    fn subset_renumbering_is_consistent() {
        let mut store = FontStore::new();
        let _doc = sample_doc(&mut store);

        let h = store.glyph_index(FontKey::SansRegular, 'H').unwrap();
        let i = store.glyph_index(FontKey::SansRegular, 'i').unwrap();
        let mut next = Ref::new(100);
        let plan = build_font_plan(&store, FontKey::SansRegular, 0, &mut next);

        let remapper = plan.remapper.as_ref().expect("DejaVu subsetting must work");
        // .notdef pre-mapped to 0; used gids renumbered in sorted order.
        assert_eq!(remapper.get(0), Some(0));
        let (lo, hi) = (h.min(i), h.max(i));
        assert_eq!(remapper.remapped_gids().map(u32::from).collect::<Vec<_>>(), vec![0, lo, hi]);
        assert_eq!(plan.output_gid(lo), 1);
        assert_eq!(plan.output_gid(hi), 2);
        assert_eq!(plan.output_gid(u32::MAX), 0, "unmapped gids fall back to .notdef");
        assert!(plan.base_font.starts_with("AAAAAA+"), "got {}", plan.base_font);
        assert!(plan.embed.len() < store.font_data(FontKey::SansRegular).len() / 10);

        // One consecutive /W run from cid 0; each width comes from the old gid.
        let runs = plan.width_runs(&store);
        assert_eq!(runs.len(), 1, "remapped gids must be consecutive from 0");
        let (start, widths) = &runs[0];
        assert_eq!(*start, 0);
        assert_eq!(widths.len(), 3);
        for (new_gid, width) in widths.iter().enumerate() {
            let old = u32::from(
                remapper.remapped_gids().nth(new_gid).expect("run longer than remapper"),
            );
            assert!(
                (*width as f64 - store.advance_em(FontKey::SansRegular, old) * 1000.0).abs()
                    < 0.01,
                "width of new gid {new_gid} must use old gid {old}'s advance"
            );
        }

        // Shown text bytes use the remapped ids, big-endian.
        let glyphs = vec![
            Glyph { gid: lo, x_advance: 0.0, unicode: 'x' },
            Glyph { gid: hi, x_advance: 0.0, unicode: 'y' },
        ];
        assert_eq!(glyph_run_bytes(&plan, &glyphs), vec![0, 1, 0, 2]);
    }

    #[test]
    fn tounicode_maps_remap_gids_to_utf16be() {
        let mut store = FontStore::new();
        let doc = sample_doc(&mut store);
        let bytes = write(&doc, &store);

        // Inflate every stream and find the ToUnicode CMap; 'H' encodes as
        // UTF-16BE hex 0048 and 'i' as 0069.
        let mut found = false;
        let mut pos = 0;
        while let Some(start) = find(&bytes[pos..], b"stream\n").map(|i| pos + i) {
            let body = start + b"stream\n".len();
            if let Some(end) = find(&bytes[body..], b"endstream").map(|i| body + i) {
                if let Ok(raw) = miniz_oxide::inflate::decompress_to_vec_zlib(&bytes[body..end]) {
                    if raw.windows(11).any(|w| w == b"beginbfchar")
                        || raw.windows(12).any(|w| w == b"beginbfrange")
                    {
                        assert!(raw.windows(4).any(|w| w == b"0048"), "H missing: {raw:?}");
                        assert!(raw.windows(4).any(|w| w == b"0069"), "i missing: {raw:?}");
                        found = true;
                    }
                }
                pos = end + b"endstream".len();
            } else {
                break;
            }
        }
        assert!(found, "no ToUnicode cmap stream found in output");
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
