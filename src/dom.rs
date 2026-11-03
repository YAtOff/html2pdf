//! HTML -> styled block tree.
//!
//! [`parse`] converts an HTML document into an optional document title
//! (from `<title>`) plus a list of top-level [`Block`]s. The HTML itself
//! is parsed by `scraper` (html5ever), so entities are already decoded
//! (`&nbsp;` arrives as U+00A0) and misnested tags are repaired before we
//! see the tree. This module only decides what is block-level, what is
//! inline, how whitespace collapses and which inline styles apply.
//!
//! Supported subset (deliberate simplifications):
//! * Whitespace: runs of ASCII whitespace collapse to one U+0020; U+00A0
//!   from `&nbsp;` is preserved, glued inside words. No leading space is
//!   emitted at block start and no trailing space at block end.
//! * Inline styles: `b`/`strong`, `i`/`em`/`var`/`cite`, `u`,
//!   `code`/`kbd`/`samp`/`tt` and `a`. Every other inline-ish element
//!   (including unknown tags) is transparent; only the innermost `<a>`
//!   supplies the link target.
//! * Structural containers (`div`, `section`, `body`, `form`, `dl`, ...)
//!   create no block of their own; pending inline flow is flushed at
//!   their boundaries, so sibling containers never merge into one
//!   paragraph.
//! * Lists: only direct `li` children of `ul`/`ol` become items; bare
//!   text and stray elements inside lists are dropped.
//! * `<pre>`: raw descendant text, each tab expanded to 4 spaces, one
//!   leading and one trailing newline dropped, internal newlines kept.
//! * Tables: rows are every `tr` below the table (except inside nested
//!   tables); cells are the direct `td`/`th` children of a row, flattened
//!   to inline content (block children of a cell are treated as
//!   transparent). Short rows are padded, `header_rows` is 1 when the
//!   first row consists entirely of `th` cells, `colspan`/`rowspan` are
//!   ignored and `caption`/`colgroup` are dropped.
//! * Dropped entirely: `head`, `title`, `script`, `style`, `template`,
//!   `noscript`, `svg`, `canvas`, `iframe`, `object`, `embed`, `video`,
//!   `audio`, `img`, `input`, `button`, `select`, `textarea`, `map`.
//! * Nesting deeper than 256 elements is dropped: the tree walkers are
//!   recursive, so pathological input (thousands of nested tags) must
//!   be cut off rather than allowed to overflow the stack.

use crate::types::{Block, Inline, Style, TableRow};
use scraper::node::{Element, Node};
use scraper::{ElementRef, Html};

/// Elements whose subtree is dropped entirely (no text to render).
const SKIP_TAGS: &[&str] = &[
    "head", "title", "script", "style", "template", "noscript", "svg", "canvas", "iframe",
    "object", "embed", "video", "audio", "img", "input", "button", "select", "textarea", "map",
];

/// Maximum element nesting the tree walkers descend into. Subtrees
/// deeper than this are dropped as if they did not exist: the walkers
/// are recursive, so unbounded nesting (e.g. thousands of nested
/// `<div>`s) would otherwise overflow the stack — a hard process
/// abort that no `catch_unwind` could intercept.
const MAX_DEPTH: usize = 256;

/// Elements that produce a block (or a paragraph-like wrapper) of their own.
const BLOCK_TAGS: &[&str] = &[
    "p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "pre", "blockquote", "hr", "table",
    "dt", "dd",
];

/// Structural containers: no block of their own; they splice their
/// children's blocks and flush inline flow at their boundaries.
const CONTAINER_TAGS: &[&str] = &[
    "div", "section", "article", "aside", "main", "nav", "header", "footer", "figure",
    "figcaption", "details", "summary", "address", "fieldset", "form", "dl", "body", "html",
    "li", // stray `li` outside a list: render its contents
];

