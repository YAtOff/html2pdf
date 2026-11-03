//! Styled block tree -> laid-out pages: typography, line breaking, pagination.
//!
//! The engine is a cursor walker. [`State`] owns the current page, the y
//! cursor (top-left origin, growing downwards, in PDF points) and the
//! collapsed inter-block spacing that has not been applied yet. Blocks are
//! laid out one after another; a line whose ascent would cross the bottom
//! margin triggers a page break.
//!
//! Nested containers (list items, blockquotes) only vary the [`BlockEnv`]
//! (content rectangle + inherited text color), so every block kind shares the
//! same pagination and spacing machinery.
//!
//! Invariants worth remembering when touching this file:
//! * spacing between two blocks = max(prev.after, next.before), in points;
//!   spacing before the first block on a page is skipped entirely;
//! * `mark_used` must fire for every glyph actually placed (it drives font
//!   subsetting + ToUnicode), but must NOT fire during measurement passes;
//! * `advance()` is the only way the cursor moves, so it can feed the height
//!   accumulator used by the "don't split a list item" measurement.

use crate::fonts::FontStore;
use crate::types::{
    Block, Color, Doc, Draw, FontKey, Glyph, Inline, Page, PageSettings, Style, TableRow,
};

// ------------------------------------------------------------------ constants

const BODY_LINE_H: f64 = 1.45; // x font size, per line
const HEADING_LINE_H: f64 = 1.25;
const PRE_LINE_H: f64 = 1.35;

const BLACK: Color = (0.0, 0.0, 0.0);
const LINK_COLOR: Color = (0.05, 0.30, 0.75);
const QUOTE_TEXT: Color = (0.30, 0.30, 0.30);
const QUOTE_BAR: Color = (0.70, 0.70, 0.70);
const PRE_TEXT: Color = (0.13, 0.13, 0.13);
const PRE_BG: Color = (0.96, 0.96, 0.96);
const RULE_COLOR: Color = (0.75, 0.75, 0.75);
const GRID_COLOR: Color = (0.78, 0.78, 0.78);
const HEADER_RULE_COLOR: Color = (0.2, 0.2, 0.2);

const MONO_INLINE_SCALE: f64 = 0.92; // inline code vs. current size
const PRE_SIZE_SCALE: f64 = 0.88; // <pre> vs. base size

const LIST_LEVEL_INDENT: f64 = 18.0; // extra left offset per nesting level
const MARKER_W: f64 = 16.0; // room reserved for the list marker
const ITEM_GAP: f64 = 0.2; // between sibling list items (x base)

const PRE_PAD: f64 = 8.0; // <pre> background padding
const CELL_PAD_X: f64 = 6.0;
const CELL_PAD_Y: f64 = 4.0;
const GRID_LINE: f64 = 0.5;
const HEADER_RULE_H: f64 = 0.75;
const RULE_H: f64 = 0.75;

/// Floor for any wrapping width so hard character breaking always progresses.
const MIN_WRAP_W: f64 = 1.0;

// ------------------------------------------------------- small pure helpers

/// Font size multiplier per heading level (1..=6).
pub(crate) fn heading_scale(level: u8) -> f64 {
    match level {
        1 => 2.0,
        2 => 1.6,
        3 => 1.3,
        4 => 1.15,
        5 => 1.0,
        6 => 0.95,
        _ => 1.0, // dom guarantees 1..=6; anything else degrades to body size
    }
}

/// (before, after) spacing around a block, in units of `base_font_size`.
pub(crate) fn block_spacing(b: &Block) -> (f64, f64) {
    match b {
        Block::Paragraph { .. } => (0.55, 0.55),
        Block::Heading { level: 1, .. } => (1.0, 0.4),
        Block::Heading { level: 2, .. } => (0.9, 0.35),
        Block::Heading { level: 3, .. } => (0.8, 0.3),
        Block::Heading { .. } => (0.7, 0.3), // h4, h5, h6
        Block::List { .. } => (0.5, 0.5),
        Block::Preformatted { .. } => (0.5, 0.5),
        Block::Blockquote { .. } => (0.5, 0.5),
        Block::Rule => (0.7, 0.7),
        Block::Table { .. } => (0.6, 0.6),
    }
}

/// Marker label for list item `index` (0-based); ordered lists count from `start`.
pub(crate) fn marker_text(ordered: bool, start: i64, index: usize) -> String {
    if ordered {
        format!("{}.", start + index as i64)
    } else {
        "\u{2022}".to_string() // bullet
    }
}

// ------------------------------------------------------------ shaped inline

/// Everything a placed glyph needs, derived from a `Style` + block context.
#[derive(Clone)]
struct RunStyle {
    font: FontKey,
    size: f64,
    color: Color,
    underline: bool,
    link: Option<String>,
}

/// Block-level styling context: dominant font size, forced boldness
/// (headings, table headers) and inherited text color (blockquote).
#[derive(Clone, Copy)]
struct StyleCtx {
    size: f64,
    force_bold: bool,
    color: Color,
}

/// Resolve an inline style against the block context: headings force bold,
/// mono runs shrink to 0.92x, links recolor + underline.
fn resolve_style(style: &Style, ctx: &StyleCtx) -> RunStyle {
    let mut s = style.clone();
    if ctx.force_bold {
        s.bold = true; // headings / table header cells; mono is kept
    }
    let size = if s.mono { ctx.size * MONO_INLINE_SCALE } else { ctx.size };
    let underline = s.underline || s.link.is_some();
    let color = if s.link.is_some() { LINK_COLOR } else { ctx.color };
    RunStyle { font: FontKey::from_style(&s), size, color, underline, link: s.link.clone() }
}

/// One character mapped into a concrete face, with its advance in points.
#[derive(Clone)]
struct ShapedChar {
    ch: char,
    gid: u32,
    adv: f64,
    rs: RunStyle,
}

/// Map a char to a glyph, falling back to U+FFFD, then to skipping the char.
fn shape(store: &FontStore, font: FontKey, size: f64, ch: char, rs: &RunStyle) -> Option<ShapedChar> {
    // Note: the *original* char is kept for ToUnicode even when the glyph is
    // the replacement glyph, so the mapping stays readable.
    let gid = store
        .glyph_index(font, ch)
        .or_else(|| store.glyph_index(font, '\u{FFFD}'))?;
    let adv = store.advance_em(font, gid) * size;
    Some(ShapedChar { ch, gid, adv, rs: rs.clone() })
}

/// Whitespace-collapsed inline stream cut into wrapping tokens.
/// A "word" glues adjacent non-space runs (even across style changes);
/// U+00A0 never splits a word because we only split on U+0020.
enum Tok {
    Word { chars: Vec<ShapedChar>, width: f64 },
    Space { ch: ShapedChar },
    Break,
}

