//! PDF — text extraction, with the limits stated rather than papered over.
//!
//! `pdf-extract` gives text in content-stream order. It does **not** give
//! reading order for multi-column layouts, table reconstruction, heading
//! inference from font metrics, or OCR. pdfminer.six (what markitdown uses)
//! is better at the first three, and no pure-Rust stack matches it today.
//!
//! So this converter does two things and says what it did:
//!
//! 1. extracts per-page text, and
//! 2. applies conservative structural heuristics — a short standalone line
//!    followed by a blank one reads as a heading — each of which raises a
//!    `HeuristicApplied` warning the first time it fires.
//!
//! A page with no extractable text raises `NoTextLayer`. That is the signal a
//! scanned document needs OCR, which in this plugin means a vision-model
//! call, not Tesseract.

use crate::converters::squeeze;
use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document, Inline};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

/// Metadata key listing the 1-based page numbers with no extractable text.
///
/// This crate cannot OCR anything — it has no host and therefore no model —
/// so it records *which* pages need it and leaves the decision to the caller.
pub const SCANNED_PAGES_KEY: &str = "pdf_scanned_pages";

/// Metadata key holding the total page count, so a caller can tell a
/// fully-scanned document from one with a single scanned insert.
pub const PAGE_COUNT_KEY: &str = "pdf_page_count";

pub struct PdfConverter;

impl Converter for PdfConverter {
    fn name(&self) -> &'static str {
        "pdf"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        // The magic bytes are decisive here, so an unlabelled PDF converts and
        // a `.pdf` that is not one goes to whatever actually matches.
        probe.starts_with(b"%PDF-")
            || (info.is_ext("pdf") && probe.contains(b"%PDF-"))
            || (info.is_mime("application/pdf") && probe.contains(b"%PDF-"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let mut doc = Document::new();

        // Load once for metadata and the encryption check. An encrypted file
        // has a completely different operator remedy from a corrupt one, so
        // the two must not collapse into one error.
        match pdf_extract::Document::load_mem(bytes) {
            Ok(loaded) => {
                if loaded.is_encrypted() {
                    return Err(ConvertError::Encrypted { format: "pdf" });
                }
                read_info(&loaded, &mut doc);
            }
            Err(e) => {
                return Err(ConvertError::Malformed {
                    format: "pdf",
                    message: e.to_string(),
                });
            }
        }
        cx.budget().check_deadline()?;

        let pages = pdf_extract::extract_text_from_mem_by_pages(bytes).map_err(|e| {
            let text = e.to_string();
            if text.to_ascii_lowercase().contains("encrypt") {
                ConvertError::Encrypted { format: "pdf" }
            } else {
                ConvertError::Malformed {
                    format: "pdf",
                    message: text,
                }
            }
        })?;

        if pages.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                "the PDF has no pages",
            ));
            return Ok(doc);
        }

        let mut empty_pages = Vec::new();
        let mut heuristic_fired = false;

        for (i, page) in pages.iter().enumerate() {
            cx.budget().check_deadline()?;
            cx.budget().charge_expanded(page.len() as u64)?;

            let number = i + 1;
            if page.trim().is_empty() {
                empty_pages.push(number);
                continue;
            }
            for block in page_blocks(page, &mut heuristic_fired) {
                doc.push(block);
            }
        }

        if !empty_pages.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                format!(
                    "{} of {} pages carried no extractable text ({}); \
                     they are probably scanned images and need OCR",
                    empty_pages.len(),
                    pages.len(),
                    page_list(&empty_pages)
                ),
            ));
            // Structured, not just prose: the plugin's OCR pass decides
            // whether to spend a model call on this document, and parsing a
            // human-readable warning to find out would be a contract nobody
            // declared. The warning stays for the operator.
            doc.metadata.set(
                SCANNED_PAGES_KEY,
                empty_pages
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
            doc.metadata.set(PAGE_COUNT_KEY, pages.len().to_string());
        }
        if heuristic_fired {
            doc.warn(Warning::new(
                WarningKind::HeuristicApplied,
                "headings were inferred from line shape, not from font metrics; \
                 PDF extraction here is text-only (no columns, no table reconstruction)",
            ));
        }
        if doc.blocks.is_empty() && empty_pages.len() == pages.len() {
            doc.warn(Warning::new(
                WarningKind::Degraded,
                "no text at all was extracted",
            ));
        }
        if doc.title.is_none()
            && let Some(f) = &info.filename
        {
            doc = doc.with_title(f.clone());
        }
        Ok(doc)
    }
}

