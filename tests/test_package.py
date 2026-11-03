"""Comprehensive integration suite for the html2pdf package.

Designed to run inside a CLEAN virtualenv that contains only the built
wheel plus pytest/pypdf (no dev artifacts, no source checkout on
sys.path), proving the wheel is self-contained.

Invocation (from the project root, against the clean venv):

    /tmp/h2p-clean/bin/python -m pytest tests/ -v

`python -m pytest` prepends the current directory to sys.path, which
would otherwise shadow the installed wheel with the source-only
``html2pdf/`` package sitting next to this file (it contains no
compiled extension and fails to import). The sys.path scrub below drops
the project root again so ``import html2pdf`` resolves to site-packages.
"""

from __future__ import annotations

import subprocess
import sys
from io import BytesIO
from pathlib import Path

import pytest
from pypdf import PdfReader

# --- import hygiene: force resolution from the installed wheel ------------

_PROJECT_ROOT = Path(__file__).resolve().parent.parent


def _real(path: str) -> Path | None:
    try:
        return Path(path).resolve()
    except OSError:  # pragma: no cover - unreadable sys.path entry
        return None


sys.path[:] = [p for p in sys.path if _real(p) != _PROJECT_ROOT]

import html2pdf  # noqa: E402  (must come after the sys.path scrub)


# --- helpers ---------------------------------------------------------------


def make_pdf(html: str, **kwargs: object) -> bytes:
    return html2pdf.convert(html, **kwargs)  # type: ignore[arg-type]


def read_pdf(data: bytes) -> PdfReader:
    return PdfReader(BytesIO(data))


def all_text(data: bytes) -> str:
    """Concatenated extracted text of every page (order may shift)."""
    return "\n".join(page.extract_text() or "" for page in read_pdf(data).pages)


def embedded_fonts(data: bytes) -> dict[str, bool]:
    """Map of BaseFont name -> whether a /FontFile2 stream is embedded.

    For Type0/CIDFontType2 subset fonts the /FontDescriptor (carrying
    /FontFile2) lives on the descendant CIDFont, so both spots are
    checked.
    """
    found: dict[str, bool] = {}
    for page in read_pdf(data).pages:
        resources = page.get("/Resources")
        if not resources:
            continue
        fonts = resources.get_object().get("/Font")
        if not fonts:
            continue
        for _label, ref in fonts.get_object().items():
            font = ref.get_object()
            base = str(font.get("/BaseFont", ""))
            descriptors: list[object] = []
            direct = font.get("/FontDescriptor")
            if direct is not None:
                descriptors.append(direct.get_object())
            for descendant in font.get("/DescendantFonts") or []:
                desc = descendant.get_object().get("/FontDescriptor")
                if desc is not None:
                    descriptors.append(desc.get_object())
            found[base] = any(
                d.get("/FontFile2") is not None for d in descriptors  # type: ignore[attr-defined]
            )
    return found


# --- fixtures --------------------------------------------------------------


@pytest.fixture(scope="module")
def long_doc() -> bytes:
    """A 200-paragraph document; spans several pages."""
    html = "".join(
        f"<p>Paragraph {i}: the quick brown fox jumps over the lazy dog.</p>"
        for i in range(200)
    )
    return make_pdf(html)


@pytest.fixture(scope="module")
def bold_doc() -> bytes:
    return make_pdf("<p>plain words and <b>bold words</b> mixed</p>")


# --- package basics --------------------------------------------------------


def test_version():
    assert html2pdf.__version__ == "0.1.0"


def test_imports_resolve_to_the_installed_wheel():
    """The suite must exercise the wheel, not a source checkout."""
    location = Path(html2pdf.__file__).resolve()
    assert "site-packages" in str(location)
    assert not str(location).startswith(str(_PROJECT_ROOT))
    # The compiled Rust core ships inside the wheel.
    from html2pdf import _html2pdf

    assert Path(_html2pdf.__file__).suffix.startswith(".so")


def test_simple_paragraph():
    data = html2pdf.convert("<p>Hello, world!</p>")
    assert data[:5] == b"%PDF-"
    assert b"%%EOF" in data[-64:]


def test_pdf_structure_magic_and_eof():
    data = make_pdf("<h1>Title</h1><p>Body text.</p>")
    assert data.startswith(b"%PDF-1.")  # header with a version
    assert data.rstrip().endswith(b"%%EOF")
    assert b"%%EOF" in data[-64:]


# --- text round-trips via pypdf ---------------------------------------------


def test_text_roundtrip_english():
    text = all_text(make_pdf("<h1>Heading One</h1><p>Hello, world!</p>"))
    for word in ("Heading", "One", "Hello,", "world!"):
        assert word in text, f"word {word!r} missing from extracted text: {text!r}"