fn tokenize(inlines: &[Inline], ctx: &StyleCtx, store: &FontStore) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut word: Vec<ShapedChar> = Vec::new();
    let mut width = 0.0;

    fn flush_word(toks: &mut Vec<Tok>, word: &mut Vec<ShapedChar>, width: &mut f64) {
        if !word.is_empty() {
            toks.push(Tok::Word { chars: std::mem::take(word), width: *width });
            *width = 0.0;
        }
    }

    for inl in inlines {
        match inl {
            Inline::Text { text, style } => {
                let rs = resolve_style(style, ctx);
                for ch in text.chars() {
                    if ch == ' ' {
                        flush_word(&mut toks, &mut word, &mut width);
                        if let Some(sp) = shape(store, rs.font, rs.size, ' ', &rs) {
                            toks.push(Tok::Space { ch: sp });
                        }
                    } else if let Some(sc) = shape(store, rs.font, rs.size, ch, &rs) {
                        width += sc.adv;
                        word.push(sc);
                    }
                }
            }
            Inline::Break => {
                flush_word(&mut toks, &mut word, &mut width);
                toks.push(Tok::Break);
            }
        }
    }
    flush_word(&mut toks, &mut word, &mut width);
    toks
}

// ------------------------------------------------------------- line breaking

/// A glyph with its x offset from the start of the line.
struct PlacedChar {
    sc: ShapedChar,
    x: f64,
}

/// One laid-out line: positioned glyphs, total width, max ascent (points).
struct LaidLine {
    chars: Vec<PlacedChar>,
    #[allow(dead_code)] // kept for tests / debugging
    width: f64,
    ascent: f64,
}

fn ascent_em(store: &FontStore, font: FontKey) -> f64 {
    store.line_metrics_em(font).0
}

fn ascent_pt(store: &FontStore, font: FontKey, size: f64) -> f64 {
    ascent_em(store, font) * size
}

/// Close the current line. Trailing spaces are dropped (width purposes);
/// `explicit` lines (from `<br>` / preserved pre lines) keep their height
/// even when empty, wrap-induced flushes of an empty line are discarded.
fn finish_line(cur: &mut Vec<PlacedChar>, lines: &mut Vec<LaidLine>, explicit: bool, store: &FontStore) {
    while matches!(cur.last(), Some(p) if p.sc.ch == ' ') {
        cur.pop();
    }
    if cur.is_empty() && !explicit {
        return;
    }
    let width = cur.last().map_or(0.0, |p| p.x + p.sc.adv);
    let ascent = cur.iter().fold(0.0f64, |m, p| m.max(p.sc.rs.size * ascent_em(store, p.sc.rs.font)));
    lines.push(LaidLine { chars: std::mem::take(cur), width, ascent });
}

/// Append chars, flushing a line whenever the width would overflow
/// (character-level hard breaking). Always places at least one char per
/// line, so it makes progress for any `max_w > 0`.
fn push_hard(
    chars: &[ShapedChar],
    max_w: f64,
    cur: &mut Vec<PlacedChar>,
    cur_w: &mut f64,
    lines: &mut Vec<LaidLine>,
    store: &FontStore,
) {
    for sc in chars {
        if !cur.is_empty() && *cur_w + sc.adv > max_w {
            finish_line(cur, lines, false, store);
            *cur_w = 0.0;
        }
        let adv = sc.adv;
        cur.push(PlacedChar { sc: sc.clone(), x: *cur_w });
        *cur_w += adv;
    }
}

/// Greedy line breaking over tokens.
/// A word fits while `line_width + (pending space? space_width : 0) + word_width
/// <= max_w`; a word that alone overflows gets its own line(s), broken by
/// characters via `push_hard`. The pending space is suppressed at line starts.
fn wrap_tokens(toks: &[Tok], max_w: f64, store: &FontStore) -> Vec<LaidLine> {
    let mut lines: Vec<LaidLine> = Vec::new();
    let mut cur: Vec<PlacedChar> = Vec::new();
    let mut cur_w = 0.0;
    let mut pending_space: Option<ShapedChar> = None;

    for tok in toks {
        match tok {
            Tok::Space { ch } => {
                // Leading spaces (empty line) are dropped outright.
                if !cur.is_empty() {
                    pending_space = Some(ch.clone());
                }
            }
            Tok::Break => {
                pending_space = None;
                finish_line(&mut cur, &mut lines, true, store);
                cur_w = 0.0;
            }
            Tok::Word { chars, width } => {
                let sp_w = pending_space.as_ref().map_or(0.0, |s| s.adv);
                if !cur.is_empty() && cur_w + sp_w + *width <= max_w {
                    if let Some(sp) = pending_space.take() {
                        let adv = sp.adv;
                        cur.push(PlacedChar { sc: sp, x: cur_w });
                        cur_w += adv;
                    }
                    for sc in chars {
                        let adv = sc.adv;
                        cur.push(PlacedChar { sc: sc.clone(), x: cur_w });
                        cur_w += adv;
                    }
                } else {
                    pending_space = None; // suppressed at the start of the next line
                    finish_line(&mut cur, &mut lines, false, store);
                    cur_w = 0.0;
                    if *width <= max_w {
                        for sc in chars {
                            let adv = sc.adv;
                            cur.push(PlacedChar { sc: sc.clone(), x: cur_w });
                            cur_w += adv;
                        }
                    } else {
                        push_hard(chars, max_w, &mut cur, &mut cur_w, &mut lines, store);
                    }
                }
            }
        }
    }
    finish_line(&mut cur, &mut lines, false, store);
    lines
}

/// Single-line width and longest word width of a token stream (table columns).
fn natural_extents(toks: &[Tok]) -> (f64, f64) {
    let mut natural = 0.0f64;
    let mut min_w = 0.0f64;
    let mut cur = 0.0f64;
    let mut space_w: Option<f64> = None;
    for t in toks {
        match t {
            Tok::Space { ch } => {
                if cur > 0.0 {
                    space_w = Some(ch.adv);
                }
            }
            Tok::Word { width, .. } => {
                if let Some(w) = space_w.take() {
                    cur += w;
                }
                cur += width;
                min_w = min_w.max(*width);
            }
            Tok::Break => {
                natural = natural.max(cur);
                cur = 0.0;
                space_w = None;
            }
        }
    }
    (natural.max(cur), min_w)
}

