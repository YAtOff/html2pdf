# html2pdf

Convert HTML text to a PDF file — one job, no system dependencies.

`html2pdf` takes an HTML string and returns PDF bytes. That is all it does,
and it does it without shelling out to anything: no wkhtmltopdf, no headless
Chromium, no pango/cairo, no LibreOffice. The converter is a Rust core
exposed to Python through [PyO3], and the DejaVu font family is embedded in
the extension binary itself, so the wheel is fully self-contained.

## Why

Most HTML-to-PDF tools wrap a browser or a system rendering stack: hundreds
of megabytes of dependencies, fragile subprocess calls, and font
substitution surprises. `html2pdf` takes the opposite trade-off:

- **Rust core via PyO3** — parsing, layout and PDF serialization all run in
  native code inside a single importable extension module.
- **Embedded fonts** — six DejaVu faces (sans regular/bold/italic/
  bold-italic, mono, mono-bold) are compiled in with `include_bytes!` and
  subsetted on export, so output files stay small.
- **Latin, Cyrillic and Greek out of the box** — DejaVu's coverage means
  common European scripts render correctly without any font configuration.
- **Deterministic, dependency-free output** — the same input always yields
  the same pages, on any machine, with no external state.

## Install

```bash
pip install html2pdf
```

Python 3.9+ is supported. Building from source requires Rust and [maturin]:

```bash
pip install maturin
maturin build --release        # produces a wheel in dist/
pip install dist/html2pdf-*.whl
# or, for development: maturin develop
```

### Development with uv

With [uv] and a Rust toolchain installed, set up the environment and build
the extension in one step:

```bash
uv sync                  # creates .venv and builds/installs html2pdf (editable)
uv run html2pdf input.html out.pdf
uv run python -c "import html2pdf; print(html2pdf.convert('<p>Hi</p>')[:4])"
```

After changing Rust code, rebuild with `uv sync --reinstall-package html2pdf`.
To build distributable artifacts (sdist and wheel in `dist/`):

```bash
uv build
```

## Usage

### Python API

```python
import html2pdf

data = html2pdf.convert("<h1>Hello</h1><p>World</p>")
with open("out.pdf", "wb") as f:
    f.write(data)
```

`convert` accepts a few keyword arguments:

```python
data = html2pdf.convert(
    html,
    title="Report",        # PDF title (falls back to <title>)
    page_size="letter",    # "a4" (default) or "letter"
    margin=56.7,           # points; 56.7 pt ≈ 2 cm
    font_size=11.0,        # base body size in points
)
```

There is also a convenience wrapper that writes the file for you:

```python
path = html2pdf.convert_to_file("<p>Done.</p>", "out.pdf")
```

### Command line

```bash
html2pdf input.html out.pdf
html2pdf --page-size letter --font-size 12 report.html report.pdf
cat input.html | html2pdf - out.pdf          # read from stdin
html2pdf input.html - > out.pdf              # write to stdout
```

## Supported HTML subset

The converter targets the structural, text-oriented parts of HTML:

- Headings `<h1>`–`<h6>`
- Paragraphs `<p>`
- Bold, italic and underline (`<b>`, `<strong>`, `<i>`, `<em>`, `<u>`)
- Inline code `<code>` (rendered in the mono face)
- Links `<a href="...">` — emitted as clickable PDF annotations
  (http/https/mailto/ftp and relative URIs)
- Ordered and unordered lists (`<ol>`/`<ul>`), nested lists, the `start`
  attribute, and definition lists (`<dl>`, `<dt>`, `<dd>` — terms render bold)
- Blockquotes `<blockquote>` (nestable, rendered with a side bar)
- Preformatted code `<pre>` with line breaks preserved (tabs expand to
  4 spaces)
- Tables `<table>` with `<th>`/`<td>` cells, auto-sized columns and a bold
  header row
- `<hr>` and `<br>`
- HTML entities (e.g. `&amp;`, `&nbsp;` — non-breaking space is preserved)
- The document `<title>` becomes the PDF title

## Limitations

By design, this is a text converter:

- **No CSS styling** — inline styles, classes and stylesheets are ignored;
  the built-in typography (sizes, spacing, list markers) is fixed.
- **No images** — `<img>` and other embedded media are skipped.
- **No JavaScript** — nothing is executed; the markup is read as-is.
- **Structural simplifications** — `<div>`-like containers always split text
  into separate paragraphs; tables ignore `colspan`/`rowspan` and nested
  tables flatten to text; only direct `li`/`td`/`th` children count; `q`,
  `mark`, `sub`, `sup`, `del`, `ins` and friends render as plain text;
  `#fragment`-only links render unlinked.

For pixel-accurate rendering of arbitrary web pages, use a browser-based
tool instead; for clean, portable PDFs from structured text, use `html2pdf`.

## Bundled fonts

This package embeds fonts from the [DejaVu] font family. DejaVu is
copyrighted by its contributors and licensed under the Bitstream Vera
license; the full text ships at `assets/fonts/LICENSE-DejaVu.txt`.

## License

`html2pdf` is distributed under the [MIT License](LICENSE).

[PyO3]: https://pyo3.rs
[maturin]: https://www.maturin.rs
[uv]: https://docs.astral.sh/uv/
[DejaVu]: https://dejavu-fonts.github.io