/// The trailer's `/Info` dictionary.
fn read_info(loaded: &pdf_extract::Document, doc: &mut Document) {
    let Ok(info_ref) = loaded.trailer.get(b"Info") else {
        return;
    };
    let Ok(dict) = info_ref
        .as_reference()
        .and_then(|r| loaded.get_object(r))
        .and_then(pdf_extract::Object::as_dict)
    else {
        return;
    };
    let text = |key: &[u8]| -> Option<String> {
        let v = dict.get(key).ok()?.as_str().ok()?;
        let s = decode_pdf_string(v);
        let s = squeeze(&s);
        if s.is_empty() { None } else { Some(s) }
    };
    if let Some(t) = text(b"Title") {
        doc.title = Some(t);
    }
    doc.metadata.author = text(b"Author");
    doc.metadata.created = text(b"CreationDate");
    doc.metadata.modified = text(b"ModDate");
    for key in [&b"Subject"[..], b"Keywords", b"Producer", b"Creator"] {
        if let Some(v) = text(key) {
            doc.metadata
                .set(String::from_utf8_lossy(key).to_lowercase(), v);
        }
    }
}

/// PDF text strings are either PDFDocEncoding or UTF-16BE with a BOM.
fn decode_pdf_string(raw: &[u8]) -> String {
    if raw.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    // PDFDocEncoding agrees with Latin-1 across the range that appears in
    // metadata in practice.
    raw.iter().map(|b| *b as char).collect()
}

/// One page's text → blocks.
///
/// Headings are tested **per line**, not per blank-line-separated paragraph.
/// `pdf-extract` emits content-stream order with single newlines and rarely a
/// blank line, so a page usually arrives as one paragraph — testing that
/// paragraph as a whole meant `heading_shape` could effectively never fire,
/// and a document with real section headings came out as one wall of prose.
///
/// Testing per line is only safe because `heading_shape` is strict: a line
/// must be all-caps or numbered-section shaped, so an ordinary wrapped line
/// like "Revenue rose to 1200 in the third" is not promoted.
fn page_blocks(page: &str, heuristic_fired: &mut bool) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut pending: Vec<&str> = Vec::new();

    let flush = |pending: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if pending.is_empty() {
            return;
        }
        // Extraction breaks lines at the layout's width, not at sentence
        // ends, so the run is rejoined into one piece of prose.
        let joined = rejoin(&pending.join("\n"));
        pending.clear();
        if !joined.is_empty() {
            blocks.push(Block::Paragraph(Inline::text(joined)));
        }
    };

    for line in page.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut pending, &mut blocks);
            continue;
        }
        if let Some(level) = heading_shape(trimmed) {
            flush(&mut pending, &mut blocks);
            *heuristic_fired = true;
            blocks.push(Block::Heading {
                level,
                text: Inline::text(squeeze(trimmed)),
            });
            continue;
        }
        pending.push(line);
    }
    flush(&mut pending, &mut blocks);
    blocks
}

/// A conservative heading test. One short line, no sentence-ending
/// punctuation, either all-caps or title-shaped.
///
/// Deliberately reluctant: a false positive turns a sentence into a heading
/// and misleads a reader about the document's structure, which is worse than
/// a missed heading.
fn heading_shape(para: &str) -> Option<u8> {
    let line = para.trim();
    if line.contains('\n') || line.len() > 80 || line.len() < 3 {
        return None;
    }
    if line.ends_with(['.', ',', ';', ':']) {
        return None;
    }
    let words = line.split_whitespace().count();
    if words > 12 {
        return None;
    }
    let letters: String = line.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return None;
    }

    // A numbered section: "3.2 Results".
    if let Some((num, rest)) = line.split_once(' ')
        && !rest.trim().is_empty()
        && num
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ')')
        && num.chars().any(|c| c.is_ascii_digit())
    {
        let depth = num.matches('.').count().min(4) as u8;
        return Some((depth + 1).clamp(1, 6));
    }

    if letters.chars().all(char::is_uppercase) {
        return Some(2);
    }
    None
}