/// Distribute `avail` points over table columns given per-column natural and
/// minimum widths (content widths; horizontal padding is added by callers).
///
/// * everything fits -> scale naturals proportionally to fill exactly;
/// * too wide -> shrink proportionally toward the mins (never below);
/// * even the mins overflow -> use the mins (rare, overflow accepted).
pub(crate) fn solve_column_widths(nat: &[f64], min: &[f64], avail: f64) -> Vec<f64> {
    let n = nat.len();
    let pad_total = 2.0 * CELL_PAD_X * n as f64;
    let sum_nat: f64 = nat.iter().sum();
    let sum_min: f64 = min.iter().sum();
    if sum_nat + pad_total <= avail {
        let target = (avail - pad_total).max(0.0);
        if sum_nat > 0.0 {
            nat.iter().map(|&w| w * target / sum_nat).collect()
        } else {
            vec![target / n as f64; n] // degenerate all-empty table
        }
    } else if sum_min + pad_total >= avail {
        min.to_vec()
    } else {
        let excess = sum_nat + pad_total - avail;
        let reducible = sum_nat - sum_min;
        nat.iter()
            .zip(min.iter())
            .map(|(&n_, &m_)| n_ - excess * (n_ - m_) / reducible)
            .collect()
    }
}

// ------------------------------------------------------------------- state

/// Content rectangle + inherited color for the block being laid out.
#[derive(Clone, Copy)]
struct BlockEnv {
    x: f64,
    w: f64,
    color: Color,
}

/// Pending list marker: rendered at the baseline of the item's first line.
#[derive(Clone)]
struct Marker {
    text: String,
    x: f64,
    color: Color,
}

/// One page-fragment of an active blockquote bar.
struct BarSeg {
    page: usize,
    y0: f64,
    y1: f64,
}

struct Bar {
    x: f64,
    segs: Vec<BarSeg>,
}

/// Everything that must survive a measurement pass rollback.
struct Snapshot {
    pages_len: usize,
    items_len: usize,
    links_len: usize,
    cursor: f64,
    pending: f64,
    at_page_top: bool,
    acc: f64,
    marker: Option<Marker>,
}

struct PreFrag {
    trace: (usize, usize), // (page, item index) where the fragment's text starts
    top: f64,
    bottom: f64,
    first: bool, // fragment starts the block (gets top padding)
}

struct TableRowFrag {
    ri: usize,
    y0: f64,
    h: f64,
}

struct TableFrag {
    page: usize,
    insert_idx: usize,
    rows: Vec<TableRowFrag>,
}

struct TableGeom {
    x: f64,
    right: f64,
    col_x: Vec<f64>, // left edge of each column box (box = content + 2*CELL_PAD_X)
}

/// The layout cursor + page accumulator.
struct State<'a> {
    s: PageSettings,
    store: &'a mut FontStore,
    pages: Vec<Page>,
    cursor: f64,
    /// Collapsed spacing waiting to be applied before the next block.
    pending: f64,
    /// True while nothing has been placed on the current page (spacing is
    /// skipped and page breaks are disallowed to guarantee progress).
    at_page_top: bool,
    bars: Vec<Bar>,
    pending_marker: Option<Marker>,
    /// Measurement mode: run the full algorithm but emit nothing and skip
    /// `mark_used`; used to measure list items for keep-together.
    measure: bool,
    /// Accumulated placed height since the last reset (see `advance`).
    acc: f64,
    /// (page, item index) of the most recently placed line, so decoration
    /// rects can be *inserted before* the line's text items.
    last_trace: (usize, usize),
}

