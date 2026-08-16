//! Plain text and Markdown.
//!
//! The catch-all, and the last converter tried. markitdown puts its
//! `PlainTextConverter` at generic priority for the same reason: anything
//! that decodes as text is better delivered as text than refused.

use crate::converters::decode_text;
use crate::cx::ConvertCx;
use crate::error::ConvertError;
use crate::ir::{Block, Document, Inline};
use crate::registry::{Converter, PRIORITY_GENERIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct TextConverter;

impl Converter for TextConverter {
    fn name(&self) -> &'static str {
        "text"
    }

    fn priority(&self) -> i32 {
        // The floor. Everything else gets first refusal.
        PRIORITY_GENERIC + 50
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        if !probe.looks_textual() {
            return false;
        }
        // A named text type, or nothing said at all. We decline formats that
        // *are* text but have a real converter (HTML, XML, CSV, JSON) so that
        // a mislabelled one still reaches its own converter on a later guess.
        match info.mimetype.as_deref() {
            Some(m) => {
                m == "text/plain"
                    || m == "text/markdown"
                    || m == "application/octet-stream"
                    || (m.starts_with("text/") && !SPECIALISED_TEXT_MIME.contains(&m))
            }
            None => true,
        }
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        _cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let text = decode_text(bytes, info.charset.as_deref());
        let mut doc = Document::new();

        if is_markdown(info) {
            // Already Markdown. Re-escaping it would turn every heading into
            // literal text, so it passes through verbatim.
            doc.title = first_atx_heading(&text);
            if !text.trim().is_empty() {
                doc.push(Block::Raw {
                    markdown: text.trim_end().to_owned(),
                });
            }
            return Ok(doc);
        }

        // Plain text: blank-line-separated paragraphs, which is the only
        // structure the format actually carries.
        for para in text.split("\n\n") {
            let para = para.trim();
            if para.is_empty() {
                continue;
            }
            doc.push(Block::Paragraph(Inline::text(para.replace('\n', " "))));
        }
        if doc.blocks.is_empty() && !text.trim().is_empty() {
            doc.push(Block::Paragraph(Inline::text(text.trim())));
        }
        Ok(doc)
    }
}

/// Text MIME types that have a converter of their own. Listing them keeps the
/// catch-all from swallowing a format that would convert better elsewhere.
const SPECIALISED_TEXT_MIME: &[&str] = &[
    "text/html",
    "text/xml",
    "text/csv",
    "text/tab-separated-values",
];

fn is_markdown(info: &StreamInfo) -> bool {
    info.is_mime("text/markdown") || info.is_ext("md") || info.is_ext("markdown")
}

/// First `# heading`, used as the document title.
fn first_atx_heading(text: &str) -> Option<String> {
    text.lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches('#').trim().to_owned())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    fn convert(bytes: &[u8], info: &StreamInfo) -> Document {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        TextConverter.convert(bytes, info, &cx).expect("converts")
    }

    fn accepts(bytes: &[u8], info: &StreamInfo) -> bool {
        TextConverter.accepts(&Probe::new(bytes), info)
    }

    #[test]
    fn paragraphs_split_on_blank_lines_and_unwrap() {
        let doc = convert(b"one\nstill one\n\ntwo\n", &StreamInfo::new());
        assert_eq!(doc.blocks.len(), 2);
        match &doc.blocks[0] {
            Block::Paragraph(i) => assert_eq!(i.to_plain(), "one still one"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn markdown_passes_through_verbatim() {
        let si = StreamInfo::new().with_filename("notes.md");
        let doc = convert(b"# Title\n\n- item\n", &si);
        assert_eq!(doc.title.as_deref(), Some("Title"));
        match &doc.blocks[0] {
            Block::Raw { markdown } => assert!(markdown.contains("- item")),
            other => panic!("markdown must not be re-escaped: {other:?}"),
        }
    }

    #[test]
    fn binary_is_declined() {
        assert!(!accepts(&[0u8, 1, 2, 3], &StreamInfo::new()));
    }

    #[test]
    fn formats_with_their_own_converter_are_declined() {
        for mime in ["text/html", "text/csv", "text/xml"] {
            let si = StreamInfo::new().with_mimetype(mime);
            assert!(!accepts(b"anything", &si), "should decline {mime}");
        }
    }

    #[test]
    fn an_unlabelled_text_stream_is_accepted() {
        assert!(accepts(b"just words", &StreamInfo::new()));
    }

    #[test]
    fn latin1_bytes_decode_via_the_declared_charset() {
        let si = StreamInfo::new().with_charset("iso-8859-1");
        let doc = convert(&[b'c', b'a', b'f', 0xE9], &si);
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Paragraph(i) => assert_eq!(i.to_plain(), "café"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_input_yields_an_empty_document_not_an_error() {
        let doc = convert(b"   \n\n  ", &StreamInfo::new());
        assert!(doc.blocks.is_empty());
    }
}