def test_text_roundtrip_bulgarian_cyrillic():
    html = "<h1>Здравей, свят!</h1><p>Това е <b>удебелен</b> тест.</p>"
    text = all_text(make_pdf(html))
    # Extraction may reorder whitespace: assert per word.
    for word in ("Здравей", "свят!", "Това", "удебелен", "тест."):
        assert word in text, f"word {word!r} missing from extracted text: {text!r}"


def test_text_roundtrip_entities():
    text = all_text(make_pdf("<p>A &amp; B &nbsp; C &copy; D</p>"))
    # `&` must survive decoding + re-encoding; © extraction varies by
    # reader (it may come back as-is or oddly), so only `&` is asserted.
    assert "&" in text
    for word in ("A", "B", "D"):
        assert word in text


# --- pages and pagination ---------------------------------------------------


def test_small_document_has_at_least_one_page():
    reader = read_pdf(make_pdf("<h1>H</h1><p>text</p><a href='https://x/'>l</a>"))
    assert len(reader.pages) >= 1


def test_long_document_spans_multiple_pages(long_doc: bytes):
    count = len(read_pdf(long_doc).pages)
    assert count >= 2, f"200 paragraphs should paginate, got {count} page(s)"
    assert count < 100, "suspiciously many pages for 200 short paragraphs"


def test_empty_html_yields_single_page_pdf():
    data = make_pdf("")
    assert data[:5] == b"%PDF-"
    assert len(read_pdf(data).pages) == 1


# --- page geometry ------------------------------------------------------------


@pytest.mark.parametrize(
    "page_size,width,height",
    [("a4", 595.28, 841.89), ("letter", 612.0, 792.0)],
)
def test_page_size_mediabox(page_size: str, width: float, height: float):
    reader = read_pdf(make_pdf("<p>x</p>", page_size=page_size))
    box = reader.pages[0].mediabox
    assert abs(float(box.width) - width) <= 0.5
    assert abs(float(box.height) - height) <= 0.5


# --- metadata ------------------------------------------------------------------


def test_title_option_sets_metadata():
    reader = read_pdf(make_pdf("<p>x</p>", title="X"))
    assert reader.metadata is not None
    assert reader.metadata.title == "X"


def test_html_title_fallback():
    html = "<html><head><title>Doc Title</title></head><body><p>x</p></body></html>"
    reader = read_pdf(make_pdf(html))
    assert reader.metadata is not None
    assert reader.metadata.title == "Doc Title"


def test_explicit_title_overrides_html_title():
    html = "<html><head><title>From HTML</title></head><body><p>x</p></body></html>"
    reader = read_pdf(make_pdf(html, title="Explicit"))
    assert reader.metadata is not None
    assert reader.metadata.title == "Explicit"


# --- links ------------------------------------------------------------------------


def test_link_annotation_has_uri():
    data = make_pdf('<p>see <a href="https://example.com/">this</a></p>')
    annots = read_pdf(data).pages[0].get("/Annots")
    assert annots, "expected at least one link annotation"
    annot = annots[0].get_object()
    assert str(annot.get("/Subtype")) == "/Link"
    action = annot.get("/A").get_object()
    assert str(action.get("/URI")) == "https://example.com/"


# --- embedded subset fonts ------------------------------------------------------------


def test_bold_and_regular_embed_distinct_subsets(bold_doc: bytes):
    fonts = embedded_fonts(bold_doc)
    assert len(fonts) >= 2, f"bold + regular should embed >= 2 fonts, got {fonts}"
    names = set(fonts)
    assert any("DejaVuSans-Bold" in name for name in names), names
    assert any("DejaVuSans" in name and "Bold" not in name for name in names), names
    # Every referenced face carries an embedded glyph stream.
    assert all(fonts.values()), fonts


def test_mono_embeds_dejavu_sans_mono_subset():
    fonts = embedded_fonts(make_pdf("<p><code>x = 1;</code></p>"))
    mono = [name for name in fonts if "DejaVuSansMono" in name]
    assert mono, f"expected a DejaVuSansMono subset, got {fonts}"
    assert all(fonts[name] for name in mono), fonts


def test_file_size_is_small_subsetting_works():
    data = make_pdf("<p>Hello</p>")
    assert len(data) < 100 * 1024, (
        f"simple output is {len(data)} bytes; a full DejaVuSans is ~740 KB, "
        "so ~750 KB+ would mean subsetting is broken"
    )


# --- python API conveniences --------------------------------------------------------


def test_convert_to_file_writes_and_returns_path(tmp_path: Path):
    out = tmp_path / "out.pdf"
    result = html2pdf.convert_to_file("<p>written</p>", out)
    assert isinstance(result, Path)
    assert result == out
    assert out.exists()
    data = out.read_bytes()
    assert data[:5] == b"%PDF-"
    assert "written" in all_text(data)