impl<'a> State<'a> {
    fn new(s: PageSettings, store: &'a mut FontStore) -> State<'a> {
        let pages = vec![Page { width: s.width, height: s.height, items: vec![], links: vec![] }];
        State {
            s,
            store,
            pages,
            cursor: s.margin,
            pending: 0.0,
            at_page_top: true,
            bars: vec![],
            pending_marker: None,
            measure: false,
            acc: 0.0,
            last_trace: (0, 0),
        }
    }

    fn page_idx(&self) -> usize {
        self.pages.len() - 1
    }

    /// Y coordinate content must not cross (bottom margin line).
    fn bottom(&self) -> f64 {
        self.s.height - self.s.margin
    }

    fn content_height(&self) -> f64 {
        self.bottom() - self.s.margin
    }

    fn new_page(&mut self) {
        self.pages
            .push(Page { width: self.s.width, height: self.s.height, items: vec![], links: vec![] });
        self.cursor = self.s.margin;
        self.at_page_top = true;
        self.pending = 0.0; // spacing does not carry across a page break
    }

    /// The only sanctioned way to move the cursor; feeds `acc`.
    fn advance(&mut self, dy: f64) {
        self.cursor += dy;
        self.acc += dy;
        self.at_page_top = false;
    }

    fn push(&mut self, d: Draw) {
        if !self.measure {
            self.pages.last_mut().unwrap().items.push(d);
        }
    }

    fn push_link(&mut self, l: (f64, f64, f64, f64, String)) {
        if !self.measure {
            self.pages.last_mut().unwrap().links.push(l);
        }
    }

    /// Insert a decoration rect before already-pushed items of a fragment.
    fn insert_draw(&mut self, page: usize, idx: usize, d: Draw) {
        if !self.measure {
            self.pages[page].items.insert(idx, d);
        }
    }

    /// Append a decoration rect to a specific page (blockquote bars: they
    /// never overlap text, and the quote may span several pages).
    fn push_draw_at(&mut self, page: usize, d: Draw) {
        if !self.measure {
            self.pages[page].items.push(d);
        }
    }

    fn mark(&mut self, font: FontKey, gid: u32, ch: char) {
        if !self.measure {
            self.store.mark_used(font, gid, ch);
        }
    }

    fn snapshot(&self) -> Snapshot {
        let last = self.pages.last().unwrap();
        Snapshot {
            pages_len: self.pages.len(),
            items_len: last.items.len(),
            links_len: last.links.len(),
            cursor: self.cursor,
            pending: self.pending,
            at_page_top: self.at_page_top,
            acc: self.acc,
            marker: self.pending_marker.clone(),
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.pages.truncate(s.pages_len);
        let last = self.pages.last_mut().unwrap();
        last.items.truncate(s.items_len);
        last.links.truncate(s.links_len);
        self.cursor = s.cursor;
        self.pending = s.pending;
        self.at_page_top = s.at_page_top;
        self.acc = s.acc;
        self.pending_marker = s.marker;
    }

    /// Apply the collapsed gap before a block. Skipped at the top of a page
    /// ("skip spacing before the FIRST block on a page").
    fn begin_block(&mut self, before: f64) {
        if !self.at_page_top {
            let gap = self.pending.max(before);
            if gap > 0.0 {
                self.advance(gap);
            }
        }
        self.pending = 0.0;
    }

    /// Fold a block's "after" spacing into the pending gap.
    fn end_block(&mut self, after: f64) {
        self.pending = self.pending.max(after);
    }

    /// Extend all active blockquote bars to cover [y0, y1] on the current page.
    fn note_bars(&mut self, y0: f64, y1: f64) {
        if self.measure {
            return;
        }
        let page = self.page_idx();
        for b in &mut self.bars {
            match b.segs.last_mut() {
                Some(seg) if seg.page == page => seg.y1 = y1,
                _ => b.segs.push(BarSeg { page, y0, y1 }),
            }
        }
    }

    /// Render the pending list marker (if any) at this baseline.
    /// Marker font: body face, regular weight, base size.
    fn flush_marker(&mut self, baseline: f64) {
        let Some(m) = self.pending_marker.take() else { return };
        let font = FontKey::SansRegular;
        let size = self.s.base_font_size;
        let rs = RunStyle { font, size, color: m.color, underline: false, link: None };
        let chars: Vec<ShapedChar> = m
            .text
            .chars()
            .filter_map(|ch| shape(&*self.store, font, size, ch, &rs))
            .collect();
        if chars.is_empty() {
            return;
        }
        let glyphs = chars
            .iter()
            .map(|sc| Glyph { gid: sc.gid, x_advance: sc.adv, unicode: sc.ch })
            .collect();
        self.push(Draw::Text { x: m.x, y: baseline, size, font, color: m.color, glyphs });
        for sc in chars {
            self.mark(font, sc.gid, sc.ch);
        }
    }

    /// Group a line's glyphs into `Draw::Text` runs (consecutive glyphs with
    /// identical font/size/color merge; x accumulates from advances), then add
    /// underline rects and link rects for each contiguous styled word run.
    fn emit_chars(&mut self, x: f64, baseline: f64, chars: &[PlacedChar]) {
        let mut i = 0;
        while i < chars.len() {
            let mut j = i;
            while j + 1 < chars.len() && same_group(&chars[j + 1].sc.rs, &chars[i].sc.rs) {
                j += 1;
            }
            let rs = &chars[i].sc.rs;
            let glyphs = chars[i..=j]
                .iter()
                .map(|p| Glyph { gid: p.sc.gid, x_advance: p.sc.adv, unicode: p.sc.ch })
                .collect();
            self.push(Draw::Text {
                x: x + chars[i].x,
                y: baseline,
                size: rs.size,
                font: rs.font,
                color: rs.color,
                glyphs,
            });
            for p in &chars[i..=j] {
                self.mark(rs.font, p.sc.gid, p.sc.ch);
            }
            i = j + 1;
        }
        self.emit_spans(x, baseline, chars);
    }

    /// Underline rects (filled, under the baseline) and clickable link rects
    /// for maximal same-style spans on a line.
    fn emit_spans(&mut self, x: f64, baseline: f64, chars: &[PlacedChar]) {
        // Underlines: links are always underlined, plus explicit <u> runs.
        let mut i = 0;
        while i < chars.len() {
            if !chars[i].sc.rs.underline {
                i += 1;
                continue;
            }
            let mut j = i;
            let mut size = chars[i].sc.rs.size;
            while j + 1 < chars.len() && chars[j + 1].sc.rs.underline {
                j += 1;
                size = size.max(chars[j].sc.rs.size);
            }
            let x0 = x + chars[i].x;
            let x1 = x + chars[j].x + chars[j].sc.adv;
            self.push(Draw::Rect {
                x: x0,
                y: baseline + 0.12 * size,
                w: (x1 - x0).max(0.0),
                h: 0.5f64.max(size / 14.0),
                color: chars[i].sc.rs.color,
            });
            i = j + 1;
        }
        // Links: one rect per contiguous run sharing the same target.
        let mut i = 0;
        while i < chars.len() {
            let target = chars[i].sc.rs.link.clone();
            if target.is_none() {
                i += 1;
                continue;
            }
            let mut j = i;
            let mut size = chars[i].sc.rs.size;
            while j + 1 < chars.len() && chars[j + 1].sc.rs.link == target {
                j += 1;
                size = size.max(chars[j].sc.rs.size);
            }
            let x0 = x + chars[i].x;
            let x1 = x + chars[j].x + chars[j].sc.adv;
            self.push_link((x0, baseline - 0.85 * size, (x1 - x0).max(0.0), 1.05 * size, target.unwrap()));
            i = j + 1;
        }
    }

    /// Place one line: paginate, emit runs, flush a pending marker, extend
    /// active bars, advance the cursor by the line height.
    fn place_line(&mut self, x: f64, lh: f64, fallback_ascent: f64, line: &LaidLine) {
        let ascent = if line.ascent > 0.0 { line.ascent } else { fallback_ascent };
        if !self.at_page_top && self.cursor + ascent > self.bottom() {
            self.new_page();
        }
        let top = self.cursor;
        let baseline = top + ascent;
        // Remember where this line's items start so decoration rects
        // (pre backgrounds, table grids) can be inserted in front of them.
        let idx = if self.measure { 0 } else { self.pages.last().unwrap().items.len() };
        self.last_trace = (self.page_idx(), idx);
        if !line.chars.is_empty() {
            self.emit_chars(x, baseline, &line.chars);
            self.flush_marker(baseline); // waits for the first *visible* line
        }
        self.note_bars(top, top + lh);
        self.advance(lh);
    }
}

fn same_group(a: &RunStyle, b: &RunStyle) -> bool {
    a.font == b.font && a.size == b.size && a.color == b.color
}

// -------------------------------------------------------------- block layer

impl<'a> State<'a> {
    /// Lay a sequence of blocks, collapsing spacing between them.
    /// `in_item` marks list-item content: a nested list as a direct child
    /// gets one LIST_LEVEL_INDENT step (18pt) relative to the *parent list*
    /// edge, i.e. 2pt past the item content edge (which is +16pt).
    fn lay_blocks(&mut self, blocks: &[Block], env: &BlockEnv, in_item: bool) {
        for b in blocks {
            match b {
                Block::List { .. } if in_item => {
                    let step = LIST_LEVEL_INDENT - MARKER_W; // 18 - 16
                    let shifted = BlockEnv {
                        x: env.x + step,
                        w: (env.w - step).max(MIN_WRAP_W),
                        color: env.color,
                    };
                    self.lay_block(b, &shifted);
                }
                _ => self.lay_block(b, env),
            }
        }
    }

    fn lay_block(&mut self, b: &Block, env: &BlockEnv) {
        let (before, after) = block_spacing(b);
        let base = self.s.base_font_size;
        match b {
            Block::Paragraph { inlines } => {
                let ctx = StyleCtx { size: base, force_bold: false, color: env.color };
                self.lay_text_block(inlines, env, ctx, BODY_LINE_H * base, before, after, false);
            }
            Block::Heading { level, inlines } => {
                let size = base * heading_scale(*level);
                let ctx = StyleCtx { size, force_bold: true, color: env.color };
                self.lay_text_block(
                    inlines,
                    env,
                    ctx,
                    HEADING_LINE_H * size,
                    before,
                    after,
                    true, // keep with at least one following line
                );
            }
            Block::List { ordered, start, items } => self.lay_list(*ordered, *start, items, env),
            Block::Preformatted { text } => self.lay_pre(text, env, before, after),
            Block::Blockquote { children } => self.lay_blockquote(children, env, before, after),
            Block::Rule => self.lay_rule(env, before, after),
            Block::Table { rows, header_rows } => self.lay_table(rows, *header_rows, env, before, after),
        }
    }

    /// Shared body of Paragraph and Heading. Empty content contributes no
    /// height but still folds its spacing into the collapse chain.
    fn lay_text_block(
        &mut self,
        inlines: &[Inline],
        env: &BlockEnv,
        ctx: StyleCtx,
        lh: f64,
        before: f64,
        after: f64,
        keep_with_next: bool,
    ) {
        let base = self.s.base_font_size;
        let toks = tokenize(inlines, &ctx, &*self.store);
        let lines = wrap_tokens(&toks, env.w.max(MIN_WRAP_W), &*self.store);
        if lines.is_empty() {
            self.end_block(after * base);
            return;
        }
        self.begin_block(before * base);
        if keep_with_next && !self.at_page_top {
            // A heading must not be the last thing on a page: require room
            // for one more body line below it, else push to the next page.
            let body_ascent = ascent_pt(&*self.store, FontKey::SansRegular, base);
            if self.cursor + lines.len() as f64 * lh + body_ascent > self.bottom() {
                self.new_page();
            }
        }
        let fallback = ascent_pt(&*self.store, FontKey::SansRegular, ctx.size);
        for line in &lines {
            self.place_line(env.x, lh, fallback, line);
        }
        self.end_block(after * base);
    }

    fn lay_pre(&mut self, text: &str, env: &BlockEnv, before: f64, after: f64) {
        let base = self.s.base_font_size;
        let size = base * PRE_SIZE_SCALE;
        let lh = PRE_LINE_H * size;
        if text.is_empty() {
            self.end_block(after * base);
            return;
        }
        let font = FontKey::MonoRegular;
        let avail = env.w.max(MIN_WRAP_W);
        let rs = RunStyle { font, size, color: PRE_TEXT, underline: false, link: None };
        // Pre keeps raw lines; only hard overflow breaks a line further.
        let mut lines: Vec<LaidLine> = Vec::new();
        for src in text.split('\n') {
            let shaped: Vec<ShapedChar> =
                src.chars().filter_map(|ch| shape(&*self.store, font, size, ch, &rs)).collect();
            let mut cur: Vec<PlacedChar> = Vec::new();
            let mut cur_w = 0.0;
            push_hard(&shaped, avail, &mut cur, &mut cur_w, &mut lines, &*self.store);
            finish_line(&mut cur, &mut lines, true, &*self.store);
        }
        self.begin_block(before * base);
        let fallback = ascent_pt(&*self.store, font, size);
        let mut frag: Option<PreFrag> = None;
        for line in &lines {
            let page_before = self.page_idx();
            self.place_line(env.x, lh, fallback, line);
            let moved_page = self.page_idx() != page_before;
            match frag.take() {
                None => {
                    frag = Some(PreFrag {
                        trace: self.last_trace,
                        top: self.cursor - lh,
                        bottom: self.cursor,
                        first: true,
                    });
                }
                Some(mut f) if !moved_page => {
                    f.bottom = self.cursor;
                    frag = Some(f);
                }
                Some(f) => {
                    self.close_pre_fragment(&f, env, false);
                    frag = Some(PreFrag {
                        trace: self.last_trace,
                        top: self.cursor - lh,
                        bottom: self.cursor,
                        first: false,
                    });
                }
            }
        }
        if let Some(f) = frag {
            self.close_pre_fragment(&f, env, true);
        }
        self.end_block(after * base);
    }

    /// Background rect for one page-fragment of a <pre> block: 8pt padding
    /// on the block's outer edges, inserted *before* the fragment's text.
    fn close_pre_fragment(&mut self, f: &PreFrag, env: &BlockEnv, last: bool) {
        let y = f.top - if f.first { PRE_PAD } else { 0.0 };
        let h = f.bottom - f.top
            + if f.first { PRE_PAD } else { 0.0 }
            + if last { PRE_PAD } else { 0.0 };
        self.insert_draw(
            f.trace.0,
            f.trace.1,
            Draw::Rect { x: env.x - PRE_PAD, y, w: env.w + 2.0 * PRE_PAD, h, color: PRE_BG },
        );
    }

    fn lay_list(&mut self, ordered: bool, start: i64, items: &[Vec<Block>], env: &BlockEnv) {
        let base = self.s.base_font_size;
        if items.is_empty() {
            // Zero-height like an empty paragraph: only spacing collapse remains.
            self.end_block(0.5 * base);
            return;
        }
        self.begin_block(0.5 * base);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.pending = self.pending.max(ITEM_GAP * base);
            }
            let content = BlockEnv {
                x: env.x + MARKER_W,
                w: (env.w - MARKER_W).max(MIN_WRAP_W),
                color: env.color,
            };
            // Keep-together: if the item fits on a single page, don't split
            // it — measure it, and push to a fresh page when needed.
            let h = self.measure_item(item, &content);
            if h > 0.0 && h <= self.content_height() && !self.at_page_top {
                let first_before = item.first().map_or(0.0, |b| block_spacing(b).0 * base);
                let gap = self.pending.max(first_before);
                if self.cursor + gap + h > self.bottom() {
                    self.new_page();
                }
            }
            self.pending_marker =
                Some(Marker { text: marker_text(ordered, start, i), x: env.x, color: env.color });
            self.lay_blocks(item, &content, true);
            self.pending_marker = None; // dropped if the item rendered nothing
        }
        self.end_block(0.5 * base);
    }

