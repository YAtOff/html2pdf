"""html2pdf: convert HTML text to a PDF file.

A single-purpose package: give it an HTML string, get PDF bytes back.
Conversion runs in an embedded Rust core with bundled DejaVu fonts, so there
are no system dependencies (no wkhtmltopdf, no headless browser, no pango).
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

from ._html2pdf import convert as _convert

__version__ = "0.1.0"
__all__ = ["convert", "convert_to_file", "main"]


def convert(
    html: str,
    *,
    title: str | None = None,
    page_size: str = "a4",
    margin: float = 56.7,
    font_size: float = 11.0,
) -> bytes:
    """Convert an HTML string to PDF bytes.

    Args:
        html: HTML markup to convert.
        title: PDF document title (falls back to the HTML ``<title>``).
        page_size: ``"a4"`` or ``"letter"``.
        margin: Page margin in PDF points (72 pt = 1 inch; 56.7 pt ≈ 2 cm).
        font_size: Base body font size in points.

    Returns:
        The PDF file as bytes.
    """
    if not isinstance(html, str):
        raise TypeError(f"html must be str, got {type(html).__name__}")
    return _convert(html, title=title, page_size=page_size, margin=margin, font_size=font_size)


def convert_to_file(html: str, path: str | Path, **kwargs: Any) -> Path:
    """Convert an HTML string and write the PDF to ``path``."""
    data = convert(html, **kwargs)
    out = Path(path)
    out.write_bytes(data)
    return out


def main(argv: list[str] | None = None) -> int:
    """CLI entry point: ``html2pdf INPUT.html OUTPUT.pdf``."""
    import argparse

    parser = argparse.ArgumentParser(
        prog="html2pdf", description="Convert an HTML file to a PDF file."
    )
    parser.add_argument("input", help="input .html file (use '-' for stdin)")
    parser.add_argument("output", help="output .pdf file (use '-' for stdout)")
    parser.add_argument("--title", default=None, help="PDF document title")
    parser.add_argument("--page-size", default="a4", choices=["a4", "letter"])
    parser.add_argument("--margin", type=float, default=56.7, help="page margin in points")
    parser.add_argument("--font-size", type=float, default=11.0, help="base font size in points")
    args = parser.parse_args(argv)

    html = sys.stdin.read() if args.input == "-" else Path(args.input).read_text(encoding="utf-8")
    data = convert(
        html,
        title=args.title,
        page_size=args.page_size,
        margin=args.margin,
        font_size=args.font_size,
    )
    if args.output == "-":
        sys.stdout.buffer.write(data)
    else:
        Path(args.output).write_bytes(data)
    return 0