/// Parse an HTML document into (document title, block tree).
///
/// Never panics: any input, including the empty string, yields a title
/// (`None` when there is no non-empty `<title>`) and a list of blocks
/// (possibly empty).
pub fn parse(html: &str) -> (Option<String>, Vec<Block>) {
    let document = Html::parse_document(html);
    // After parsing there is always an <html> root element, with <head>
    // and <body> subtrees below it (created implicitly when missing).
    let root = document.root_element();

    let title = find_title(root, 0).and_then(|title| {
        let mut raw = String::new();
        collect_text(title, &mut raw, 0);
        let collapsed = collapse_ws(&raw);
        (!collapsed.is_empty()).then_some(collapsed)
    });

    (title, blocks_of(root, &Style::default(), 0))
}

/// First `<title>` element in document order (usually inside `<head>`).
/// Stops descending past `MAX_DEPTH`.
fn find_title(root: ElementRef<'_>, depth: usize) -> Option<ElementRef<'_>> {
    if depth > MAX_DEPTH {
        return None;
    }
    for child in root.children() {
        if let Some(element) = ElementRef::wrap(child) {
            if element.value().name() == "title" {
                return Some(element);
            }
            if let Some(found) = find_title(element, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Inline content accumulated between two block-level elements.
struct Builder {
    inlines: Vec<Inline>,
    /// Saw collapsible whitespace since the last emitted word.
    pending_ws: bool,
}

impl Builder {
    fn new() -> Builder {
        Builder { inlines: Vec::new(), pending_ws: false }
    }

    /// Append one word (space-prefixed by the caller when needed),
    /// merging into the previous run when the style is identical.
    fn push_word(&mut self, word: &str, style: &Style) {
        match self.inlines.last_mut() {
            Some(Inline::Text { text, style: prev }) if *prev == *style => {
                text.push_str(word);
            }
            _ => self.inlines.push(Inline::Text { text: word.to_string(), style: style.clone() }),
        }
    }

    /// Collapse one text node's whitespace: non-whitespace runs become
    /// words, a single space is inserted before a word when the
    /// accumulator is non-empty and whitespace was seen since the last
    /// word. U+00A0 is not whitespace and stays glued inside words.
    fn push_text(&mut self, raw: &str, style: &Style) {
        if raw.is_empty() {
            return;
        }
        let mut word = String::new();
        for c in raw.chars() {
            if is_collapsible_ws(c) {
                if !word.is_empty() {
                    let finished = std::mem::take(&mut word);
                    self.emit_word(&finished, style);
                }
                self.pending_ws = true;
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            self.emit_word(&word, style);
        }
    }

    /// Emit one word, space-prefixed when whitespace separates it from
    /// the previous content; resets the pending-whitespace flag.
    fn emit_word(&mut self, word: &str, style: &Style) {
        if self.pending_ws && !self.inlines.is_empty() {
            let mut prefixed = String::with_capacity(word.len() + 1);
            prefixed.push(' ');
            prefixed.push_str(word);
            self.push_word(&prefixed, style);
        } else {
            self.push_word(word, style);
        }
        self.pending_ws = false;
    }

    fn push_break(&mut self) {
        self.inlines.push(Inline::Break);
        self.pending_ws = false;
    }

    /// True when the accumulator holds at least one non-empty text run
    /// (not just breaks), i.e. flushing it would emit a visible block.
    fn has_text(&self) -> bool {
        self.inlines
            .iter()
            .any(|inline| matches!(inline, Inline::Text { text, .. } if !text.is_empty()))
    }

    /// Emit the accumulated inlines as a `Paragraph` block, unless they
    /// contain no real text (`<p> </p>` and `<p><br></p>` flush nothing).
    fn flush(&mut self, blocks: &mut Vec<Block>) {
        if self.has_text() {
            blocks.push(Block::Paragraph { inlines: std::mem::take(&mut self.inlines) });
        } else {
            self.inlines.clear();
        }
        self.pending_ws = false;
    }
}

/// ASCII whitespace per HTML: space, tab, LF, CR, form feed.
/// (U+00A0 and other Unicode spaces are deliberately NOT included.)
fn is_collapsible_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

/// Whitespace runs -> single spaces, trimmed at both ends.
fn collapse_ws(s: &str) -> String {
    s.split(is_collapsible_ws)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Walk `parent`'s children in order, accumulating inline content in
/// `builder` and completed blocks into `blocks`. `style` is the inline
/// style inherited from the ancestors of `parent`; `depth` is the
/// nesting level of `parent` itself — the walk stops descending once it
/// exceeds `MAX_DEPTH`, so deeper content is dropped, never recursed
/// into.
fn walk(
    parent: ElementRef<'_>,
    style: &Style,
    builder: &mut Builder,
    blocks: &mut Vec<Block>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in parent.children() {
        match child.value() {
            Node::Text(text) => builder.push_text(&text.text, style),
            Node::Element(el) => {
                let name = el.name();
                if SKIP_TAGS.contains(&name) {
                    continue;
                }
                if name == "br" {
                    builder.push_break();
                    continue;
                }
                // Element nodes always wrap successfully.
                let Some(element) = ElementRef::wrap(child) else { continue };
                if BLOCK_TAGS.contains(&name) {
                    builder.flush(blocks);
                    make_blocks(element, style, blocks, depth + 1);
                } else if CONTAINER_TAGS.contains(&name) {
                    // Transparent container: no block of its own; any
                    // pending inline flow ends at its boundaries.
                    builder.flush(blocks);
                    walk(element, style, builder, blocks, depth + 1);
                    builder.flush(blocks);
                } else {
                    // Inline (or unknown inline-ish) element: continue the
                    // same text flow with an updated style.
                    let mut inner = style.clone();
                    apply_inline_style(name, el, &mut inner);
                    walk(element, &inner, builder, blocks, depth + 1);
                }
            }
            // Comments, doctypes and processing instructions: no content.
            _ => {}
        }
    }
}

/// Update `style` for one inline element. Unknown names leave it as-is.
fn apply_inline_style(name: &str, el: &Element, style: &mut Style) {
    match name {
        "b" | "strong" => style.bold = true,
        "i" | "em" | "var" | "cite" => style.italic = true,
        "u" => style.underline = true,
        "code" | "kbd" | "samp" | "tt" => style.mono = true,
        // Only the innermost <a> applies; one without a usable href
        // explicitly clears any enclosing link.
        "a" => style.link = el.attr("href").and_then(normalize_link),
        _ => {}
    }
}

/// Keep http/https/mailto/ftp and relative URIs (including
/// protocol-relative `//host/...`). Drop empty targets, fragment-only
/// anchors (`#...`) and other schemes (`javascript:`, `data:`, ...).
fn normalize_link(href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if ["http://", "https://", "mailto:", "ftp://"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return Some(href.to_string());
    }
    // A `:` before any `/`, `?` or `#` introduces a scheme we do not handle.
    if let Some(colon) = href.find(':') {
        if !href[..colon].contains(|c| c == '/' || c == '?' || c == '#') {
            return None;
        }
    }
    Some(href.to_string())
}

/// Collect `node`'s content as blocks: walk it with a fresh accumulator
/// and flush whatever inline content is left at the end. `depth` is the
/// nesting level of `node` itself.
fn blocks_of(node: ElementRef<'_>, style: &Style, depth: usize) -> Vec<Block> {
    let mut builder = Builder::new();
    let mut blocks = Vec::new();
    walk(node, style, &mut builder, &mut blocks, depth);
    builder.flush(&mut blocks);
    blocks
}

/// Process one block-level element, appending its blocks. Elements whose
/// content is inline (`p`, headings, `dt`, `dd`) emit exactly one block
/// when they contain any text; if such an element unexpectedly contains
/// block-level children, those are spliced before the trailing block.
/// `depth` is the nesting level of `node` itself.
fn make_blocks(node: ElementRef<'_>, style: &Style, blocks: &mut Vec<Block>, depth: usize) {
    let el = node.value();
    match el.name() {
        "p" | "dd" => {
            push_wrapped(node, style, blocks, depth, |inlines| Block::Paragraph { inlines });
        }
        "dt" => {
            let mut bold = style.clone();
            bold.bold = true;
            push_wrapped(node, &bold, blocks, depth, |inlines| Block::Paragraph { inlines });
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = el.name().as_bytes()[1] - b'0';
            push_wrapped(node, style, blocks, depth, move |inlines| Block::Heading { level, inlines });
        }
        "ul" | "ol" => blocks.push(make_list(node, style, depth)),
        "pre" => blocks.push(make_pre(node, depth)),
        "blockquote" => blocks.push(Block::Blockquote { children: blocks_of(node, style, depth) }),
        "hr" => blocks.push(Block::Rule),
        "table" => blocks.push(make_table(node, style, depth)),
        // Anything else tagged block-ish: behave like a container.
        _ => {
            let mut builder = Builder::new();
            walk(node, style, &mut builder, blocks, depth);
            builder.flush(blocks);
        }
    }
}

/// Walk `node`'s children and append one block built from the collected
/// inline content. Nothing is appended when the element holds no real
/// text (so `<p> </p>` and `<h2></h2>` produce nothing).
fn push_wrapped(
    node: ElementRef<'_>,
    style: &Style,
    blocks: &mut Vec<Block>,
    depth: usize,
    wrap: impl FnOnce(Vec<Inline>) -> Block,
) {
    let mut builder = Builder::new();
    walk(node, style, &mut builder, blocks, depth);
    if builder.has_text() {
        blocks.push(wrap(builder.inlines));
    }
}

/// `<ul>` / `<ol>` -> `List`. Items are the direct `li` children; bare
/// text and stray elements between them are ignored. `start` is the
/// parsed `start` attribute of an `ol` (default 1).
fn make_list(node: ElementRef<'_>, style: &Style, depth: usize) -> Block {
    let el = node.value();
    let ordered = el.name() == "ol";
    let start = if ordered {
        el.attr("start").and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(1)
    } else {
        1
    };
    let mut items = Vec::new();
    for child in node.children() {
        if let Node::Element(child_el) = child.value() {
            if child_el.name() == "li" {
                if let Some(li) = ElementRef::wrap(child) {
                    items.push(blocks_of(li, style, depth + 1));
                }
            }
        }
    }
    Block::List { ordered, start, items }
}

/// `<pre>` -> `Preformatted`: every descendant text node concatenated
/// with no whitespace collapsing, each tab expanded to 4 spaces, one
/// leading and one trailing newline dropped.
fn make_pre(node: ElementRef<'_>, depth: usize) -> Block {
    let mut text = String::new();
    collect_text(node, &mut text, depth);
    let text = text.replace('\t', "    ");
    let text = text.strip_prefix('\n').unwrap_or(&text);
    let text = text.strip_suffix('\n').unwrap_or(text);
    Block::Preformatted { text: text.to_string() }
}

/// Concatenate every descendant text node, ignoring element tags.
/// Stops descending past `MAX_DEPTH`.
fn collect_text(node: ElementRef<'_>, out: &mut String, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in node.children() {
        match child.value() {
            Node::Text(text) => out.push_str(&text.text),
            Node::Element(_) => {
                if let Some(element) = ElementRef::wrap(child) {
                    collect_text(element, out, depth + 1);
                }
            }
            _ => {}
        }
    }
}

/// `<table>` -> `Table`.
fn make_table(node: ElementRef<'_>, style: &Style, depth: usize) -> Block {
    let mut row_nodes: Vec<ElementRef<'_>> = Vec::new();
    collect_rows(node, &mut row_nodes, depth);

    let mut rows: Vec<TableRow> = Vec::new();
    let mut header_rows = 0usize;
    for (index, row) in row_nodes.iter().enumerate() {
        let mut cells: TableRow = Vec::new();
        let mut all_th = true;
        for child in row.children() {
            if let Node::Element(cell_el) = child.value() {
                let name = cell_el.name();
                if name == "td" || name == "th" {
                    if name != "th" {
                        all_th = false;
                    }
                    if let Some(cell) = ElementRef::wrap(child) {
                        let mut builder = Builder::new();
                        flatten_cell(cell, style, &mut builder, depth + 1);
                        cells.push(builder.inlines);
                    }
                }
            }
        }
        if index == 0 && all_th && !cells.is_empty() {
            header_rows = 1;
        }
        rows.push(cells);
    }

    // Pad short rows to the table's width; over-wide rows keep all cells.
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    for row in &mut rows {
        while row.len() < width {
            row.push(Vec::new());
        }
    }

    Block::Table { rows, header_rows }
}

/// Collect every `tr` under `node` in document order (thead/tbody/tfoot
/// or direct children), without descending into nested tables: their
/// rows belong to the inner table.
fn collect_rows<'a>(node: ElementRef<'a>, rows: &mut Vec<ElementRef<'a>>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in node.children() {
        if let Node::Element(el) = child.value() {
            let name = el.name();
            if name == "table" {
                continue;
            }
            if let Some(element) = ElementRef::wrap(child) {
                if name == "tr" {
                    rows.push(element);
                }
                collect_rows(element, rows, depth + 1);
            }
        }
    }
}

/// Flatten ALL descendants of a table cell into inline content: block
/// children (`p`, `div`, lists, ...) are treated as transparent, inline
/// styles still apply and whitespace still collapses.
fn flatten_cell(node: ElementRef<'_>, style: &Style, builder: &mut Builder, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for child in node.children() {
        match child.value() {
            Node::Text(text) => builder.push_text(&text.text, style),
            Node::Element(el) => {
                let name = el.name();
                if SKIP_TAGS.contains(&name) {
                    continue;
                }
                if name == "br" {
                    builder.push_break();
                    continue;
                }
                if let Some(element) = ElementRef::wrap(child) {
                    let mut inner = style.clone();
                    apply_inline_style(name, el, &mut inner);
                    flatten_cell(element, &inner, builder, depth + 1);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::types::{Block, Inline, Style};

    /// Text runs (text + style) of an inline list, ignoring breaks.
    fn runs(inlines: &[Inline]) -> Vec<(String, Style)> {
        inlines
            .iter()
            .filter_map(|inline| match inline {
                Inline::Text { text, style } => Some((text.clone(), style.clone())),
                _ => None,
            })
            .collect()
    }

    /// Parse and assert the result is exactly one paragraph; return runs.
    fn paragraph_runs(html: &str) -> Vec<(String, Style)> {
        let (_, blocks) = parse(html);
        assert_eq!(blocks.len(), 1, "expected one block, got {blocks:?}");
        match &blocks[0] {
            Block::Paragraph { inlines } => runs(inlines),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    fn bold() -> Style {
        Style { bold: true, ..Style::default() }
    }

    fn mono() -> Style {
        Style { mono: true, ..Style::default() }
    }

    #[test]
    fn heading_and_bold_paragraph() {
        let (_, blocks) = parse("<h1>Hi</h1><p>Hello <b>world</b>!</p>");
        assert_eq!(blocks.len(), 2, "got {blocks:?}");
        match &blocks[0] {
            Block::Heading { level, inlines } => {
                assert_eq!(*level, 1);
                // Heading content keeps its natural (non-bold) styles.
                assert_eq!(runs(inlines), vec![("Hi".to_string(), Style::default())]);
            }
            other => panic!("expected a heading, got {other:?}"),
        }
        match &blocks[1] {
            Block::Paragraph { inlines } => {
                let r = runs(inlines);
                assert_eq!(r.len(), 3);
                assert_eq!(r[0], ("Hello".to_string(), Style::default()));
                // Collapsed single space, prefixed into the bold run.
                assert_eq!(r[1], (" world".to_string(), bold()));
                assert_eq!(r[2], ("!".to_string(), Style::default()));
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn entities_and_nbsp_are_decoded() {
        // &nbsp; is U+00A0: not whitespace, glued inside the segment;
        // &amp; is decoded and the ASCII space around it collapses.
        let r = paragraph_runs("<p>a&nbsp;b &amp; c</p>");
        assert_eq!(r, vec![("a\u{a0}b & c".to_string(), Style::default())]);
    }

    #[test]
    fn nested_lists() {
        let (_, blocks) = parse("<ul><li>one<ul><li>inner</li></ul></li><li>two</li></ul>");
        assert_eq!(blocks.len(), 1);
        let Block::List { ordered, start, items } = &blocks[0] else {
            panic!("expected a list, got {:?}", blocks[0])
        };
        assert!(!*ordered);
        assert_eq!(*start, 1);
        assert_eq!(items.len(), 2, "got {items:?}");

        // First item: a paragraph followed by the nested list.
        assert_eq!(items[0].len(), 2, "got {items:?}");
        match &items[0][0] {
            Block::Paragraph { inlines } => {
                assert_eq!(runs(inlines), vec![("one".to_string(), Style::default())]);
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
        let Block::List { items: inner_items, .. } = &items[0][1] else {
            panic!("expected a nested list, got {:?}", items[0][1])
        };
        assert_eq!(inner_items.len(), 1);
        assert!(matches!(&inner_items[0][0], Block::Paragraph { .. }));

        // Second item: just a paragraph.
        assert!(matches!(&items[1][0], Block::Paragraph { .. }));
    }

    #[test]
    fn preformatted_tabs_and_newlines() {
        let (_, blocks) = parse("<pre>line1\n\tline2\n</pre>");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Preformatted { text } => {
                // Tab -> 4 spaces, internal newlines kept, one trailing
                // newline dropped.
                assert_eq!(text, "line1\n    line2");
            }
            other => panic!("expected preformatted text, got {other:?}"),
        }

        // The parser (or we) drop exactly one leading newline.
        let (_, blocks) = parse("<pre>\nx</pre>");
        match &blocks[0] {
            Block::Preformatted { text } => assert_eq!(text, "x"),
            other => panic!("expected preformatted text, got {other:?}"),
        }

        // Tags inside <pre> are ignored; entities are decoded.
        let (_, blocks) = parse("<pre><code>a &amp; b</code>\r\n.</pre>");
        match &blocks[0] {
            Block::Preformatted { text } => assert_eq!(text, "a & b\n."),
            other => panic!("expected preformatted text, got {other:?}"),
        }
    }

    #[test]
    fn table_header_row_and_cells() {
        let html = "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>";
        let (_, blocks) = parse(html);
        assert_eq!(blocks.len(), 1);
        let Block::Table { rows, header_rows } = &blocks[0] else {
            panic!("expected a table, got {:?}", blocks[0])
        };
        assert_eq!(*header_rows, 1);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[1].len(), 2);
        assert_eq!(runs(&rows[0][0]), vec![("A".to_string(), Style::default())]);
        assert_eq!(runs(&rows[0][1]), vec![("B".to_string(), Style::default())]);
        assert_eq!(runs(&rows[1][0]), vec![("1".to_string(), Style::default())]);
        assert_eq!(runs(&rows[1][1]), vec![("2".to_string(), Style::default())]);
    }

    #[test]
    fn link_href_becomes_run_style() {
        let r = paragraph_runs("<p><a href=\"https://x/\">t</a></p>");
        let mut expected = Style::default();
        expected.link = Some("https://x/".to_string());
        assert_eq!(r, vec![("t".to_string(), expected)]);
    }

    #[test]
    fn title_from_head_is_collapsed() {
        let html = "<html><head><title> My  Title </title></head><body><p>x</p></body></html>";
        let (title, blocks) = parse(html);
        assert_eq!(title, Some("My Title".to_string()));
        // Head content is skipped entirely; only the body renders.
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph { inlines } => {
                assert_eq!(runs(inlines), vec![("x".to_string(), Style::default())]);
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn empty_and_whitespace_inputs_yield_nothing() {
        let (title, blocks) = parse("");
        assert_eq!(title, None);
        assert!(blocks.is_empty());
        let (title, blocks) = parse("   \n\t  ");
        assert_eq!(title, None);
        assert!(blocks.is_empty());
        // Empty / whitespace-only titles collapse to None.
        let (title, _) = parse("<html><head><title>  </title></head><body></body></html>");
        assert_eq!(title, None);
    }

    #[test]
    fn whitespace_only_paragraphs_are_dropped() {
        let (_, blocks) = parse("<p> </p><p></p><p><br></p>");
        assert!(blocks.is_empty());
    }

    #[test]
    fn link_filtering() {
        let r = paragraph_runs("<p><a href=\"#top\">t</a></p>");
        assert_eq!(r[0].1.link, None, "fragment anchors render unlinked");

        let r = paragraph_runs("<p><a href=\"javascript:alert(1)\">t</a></p>");
        assert_eq!(r[0].1.link, None, "javascript: is dropped");

        let r = paragraph_runs("<p><a href=\"ftp://h/f\">t</a></p>");
        assert_eq!(r[0].1.link, Some("ftp://h/f".to_string()));

        let r = paragraph_runs("<p><a href=\"/rel?a=1\">t</a></p>");
        assert_eq!(r[0].1.link, Some("/rel?a=1".to_string()));

        // Innermost <a> without a usable href clears the outer link.
        let r = paragraph_runs("<p><a href=\"https://outer/\"><span>t</span></a></p>");
        assert_eq!(r[0].1.link, Some("https://outer/".to_string()));
    }

    #[test]
    fn table_pads_short_rows() {
        let html = "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>";
        let (_, blocks) = parse(html);
        let Block::Table { rows, header_rows } = &blocks[0] else {
            panic!("expected a table, got {:?}", blocks[0])
        };
        assert_eq!(*header_rows, 0, "a td row is not a header");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].len(), 2, "short row padded to table width");
        assert!(rows[1][1].is_empty(), "padding is an empty cell");
    }

    #[test]
    fn ordered_list_start_attribute() {
        let (_, blocks) = parse("<ol start=\"3\"><li>a</li></ol>");
        match &blocks[0] {
            Block::List { ordered, start, items } => {
                assert!(*ordered);
                assert_eq!(*start, 3);
                assert_eq!(items.len(), 1);
            }
            other => panic!("expected a list, got {other:?}"),
        }
        for html in ["<ol start=\"abc\"><li>a</li></ol>", "<ol><li>a</li></ol>"] {
            let (_, blocks) = parse(html);
            match &blocks[0] {
                Block::List { start, .. } => assert_eq!(*start, 1),
                other => panic!("expected a list, got {other:?}"),
            }
        }
        // ul ignores any start attribute.
        let (_, blocks) = parse("<ul start=\"9\"><li>a</li></ul>");
        match &blocks[0] {
            Block::List { ordered, start, .. } => {
                assert!(!*ordered);
                assert_eq!(*start, 1);
            }
            other => panic!("expected a list, got {other:?}"),
        }
    }

    #[test]
    fn definition_list_terms_are_bold() {
        let (_, blocks) = parse("<dl><dt>Term</dt><dd>Definition</dd></dl>");
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::Paragraph { inlines } => {
                assert_eq!(runs(inlines), vec![("Term".to_string(), bold())]);
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
        match &blocks[1] {
            Block::Paragraph { inlines } => {
                assert_eq!(runs(inlines), vec![("Definition".to_string(), Style::default())]);
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn script_style_and_hidden_elements_are_skipped() {
        let html = "<p>a</p><script>var x = 1;</script><style>p{color:red}</style>\
                    <template><p>t</p></template><noscript>n</noscript><img src=\"x.png\">";
        let (_, blocks) = parse(html);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Paragraph { inlines } => {
                assert_eq!(runs(inlines), vec![("a".to_string(), Style::default())]);
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn mono_code_run() {
        let r = paragraph_runs("<p>a <code>x = 1</code> b</p>");
        assert_eq!(
            r,
            vec![("a".to_string(), Style::default()), (" x = 1".to_string(), mono()), (" b".to_string(), Style::default())]
        );
    }

    #[test]
    fn adjacent_equal_style_runs_merge() {
        // The two bold words merge into one run; the space between them
        // survives as a single prefix inside that run.
        let r = paragraph_runs("<p>a <b>b</b> <b>c</b></p>");
        assert_eq!(
            r,
            vec![("a".to_string(), Style::default()), (" b c".to_string(), bold())]
        );
    }

    #[test]
    fn containers_split_inline_flow() {
        // div is transparent for blocks but terminates pending inline
        // flow at its boundaries: three paragraphs, no cross-boundary
        // merging and no leading/trailing spaces.
        let (_, blocks) = parse("a<div>b</div>c");
        assert_eq!(blocks.len(), 3);
        for (block, word) in blocks.iter().zip(["a", "b", "c"]) {
            match block {
                Block::Paragraph { inlines } => {
                    assert_eq!(runs(inlines), vec![(word.to_string(), Style::default())]);
                }
                other => panic!("expected a paragraph, got {other:?}"),
            }
        }
    }

    #[test]
    fn blockquote_and_rule() {
        let (_, blocks) = parse("<blockquote><p>q</p></blockquote><hr>");
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::Blockquote { children } => assert_eq!(children.len(), 1),
            other => panic!("expected a blockquote, got {other:?}"),
        }
        assert!(matches!(&blocks[1], Block::Rule));
    }

    #[test]
    fn table_cell_flattens_block_children() {
        let html = "<table><tr><td><p>a</p> <p>b</p></td></tr></table>";
        let (_, blocks) = parse(html);
        let Block::Table { rows, .. } = &blocks[0] else {
            panic!("expected a table, got {:?}", blocks[0])
        };
        assert_eq!(
            runs(&rows[0][0]),
            vec![("a b".to_string(), Style::default())],
            "block children of a cell flatten to inline content"
        );
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for html in [
            "<p><b>unclosed & <em>bad",
            "<div",
            "<html><body>",
            "<table><tr><td>",
            "<<<>>>&amp;&",
            "<ul><li>",
            "<a href=>t</a>",
        ] {
            let (title, blocks) = parse(html);
            let _ = title;
            let _ = blocks;
        }
    }

    #[test]
    fn deep_nesting_is_truncated_not_crashed() {
        // 5000 nested <div>s would overflow the stack without the depth
        // guard — a hard process abort, not a catchable panic. The walk
        // must instead stop at MAX_DEPTH: the shallow text still renders
        // and the text at the bottom (beyond the cutoff) is dropped as
        // if it did not exist.
        let mut html = String::from("<p>top</p>");
        for _ in 0..5000 {
            html.push_str("<div>");
        }
        html.push_str("<p>bottom</p>");
        for _ in 0..5000 {
            html.push_str("</div>");
        }
        let (title, blocks) = parse(&html);
        assert_eq!(title, None);
        assert_eq!(blocks.len(), 1, "got {blocks:?}");
        match &blocks[0] {
            Block::Paragraph { inlines } => {
                assert_eq!(runs(inlines), vec![("top".to_string(), Style::default())]);
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }

        // Same guarantee for deep recursion through block re-entry
        // (blockquote wraps its children via blocks_of): no crash, and
        // the text beyond the cutoff is dropped — only the empty outer
        // wrapper blocks remain.
        let mut html = String::new();
        for _ in 0..3000 {
            html.push_str("<blockquote>");
        }
        html.push_str("q");
        let (_, blocks) = parse(&html);
        assert_eq!(blocks.len(), 1, "got {blocks:?}");
        assert!(matches!(&blocks[0], Block::Blockquote { .. }));
        assert!(!blocks_have_text(&blocks[0]), "deep text must be dropped");
    }

    /// Recursively check whether any block carries a non-empty text run.
    fn blocks_have_text(block: &Block) -> bool {
        let text_of = |inlines: &Vec<crate::types::Inline>| {
            inlines
                .iter()
                .any(|i| matches!(i, crate::types::Inline::Text { text, .. } if !text.is_empty()))
        };
        match block {
            Block::Paragraph { inlines } | Block::Heading { inlines, .. } => text_of(inlines),
            Block::List { items, .. } => {
                items.iter().any(|item| item.iter().any(blocks_have_text))
            }
            Block::Preformatted { text } => !text.is_empty(),
            Block::Blockquote { children } => children.iter().any(blocks_have_text),
            Block::Rule => false,
            Block::Table { rows, .. } => rows.iter().any(|r| r.iter().any(|c| !c.is_empty())),
        }
    }
}