    /// Height of `item` when laid out from the top of a page (measurement
    /// pass: full algorithm, no emission, no mark_used; state is rolled back).
    fn measure_item(&mut self, item: &[Block], env: &BlockEnv) -> f64 {
        let snap = self.snapshot();
        self.measure = true;
        self.cursor = self.s.margin;
        self.at_page_top = true;
        self.pending = 0.0;
        self.acc = 0.0;
        self.lay_blocks(item, env, true);
        let h = self.acc;
        self.restore(snap);
        self.measure = false;
        h
    }

    fn lay_blockquote(&mut self, children: &[Block], env: &BlockEnv, before: f64, after: f64) {
        let base = self.s.base_font_size;
        if children.is_empty() {
            self.end_block(after * base);
            return;
        }
        self.begin_block(before * base);
        self.bars.push(Bar { x: env.x, segs: vec![] });
        let inner = BlockEnv {
            x: env.x + 18.0,
            w: (env.w - 18.0).max(MIN_WRAP_W),
            color: QUOTE_TEXT,
        };
        self.lay_blocks(children, &inner, false);
        let bar = self.bars.pop().unwrap();
        // Vertical bar at the block's left edge, first laid line to last.
        for seg in bar.segs {
            self.push_draw_at(
                seg.page,
                Draw::Rect {
                    x: bar.x,
                    y: seg.y0,
                    w: 3.0,
                    h: (seg.y1 - seg.y0).max(0.0),
                    color: QUOTE_BAR,
                },
            );
        }
        self.end_block(after * base);
    }

