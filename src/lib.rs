//! html2pdf: convert HTML text to a PDF file.
//!
//! Pipeline: `dom::parse` (HTML -> styled block tree) -> `layout::layout`
//! (line breaking + pagination) -> `pdf::write` (PDF serialization with
//! subset embedded fonts).

mod dom;
mod fonts;
mod layout;
mod pdf;
mod types;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use types::PageSettings;

/// Convert an HTML string to PDF bytes.
#[pyfunction(signature = (html, *, title=None, page_size="a4", margin=56.7, font_size=11.0))]
fn convert(
    html: &str,
    title: Option<String>,
    page_size: &str,
    margin: f64,
    font_size: f64,
) -> PyResult<Vec<u8>> {
    if !(8.0..=64.0).contains(&font_size) {
        return Err(PyValueError::new_err("font_size must be between 8 and 64 points"));
    }
    if !(16.0..=144.0).contains(&margin) {
        return Err(PyValueError::new_err("margin must be between 16 and 144 points"));
    }
    let (width, height) = match page_size.to_ascii_lowercase().as_str() {
        "a4" => (595.28, 841.89),
        "letter" => (612.0, 792.0),
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown page_size {other:?}: expected 'a4' or 'letter'"
            )))
        }
    };
    let settings = PageSettings { width, height, margin, base_font_size: font_size };

    let (html_title, blocks) = dom::parse(html);
    let mut store = fonts::FontStore::new();
    let mut doc = layout::layout(&blocks, settings, &mut store);
    doc.title = title.or(html_title);

    Ok(pdf::write(&doc, &store))
}

#[pymodule]
fn _html2pdf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    Ok(())
}
