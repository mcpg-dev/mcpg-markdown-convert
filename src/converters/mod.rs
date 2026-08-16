//! The built-in converters.
//!
//! One module per format family, each registering at a priority that says how
//! specific it is. Anything at [`PRIORITY_GENERIC`] is a near-catch-all and
//! must be careful in `accepts`: a converter that says yes too readily starves
//! the ones behind it.

use std::sync::Arc;

use crate::registry::ConverterRegistry;

pub mod csv;
pub mod json;
pub mod text;
pub mod xml;

#[cfg(feature = "web")]
pub mod html;

#[cfg(feature = "office")]
pub mod epub;
#[cfg(feature = "office")]
pub mod ooxml;
#[cfg(feature = "office")]
pub mod sheet;
#[cfg(feature = "office")]
pub mod zip;

#[cfg(feature = "pdf")]
pub mod pdf;

#[cfg(feature = "media")]
pub mod media;

#[cfg(feature = "email")]
pub mod msg;

/// Every converter compiled into this build, in registration order.
///
/// Registration order matters within a priority tier: the registry tries the
/// most recently registered first. Generic converters are registered *first*
/// here so that, within their tier, the more discriminating of them still
/// wins — plain text is registered before HTML for exactly that reason.
#[must_use]
pub fn builtin_registry() -> ConverterRegistry {
    let mut r = ConverterRegistry::new();

    // Generic tier, least discriminating first.
    r.register(Arc::new(text::TextConverter));
    #[cfg(feature = "office")]
    r.register(Arc::new(zip::ZipConverter));
    #[cfg(feature = "web")]
    r.register(Arc::new(html::HtmlConverter));

    // Specific tier.
    r.register(Arc::new(csv::CsvConverter));
    r.register(Arc::new(json::JsonConverter));
    r.register(Arc::new(json::IpynbConverter));
    r.register(Arc::new(xml::XmlConverter));
    r.register(Arc::new(xml::FeedConverter));

    #[cfg(feature = "office")]
    {
        r.register(Arc::new(ooxml::DocxConverter));
        r.register(Arc::new(ooxml::PptxConverter));
        r.register(Arc::new(sheet::SpreadsheetConverter));
        r.register(Arc::new(epub::EpubConverter));
    }
    #[cfg(feature = "pdf")]
    r.register(Arc::new(pdf::PdfConverter));
    #[cfg(feature = "media")]
    {
        r.register(Arc::new(media::ImageConverter));
        r.register(Arc::new(media::AudioConverter));
    }
    #[cfg(feature = "email")]
    r.register(Arc::new(msg::OutlookMsgConverter));

    r
}

/// Decode bytes to text using the declared charset, falling back to
/// detection. Mirrors what markitdown gets from `charset_normalizer`, with
/// the same posture: never fail, because a document with a few undecodable
/// bytes is still worth converting.
#[must_use]
pub fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    let label = match charset {
        Some(c) if !c.is_empty() => c.to_owned(),
        _ => crate::detect::sniff_charset(bytes),
    };
    let enc = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = enc.decode(bytes);
    // Strip a BOM the decoder left in place; it would otherwise show up as a
    // zero-width space at the head of the first heading.
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}

/// Collapse runs of whitespace and trim. Document formats are full of
/// soft-wrapped runs and stray tabs that carry no meaning in Markdown.
#[must_use]
pub fn squeeze(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            in_space = true;
        } else {
            if in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = false;
            out.push(c);
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_honours_a_declared_charset() {
        // "é" in latin-1 is a single 0xE9 byte, invalid UTF-8.
        assert_eq!(decode_text(&[0xE9], Some("iso-8859-1")), "é");
    }

    #[test]
    fn decode_falls_back_to_detection() {
        assert_eq!(decode_text("héllo".as_bytes(), None), "héllo");
    }

    #[test]
    fn decode_never_fails_on_broken_bytes() {
        let out = decode_text(&[0xFF, 0xFE, 0x00, 0x41], Some("utf-8"));
        assert!(!out.is_empty());
    }

    #[test]
    fn decode_strips_a_leading_bom() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi", Some("utf-8")), "hi");
    }

    #[test]
    fn squeeze_collapses_runs() {
        assert_eq!(squeeze("  a \n\t b  "), "a b");
        assert_eq!(squeeze(""), "");
    }

    #[test]
    fn the_registry_registers_something() {
        let r = builtin_registry();
        assert!(!r.is_empty());
        assert!(r.names().contains(&"text"));
    }

    #[test]
    fn converter_names_are_unique() {
        let names = builtin_registry().names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate converter name in {names:?}"
        );
    }
}