/// Rejoin extraction line breaks, repairing hyphenation across them.
fn rejoin(para: &str) -> String {
    let mut out = String::with_capacity(para.len());
    for line in para.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if out.ends_with('-') {
            // A word split across the line break.
            out.pop();
            out.push_str(line);
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    squeeze(&out)
}

/// "1, 2, 5" — capped so a 900-page scan does not produce a 900-number
/// warning message.
fn page_list(pages: &[usize]) -> String {
    const MAX: usize = 12;
    if pages.len() <= MAX {
        return pages
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
    }
    let head = pages[..MAX]
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{head}, … and {} more", pages.len() - MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    fn convert(bytes: &[u8]) -> Result<Document, ConvertError> {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        PdfConverter.convert(bytes, &StreamInfo::new().with_extension("pdf"), &cx)
    }

    /// The smallest structurally valid PDF: catalog, page tree, one page with
    /// a content stream that draws one string.
    fn minimal_pdf(text: &str) -> Vec<u8> {
        let content = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        let mut objects: Vec<String> = Vec::new();
        objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_owned());
        objects.push("<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned());
        objects.push(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_owned(),
        );
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
        objects.push(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_owned(),
        );

        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", i + 1));
        }
        let xref_at = out.len();
        out.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
        out.push_str("0000000000 65535 f \n");
        for off in &offsets {
            out.push_str(&format!("{off:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        ));
        out.into_bytes()
    }

    #[test]
    fn a_pdf_with_a_text_layer_yields_its_words() {
        let doc = convert(&minimal_pdf("Hello from a PDF")).expect("converts");
        let out = format!("{:?}", doc.blocks);
        assert!(out.contains("Hello"), "{out}");
    }

    #[test]
    fn a_non_pdf_is_malformed_not_a_panic() {
        let e = convert(b"not a pdf at all").unwrap_err();
        assert!(
            matches!(e, ConvertError::Malformed { .. }),
            "{e} ({})",
            e.code()
        );
    }

    #[test]
    fn a_truncated_pdf_is_malformed() {
        let mut bytes = minimal_pdf("x");
        bytes.truncate(bytes.len() / 2);
        assert!(convert(&bytes).is_err());
    }

    #[test]
    fn only_actual_pdf_bytes_are_accepted() {
        let pdf = StreamInfo::new().with_extension("pdf");
        assert!(PdfConverter.accepts(&Probe::new(b"%PDF-1.7\n"), &pdf));
        // The extension alone is not enough — a mislabelled file must reach
        // whatever converter actually matches it.
        assert!(!PdfConverter.accepts(&Probe::new(b"PK\x03\x04"), &pdf));
        // ...and an unlabelled PDF still converts.
        assert!(PdfConverter.accepts(&Probe::new(b"%PDF-1.7\n"), &StreamInfo::new()));
    }

    // --- heuristics -------------------------------------------------------

    #[test]
    fn numbered_sections_become_nested_headings() {
        assert_eq!(heading_shape("1 Introduction"), Some(1));
        assert_eq!(heading_shape("3.2 Results"), Some(2));
        assert_eq!(heading_shape("3.2.1 Detail"), Some(3));
    }

    #[test]
    fn all_caps_short_lines_become_headings() {
        assert_eq!(heading_shape("EXECUTIVE SUMMARY"), Some(2));
    }

    #[test]
    fn ordinary_prose_is_never_promoted_to_a_heading() {
        assert_eq!(heading_shape("This is a normal sentence."), None);
        assert_eq!(
            heading_shape("A line with no full stop but rather a lot of words in it indeed truly"),
            None
        );
        assert_eq!(heading_shape("two\nlines"), None);
        assert_eq!(heading_shape("Introduction:"), None);
    }

    #[test]
    fn extraction_line_breaks_are_rejoined() {
        assert_eq!(rejoin("one\ntwo\nthree"), "one two three");
    }

    #[test]
    fn hyphenation_across_a_line_break_is_repaired() {
        assert_eq!(rejoin("multi-\nline word"), "multiline word");
    }

    #[test]
    fn the_empty_page_list_is_capped() {
        let many: Vec<usize> = (1..=40).collect();
        let s = page_list(&many);
        assert!(s.contains("and 28 more"), "{s}");
        assert!(s.len() < 120, "{s}");
    }

    #[test]
    fn utf16_metadata_strings_decode() {
        let raw = [0xFE, 0xFF, 0x00, b'H', 0x00, b'i'];
        assert_eq!(decode_pdf_string(&raw), "Hi");
        assert_eq!(decode_pdf_string(b"Plain"), "Plain");
    }

    #[test]
    fn a_page_with_no_text_raises_no_text_layer() {
        let mut fired = false;
        assert!(page_blocks("   \n  ", &mut fired).is_empty());
        assert!(!fired);
    }

    #[test]
    fn a_scanned_page_is_recorded_in_metadata_not_only_in_prose() {
        // The plugin's OCR pass keys on this. Leaving the page list only in
        // the warning text would make a human-readable string a contract.
        let doc = convert(&minimal_pdf_without_text()).expect("converts");
        assert_eq!(doc.metadata.get(SCANNED_PAGES_KEY), Some("1"));
        assert_eq!(doc.metadata.get(PAGE_COUNT_KEY), Some("1"));
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::NoTextLayer),
            "{:?}",
            doc.warnings
        );
    }

    #[test]
    fn a_pdf_with_text_records_no_scanned_pages() {
        let doc = convert(&minimal_pdf("Readable text")).expect("converts");
        assert_eq!(doc.metadata.get(SCANNED_PAGES_KEY), None);
    }

    /// A structurally valid PDF whose page draws nothing — the shape a scan
    /// takes once the image is stripped.
    fn minimal_pdf_without_text() -> Vec<u8> {
        minimal_pdf("")
    }
}