    fn lay_rule(&mut self, env: &BlockEnv, before: f64, after: f64) {
        let base = self.s.base_font_size;
        self.begin_block(before * base);
        if !self.at_page_top && self.cursor + RULE_H > self.bottom() {
            self.new_page();
        }
        let y = self.cursor;
        self.push(Draw::Rect { x: env.x, y, w: env.w, h: RULE_H, color: RULE_COLOR });
        self.note_bars(y, y + RULE_H);
        self.advance(RULE_H);
        self.end_block(after * base);
    }

    fn lay_table(&mut self, rows: &[TableRow], header_rows: usize, env: &BlockEnv, before: f64, after: f64) {
        let base = self.s.base_font_size;
        let lh = BODY_LINE_H * base;
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 {
            // Zero-height like an empty paragraph: only spacing collapse remains.
            self.end_block(after * base);
            return;
        }
        self.begin_block(before * base);
        let ctx_body = StyleCtx { size: base, force_bold: false, color: env.color };
        let ctx_head = StyleCtx { size: base, force_bold: true, color: env.color };

        // Column sizing from natural (single-line) and min (longest word) widths.
        let mut nat = vec![0.0f64; ncols];
        let mut minw = vec![0.0f64; ncols];
        for (ri, row) in rows.iter().enumerate() {
            let ctx = if ri < header_rows { &ctx_head } else { &ctx_body };
            for (ci, cell) in row.iter().enumerate() {
                let (n, m) = natural_extents(&tokenize(cell, ctx, &*self.store));
                nat[ci] = nat[ci].max(n);
                minw[ci] = minw[ci].max(m);
            }
        }
        let widths = solve_column_widths(&nat, &minw, env.w);
        let mut geom = TableGeom { x: env.x, right: env.x, col_x: Vec::with_capacity(ncols) };
        for w in &widths {
            geom.col_x.push(geom.right);
            geom.right += w + 2.0 * CELL_PAD_X;
        }

        let total_rows = rows.len();
        let mut frag: Option<TableFrag> = None;
        for (ri, row) in rows.iter().enumerate() {
            let ctx = if ri < header_rows { &ctx_head } else { &ctx_body };
            // Wrap every cell at its column content width.
            let cell_lines: Vec<Vec<LaidLine>> = (0..ncols)
                .map(|ci| match row.get(ci) {
                    Some(cell) => wrap_tokens(
                        &tokenize(cell, ctx, &*self.store),
                        widths[ci].max(MIN_WRAP_W),
                        &*self.store,
                    ),
                    None => vec![],
                })
                .collect();
            let inner_h = cell_lines
                .iter()
                .map(|ls| ls.len() as f64 * lh)
                .fold(0.0, f64::max);
            let row_h = inner_h + 2.0 * CELL_PAD_Y;

            // Tables split at row boundaries only.
            if !self.at_page_top && self.cursor + row_h > self.bottom() {
                if let Some(f) = frag.take() {
                    self.close_table_fragment(&f, &geom, header_rows, total_rows);
                }
                self.new_page();
            }
            if frag.is_none() {
                let idx = if self.measure { 0 } else { self.pages.last().unwrap().items.len() };
                frag = Some(TableFrag { page: self.page_idx(), insert_idx: idx, rows: vec![] });
            }
            let y0 = self.cursor;
            for (ci, ls) in cell_lines.iter().enumerate() {
                let tx = geom.col_x[ci] + CELL_PAD_X;
                for (k, line) in ls.iter().enumerate() {
                    let ascent = if line.ascent > 0.0 {
                        line.ascent
                    } else {
                        ascent_pt(&*self.store, FontKey::SansRegular, base)
                    };
                    let baseline = y0 + CELL_PAD_Y + k as f64 * lh + ascent;
                    if !line.chars.is_empty() {
                        self.emit_chars(tx, baseline, &line.chars);
                        self.flush_marker(baseline); // table as first block of an item
                    }
                }
            }
            self.note_bars(y0, y0 + row_h);
            self.advance(row_h);
            frag.as_mut().unwrap().rows.push(TableRowFrag { ri, y0, h: row_h });
        }
        if let Some(f) = frag {
            self.close_table_fragment(&f, &geom, header_rows, total_rows);
        }
        self.end_block(after * base);
    }

    /// Grid lines for one page-fragment of a table, inserted before the
    /// fragment's cell text: horizontals between adjacent rows (0.75pt dark
    /// under the header, 0.5pt light elsewhere) and verticals between columns.
    fn close_table_fragment(
        &mut self,
        f: &TableFrag,
        geom: &TableGeom,
        header_rows: usize,
        total_rows: usize,
    ) {
        if f.rows.is_empty() {
            return;
        }
        let bottom = f.rows.last().map_or(f.rows[0].y0, |r| r.y0 + r.h);
        let top = f.rows[0].y0;
        let mut rects: Vec<Draw> = Vec::new();
        for pair in f.rows.windows(2) {
            let a = &pair[0];
            let y = a.y0 + a.h;
            let (h, c) = if a.ri < header_rows {
                (HEADER_RULE_H, HEADER_RULE_COLOR)
            } else {
                (GRID_LINE, GRID_COLOR)
            };
            rects.push(Draw::Rect { x: geom.x, y: y - h / 2.0, w: geom.right - geom.x, h, color: c });
        }
        // Header ends the fragment (page break right after it) but the table
        // continues: still draw the header rule.
        if let Some(last) = f.rows.last() {
            if last.ri < header_rows && last.ri + 1 < total_rows {
                rects.push(Draw::Rect {
                    x: geom.x,
                    y: last.y0 + last.h - HEADER_RULE_H / 2.0,
                    w: geom.right - geom.x,
                    h: HEADER_RULE_H,
                    color: HEADER_RULE_COLOR,
                });
            }
        }
        for &vx in &geom.col_x[1..] {
            rects.push(Draw::Rect {
                x: vx - GRID_LINE / 2.0,
                y: top,
                w: GRID_LINE,
                h: (bottom - top).max(0.0),
                color: GRID_COLOR,
            });
        }
        for (k, r) in rects.into_iter().enumerate() {
            self.insert_draw(f.page, f.insert_idx + k, r);
        }
    }
}