def test_convert_to_file_accepts_str_path_and_kwargs(tmp_path: Path):
    out = tmp_path / "str.pdf"
    html2pdf.convert_to_file("<p>s</p>", str(out), page_size="letter", title="T")
    reader = PdfReader(str(out))
    assert len(reader.pages) == 1
    box = reader.pages[0].mediabox
    assert abs(float(box.width) - 612.0) <= 0.5
    assert reader.metadata is not None
    assert reader.metadata.title == "T"


# --- error handling ---------------------------------------------------------------------


def test_rejects_bad_input():
    with pytest.raises(TypeError):
        html2pdf.convert(123)
    with pytest.raises(ValueError):
        html2pdf.convert("<p>x</p>", page_size="a3")
    with pytest.raises(ValueError):
        html2pdf.convert("<p>x</p>", font_size=0)


def test_type_error_message_names_the_actual_type():
    with pytest.raises(TypeError, match="int"):
        html2pdf.convert(123)


def test_page_size_error_mentions_supported_values():
    with pytest.raises(ValueError, match="a4"):
        html2pdf.convert("<p>x</p>", page_size="a3")
    with pytest.raises(ValueError, match="letter"):
        html2pdf.convert("<p>x</p>", page_size="a3")


def test_font_size_bounds_are_validated():
    for bad in (0, 7.9, 64.1, -11):
        with pytest.raises(ValueError):
            html2pdf.convert("<p>x</p>", font_size=bad)


# --- determinism -------------------------------------------------------------------------


def test_same_input_is_byte_identical():
    html = "<h1>Det</h1><p>Same <b>input</b>, same output &amp; entities.</p>"
    assert make_pdf(html) == make_pdf(html)


# --- deep nesting (integration mirror of the Rust depth guard) -----------------------------


def test_deeply_nested_html_returns_a_pdf():
    html = "<p>top</p>" + "<div>" * 5000 + "<p>bottom</p>" + "</div>" * 5000
    data = make_pdf(html)  # must not crash the process (stack overflow)
    assert data[:5] == b"%PDF-"
    assert len(read_pdf(data).pages) >= 1
    text = all_text(data)
    assert "top" in text  # shallow content survives
    assert "bottom" not in text  # content beyond the depth cutoff is dropped


# --- CLI ------------------------------------------------------------------------------------


def _venv_bin(name: str) -> Path:
    """Path of a console script installed next to the running python."""
    exe = Path(sys.executable)
    candidate = exe.with_name(name)
    assert candidate.exists(), f"console script {candidate} is missing from the venv"
    return candidate


def test_cli_module_mode(tmp_path: Path):
    infile = tmp_path / "in.html"
    infile.write_text("<p>cli module text</p>", encoding="utf-8")
    outfile = tmp_path / "module-out.pdf"
    proc = subprocess.run(
        [sys.executable, "-m", "html2pdf", str(infile), str(outfile)],
        capture_output=True,
        cwd=tmp_path,  # keep the project checkout out of sys.path
    )
    assert proc.returncode == 0, proc.stderr.decode("utf-8", "replace")
    assert outfile.exists()
    assert outfile.read_bytes()[:5] == b"%PDF-"
    assert "cli module text" in all_text(outfile.read_bytes())


def test_cli_console_script(tmp_path: Path):
    script = _venv_bin("html2pdf")
    infile = tmp_path / "in.html"
    infile.write_text("<p>console script text</p>", encoding="utf-8")
    outfile = tmp_path / "script-out.pdf"
    proc = subprocess.run(
        [str(script), str(infile), str(outfile)],
        capture_output=True,
        cwd=tmp_path,
    )
    assert proc.returncode == 0, proc.stderr.decode("utf-8", "replace")
    assert outfile.exists()
    text = all_text(outfile.read_bytes())
    assert "console script text" in text


def test_cli_stdout_and_stdin_with_dash(tmp_path: Path):
    proc = subprocess.run(
        [sys.executable, "-m", "html2pdf", "-", "-"],
        input="<p>piped through stdin</p>".encode("utf-8"),
        capture_output=True,
        cwd=tmp_path,
    )
    assert proc.returncode == 0, proc.stderr.decode("utf-8", "replace")
    assert proc.stdout[:5] == b"%PDF-"
    text = all_text(proc.stdout)
    assert "piped through stdin" in text


def test_cli_console_script_stdout_mode(tmp_path: Path):
    script = _venv_bin("html2pdf")
    infile = tmp_path / "in.html"
    infile.write_text("<p>script stdout</p>", encoding="utf-8")
    proc = subprocess.run(
        [str(script), str(infile), "-"],
        capture_output=True,
        cwd=tmp_path,
    )
    assert proc.returncode == 0, proc.stderr.decode("utf-8", "replace")
    assert proc.stdout[:5] == b"%PDF-"
    assert len(read_pdf(proc.stdout).pages) >= 1