// -------------------------------------------------------------------- entry

/// Lay blocks out onto pages. Glyphs used along the way are recorded in
/// `store` (for subsetting + ToUnicode).
pub fn layout(blocks: &[Block], settings: PageSettings, store: &mut FontStore) -> Doc {
    let blank = || Page { width: settings.width, height: settings.height, items: vec![], links: vec![] };
    if settings.content_width() <= 0.0 || settings.height - 2.0 * settings.margin <= 0.0 {
        return Doc { title: None, pages: vec![blank()] };
    }
    let mut st = State::new(settings, store);
    let env = BlockEnv { x: settings.margin, w: settings.content_width(), color: BLACK };
    st.lay_blocks(blocks, &env, false);
    Doc { title: None, pages: st.pages }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_rs() -> RunStyle {
        RunStyle { font: FontKey::SansRegular, size: 10.0, color: BLACK, underline: false, link: None }
    }

    /// Word token where every char has the same advance (width math stays exact).
    fn fake_word(s: &str, adv: f64) -> Tok {
        let mut chars = Vec::new();
        let mut width = 0.0;
        for ch in s.chars() {
            width += adv;
            chars.push(ShapedChar { ch, gid: 1, adv, rs: fake_rs() });
        }
        Tok::Word { chars, width }
    }

    fn fake_space(adv: f64) -> Tok {
        Tok::Space { ch: ShapedChar { ch: ' ', gid: 1, adv, rs: fake_rs() } }
    }

    fn settings() -> PageSettings {
        PageSettings { width: 595.28, height: 841.89, margin: 56.7, base_font_size: 11.0 }
    }

    #[test]
    fn heading_scale_matches_spec() {
        let want = [2.0, 1.6, 1.3, 1.15, 1.0, 0.95];
        for (i, &w) in want.iter().enumerate() {
            assert!((heading_scale(i as u8 + 1) - w).abs() < 1e-12);
        }
    }

    #[test]
    fn block_spacing_matches_spec() {
        let p = Block::Paragraph { inlines: vec![] };
        assert_eq!(block_spacing(&p), (0.55, 0.55));
        let h1 = Block::Heading { level: 1, inlines: vec![] };
        assert_eq!(block_spacing(&h1), (1.0, 0.4));
        let h2 = Block::Heading { level: 2, inlines: vec![] };
        assert_eq!(block_spacing(&h2), (0.9, 0.35));
        let h3 = Block::Heading { level: 3, inlines: vec![] };
        assert_eq!(block_spacing(&h3), (0.8, 0.3));
        for lvl in 4..=6 {
            let h = Block::Heading { level: lvl, inlines: vec![] };
            assert_eq!(block_spacing(&h), (0.7, 0.3));
        }
        let l = Block::List { ordered: false, start: 1, items: vec![] };
        assert_eq!(block_spacing(&l), (0.5, 0.5));
        let t = Block::Table { rows: vec![], header_rows: 0 };
        assert_eq!(block_spacing(&t), (0.6, 0.6));
        assert_eq!(block_spacing(&Block::Rule), (0.7, 0.7));
    }

    #[test]
    fn marker_numbering_and_bullet() {
        assert_eq!(marker_text(false, 1, 0), "\u{2022}");
        assert_eq!(marker_text(false, 1, 7), "\u{2022}");
        assert_eq!(marker_text(true, 1, 0), "1.");
        assert_eq!(marker_text(true, 1, 2), "3.");
        assert_eq!(marker_text(true, 5, 0), "5.");
        assert_eq!(marker_text(true, 5, 1), "6.");
    }

    #[test]
    fn greedy_wrap_fits_two_words_per_line() {
        let toks = vec![
            fake_word("ab", 10.0),
            fake_space(10.0),
            fake_word("cd", 10.0),
            fake_space(10.0),
            fake_word("ef", 10.0),
        ];
        let lines = wrap_tokens(&toks, 50.0, &FontStore::new());
        assert_eq!(lines.len(), 2);
        // "ab cd" (incl. the space glyph) then "ef".
        assert_eq!(lines[0].chars.len(), 5);
        assert!((lines[0].width - 50.0).abs() < 1e-9);
        assert_eq!(lines[1].chars.len(), 2);
    }

    #[test]
    fn oversized_word_hard_breaks_by_characters() {
        let toks = vec![fake_word("abcde", 10.0)];
        let lines = wrap_tokens(&toks, 25.0, &FontStore::new());
        let counts: Vec<usize> = lines.iter().map(|l| l.chars.len()).collect();
        assert_eq!(counts, vec![2, 2, 1]);
    }

    #[test]
    fn column_widths_scale_shrink_and_clamp() {
        // Fits: scale naturals [100, 50] up to fill 200 (padding 12/col).
        let w = solve_column_widths(&[100.0, 50.0], &[40.0, 30.0], 200.0);
        assert!((w[0] + w[1] + 24.0 - 200.0).abs() < 1e-9);
        assert!((w[0] / w[1] - 2.0).abs() < 1e-9);
        // Too wide: shrink proportionally toward mins, landing exactly on avail.
        let w = solve_column_widths(&[100.0, 50.0], &[40.0, 30.0], 124.0);
        assert!((w[0] + w[1] + 24.0 - 124.0).abs() < 1e-9);
        assert!(w[0] >= 40.0 - 1e-9 && w[1] >= 30.0 - 1e-9);
        // Even mins overflow: use mins (overflow accepted).
        let w = solve_column_widths(&[100.0, 50.0], &[40.0, 30.0], 80.0);
        assert_eq!(w, vec![40.0, 30.0]);
    }

    #[test]
    fn layout_paragraph_produces_one_page_with_glyphs() {
        let blocks = vec![Block::Paragraph {
            inlines: vec![Inline::Text { text: "Hello world".into(), style: Style::default() }],
        }];
        let mut store = FontStore::new();
        let doc = layout(&blocks, settings(), &mut store);
        assert_eq!(doc.pages.len(), 1);
        assert!(doc.pages[0]
            .items
            .iter()
            .any(|d| matches!(d, Draw::Text { glyphs, .. } if !glyphs.is_empty())));
        let used = store.unicode_map(FontKey::SansRegular);
        assert!(used.iter().any(|&(_, c)| c == 'H'));
        assert!(used.iter().any(|&(_, c)| c == 'w'));
    }

    #[test]
    fn layout_link_adds_click_rect_and_underline() {
        let style = Style { link: Some("https://example.com".into()), ..Default::default() };
        let blocks = vec![Block::Paragraph {
            inlines: vec![Inline::Text { text: "click me".into(), style }],
        }];
        let mut store = FontStore::new();
        let doc = layout(&blocks, settings(), &mut store);
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].links.len(), 1);
        assert_eq!(doc.pages[0].links[0].4, "https://example.com");
        // underline rect uses the link color
        assert!(doc.pages[0]
            .items
            .iter()
            .any(|d| matches!(d, Draw::Rect { color, .. } if *color == LINK_COLOR)));
    }

    #[test]
    fn layout_heading_uses_bold_face() {
        let blocks = vec![Block::Heading {
            level: 2,
            inlines: vec![Inline::Text { text: "Title".into(), style: Style::default() }],
        }];
        let mut store = FontStore::new();
        let doc = layout(&blocks, settings(), &mut store);
        assert!(doc.pages[0]
            .items
            .iter()
            .any(|d| matches!(d, Draw::Text { font: FontKey::SansBold, size, .. }
                             if (*size - 11.0 * 1.6).abs() < 1e-9)));
    }

    #[test]
    fn layout_empty_input_yields_single_blank_page() {
        let mut store = FontStore::new();
        let doc = layout(&[], settings(), &mut store);
        assert_eq!(doc.pages.len(), 1);
        assert!(doc.pages[0].items.is_empty());
        assert!(doc.pages[0].links.is_empty());
        assert!(store.used_keys().is_empty());
    }

    #[test]
    fn layout_unordered_list_places_bullet_marker() {
        let blocks = vec![Block::List {
            ordered: false,
            start: 1,
            items: vec![vec![Block::Paragraph {
                inlines: vec![Inline::Text { text: "one".into(), style: Style::default() }],
            }]],
        }];
        let mut store = FontStore::new();
        let doc = layout(&blocks, settings(), &mut store);
        assert!(doc.pages[0]
            .items
            .iter()
            .any(|d| matches!(d, Draw::Text { glyphs, .. }
                             if glyphs.iter().any(|g| g.unicode == '\u{2022}'))));
        assert!(store.unicode_map(FontKey::SansRegular).iter().any(|&(_, c)| c == '\u{2022}'));
    }

    #[test]
    fn layout_paginates_long_paragraph() {
        let words: Vec<Inline> = std::iter::repeat(Inline::Text {
            text: "word ".repeat(20),
            style: Style::default(),
        })
        .take(40)
        .collect();
        let blocks = vec![Block::Paragraph { inlines: words }];
        let mut store = FontStore::new();
        let doc = layout(&blocks, settings(), &mut store);
        assert!(doc.pages.len() > 1, "expected a page break, got {}", doc.pages.len());
    }

    #[test]
    fn layout_complex_document_smoke() {
        let para = |t: &str| {
            Block::Paragraph {
                inlines: vec![Inline::Text { text: t.into(), style: Style::default() }],
            }
        };
        let cell = |t: &str| vec![Inline::Text { text: t.into(), style: Style::default() }];
        let blocks = vec![
            Block::Heading { level: 1, inlines: vec![Inline::Text { text: "Doc".into(), style: Style::default() }] },
            Block::Table {
                header_rows: 1,
                rows: vec![
                    vec![cell("Name"), cell("Description")],
                    vec![cell("a"), cell("the first row of the table with a longer text")],
                    vec![cell("b"), cell("second row")],
                ],
            },
            Block::Preformatted { text: "fn main() {\n    println!(\"hi\");\n}".into() },
            Block::Blockquote { children: vec![para("quoted line one"), para("quoted line two")] },
            Block::List {
                ordered: false,
                start: 1,
                items: vec![
                    vec![para("outer item")],
                    vec![Block::List {
                        ordered: true,
                        start: 3,
                        items: vec![vec![para("inner item")]],
                    }],
                ],
            },
            Block::Rule,
        ];
        let mut store = FontStore::new();
        let doc = layout(&blocks, settings(), &mut store);
        assert_eq!(doc.pages.len(), 1); // small doc fits one page
        let items = &doc.pages[0].items;
        // <pre> background rect with its padding exists and is drawn before a mono text run.
        let bg = items.iter().position(|d| matches!(d, Draw::Rect { color, .. } if *color == PRE_BG));
        let mono = items
            .iter()
            .position(|d| matches!(d, Draw::Text { font: FontKey::MonoRegular, .. }));
        assert!(bg.is_some(), "pre background missing");
        assert!(mono.is_some(), "mono text missing");
        assert!(bg.unwrap() < mono.unwrap(), "pre background must precede its text");
        // header rule (dark) and light grid lines
        assert!(items.iter().any(|d| matches!(d, Draw::Rect { h, color, .. } if (*h - 0.75).abs() < 1e-9 && *color == HEADER_RULE_COLOR)));
        assert!(items.iter().any(|d| matches!(d, Draw::Rect { color, .. } if *color == GRID_COLOR)));
        // blockquote bar
        assert!(items.iter().any(|d| matches!(d, Draw::Rect { w, color, .. } if (*w - 3.0).abs() < 1e-9 && *color == QUOTE_BAR)));
        // hr
        assert!(items.iter().any(|d| matches!(d, Draw::Rect { h, color, .. } if (*h - 0.75).abs() < 1e-9 && *color == RULE_COLOR)));
        // nested ordered marker "3." and outer bullet both placed and marked used
        let used = store.unicode_map(FontKey::SansRegular);
        assert!(used.iter().any(|&(_, c)| c == '\u{2022}'));
        let has_marker = items.iter().any(|d| match d {
            Draw::Text { glyphs, .. } => {
                let s: String = glyphs.iter().map(|g| g.unicode).collect();
                s == "3."
            }
            _ => false,
        });
        assert!(has_marker, "ordered marker '3.' missing");
        // glyphs got recorded for bold (heading/header) and mono (pre) faces too
        assert!(!store.used_glyphs(FontKey::SansBold).is_empty());
        assert!(!store.used_glyphs(FontKey::MonoRegular).is_empty());
    }
}
