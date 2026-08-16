//! Deterministic document builders for tests.
//!
//! Binary formats are *built*, not committed. A checked-in `.docx` is an
//! opaque blob nobody reviews, it inflates the OSS mirror, and when a test
//! that reads it fails there is no way to see what the input actually said.
//! These builders keep every fixture readable in the diff.
//!
//! Determinism matters as much as readability: the golden corpus compares
//! rendered output byte-for-byte, so a builder that varied its output — a
//! timestamp in a zip entry, a hash-ordered attribute — would make the
//! corpus flap.

#![cfg(test)]

use std::io::{Cursor, Write};

/// A zip archive with the given entries, stored in order.
pub(crate) fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in parts {
            w.start_file(*name, opts).expect("start entry");
            w.write_all(body).expect("write entry");
        }
        w.finish().expect("finish archive");
    }
    buf
}

/// Text entries, for the common case.
pub(crate) fn zip_of_text(parts: &[(&str, &str)]) -> Vec<u8> {
    let owned: Vec<(&str, &[u8])> = parts.iter().map(|(n, b)| (*n, b.as_bytes())).collect();
    zip_of(&owned)
}

/// A `.docx` wrapping the given `<w:body>` content, with optional core
/// properties and relationships.
pub(crate) fn docx(body: &str) -> Vec<u8> {
    docx_full(body, None, None)
}

pub(crate) fn docx_full(body: &str, core: Option<&str>, rels: Option<&str>) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <w:body>{body}</w:body>
</w:document>"#
    );
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", b"<Types/>"),
        ("word/document.xml", document.as_bytes()),
    ];
    if let Some(c) = core {
        parts.push(("docProps/core.xml", c.as_bytes()));
    }
    if let Some(r) = rels {
        parts.push(("word/_rels/document.xml.rels", r.as_bytes()));
    }
    zip_of(&parts)
}

/// A `.pptx` from one XML body per slide, plus optional notes keyed by slide
/// number.
pub(crate) fn pptx(slides: &[&str], notes: &[(u32, &str)]) -> Vec<u8> {
    let mut names: Vec<String> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    for (i, body) in slides.iter().enumerate() {
        names.push(format!("ppt/slides/slide{}.xml", i + 1));
        bodies.push(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">{body}</p:sld>"#
        ));
    }
    for (n, body) in notes {
        names.push(format!("ppt/notesSlides/notesSlide{n}.xml"));
        bodies.push(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
         xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">{body}</p:notes>"#
        ));
    }
    let parts: Vec<(&str, &[u8])> = names
        .iter()
        .zip(bodies.iter())
        .map(|(n, b)| (n.as_str(), b.as_bytes()))
        .collect();
    zip_of(&parts)
}

/// One PPTX text paragraph.
pub(crate) fn slide_text(lines: &[&str]) -> String {
    let paragraphs: String = lines
        .iter()
        .map(|l| format!("<a:p><a:r><a:t>{l}</a:t></a:r></a:p>"))
        .collect();
    format!("<p:cSld><p:spTree><p:sp><p:txBody>{paragraphs}</p:txBody></p:sp></p:spTree></p:cSld>")
}

/// An EPUB with the given chapters, in the given spine order.
///
/// `spine` names chapter ids, so a test can put the chapters out of zip order
/// and prove the converter follows the spine rather than the archive.
pub(crate) fn epub(chapters: &[(&str, &str, &str)], spine: &[&str]) -> Vec<u8> {
    let container = r#"<?xml version="1.0" encoding="UTF-8"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles><rootfile full-path="OEBPS/content.opf"
    media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    let manifest: String = chapters
        .iter()
        .map(|(id, _, _)| {
            format!(r#"<item id="{id}" href="{id}.xhtml" media-type="application/xhtml+xml"/>"#)
        })
        .collect();
    let itemrefs: String = spine
        .iter()
        .map(|id| format!(r#"<itemref idref="{id}"/>"#))
        .collect();
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>A Short Book</dc:title>
    <dc:creator>Ada Lovelace</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>{manifest}</manifest>
  <spine>{itemrefs}</spine>
</package>"#
    );

    let mut names = vec![
        "mimetype".to_owned(),
        "META-INF/container.xml".to_owned(),
        "OEBPS/content.opf".to_owned(),
    ];
    let mut bodies = vec!["application/epub+zip".to_owned(), container.to_owned(), opf];
    for (id, title, prose) in chapters {
        names.push(format!("OEBPS/{id}.xhtml"));
        bodies.push(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>{title}</h1><p>{prose}</p></body></html>"#
        ));
    }
    let parts: Vec<(&str, &[u8])> = names
        .iter()
        .zip(bodies.iter())
        .map(|(n, b)| (n.as_str(), b.as_bytes()))
        .collect();
    zip_of(&parts)
}

/// The smallest structurally valid PDF that draws the given lines: catalog,
/// page tree, one page, one content stream.
pub(crate) fn pdf(lines: &[&str]) -> Vec<u8> {
    let mut content = String::from("BT /F1 12 Tf 72 720 Td");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            content.push_str(" 0 -18 Td");
        }
        content.push_str(&format!(" ({line}) Tj"));
    }
    content.push_str(" ET");

    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_owned(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_owned(),
    ];

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

/// A 1×1 transparent PNG. No EXIF — the common case for a generated image,
/// and the one where the converter has nothing but the filename to report.
#[cfg(feature = "media")]
pub(crate) fn png() -> Vec<u8> {
    vec![
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, // signature
        0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R', // IHDR length + type
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1×1
        0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, // bit depth, colour, CRC
        0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82, // IEND
    ]
}

/// A minimal PCM WAV: RIFF/WAVE container, 8 kHz mono 8-bit, `samples` bytes
/// of silence. Enough for `lofty` to read real duration and sample-rate
/// properties, which is what the audio converter reports.
#[cfg(feature = "media")]
pub(crate) fn wav(samples: u32) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 8000;
    let data_len = samples;
    let riff_len = 36 + data_len;

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // format: PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels: mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes()); // byte rate (8-bit mono)
    out.extend_from_slice(&1u16.to_le_bytes()); // block align
    out.extend_from_slice(&8u16.to_le_bytes()); // bits per sample

    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    // 8-bit PCM silence is 0x80, not 0x00 — it is unsigned and centred.
    out.resize(44 + data_len as usize, 0x80);
    out
}

/// An OLE compound file holding the given MAPI property streams.
#[cfg(feature = "email")]
pub(crate) fn msg(props: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut comp = cfb::OpenOptions::new()
        .create_with(Cursor::new(Vec::new()))
        .expect("create compound file");
    for (name, body) in props {
        let mut s = comp
            .create_stream(format!("/{name}"))
            .expect("create stream");
        s.write_all(body).expect("write stream");
        s.flush().expect("flush stream");
    }
    comp.into_inner().into_inner()
}

/// UTF-16LE bytes, the encoding MAPI `001F` properties use.
#[cfg(feature = "email")]
pub(crate) fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_are_deterministic() {
        // The golden corpus diffs rendered output byte-for-byte. A builder
        // that varied between calls would make it flap rather than fail.
        assert_eq!(docx("<w:p/>"), docx("<w:p/>"));
        assert_eq!(pdf(&["a"]), pdf(&["a"]));
        assert_eq!(zip_of_text(&[("a", "b")]), zip_of_text(&[("a", "b")]));
    }

    #[test]
    fn the_docx_builder_produces_a_readable_container() {
        let bytes = docx("<w:p><w:r><w:t>hi</w:t></w:r></w:p>");
        assert!(bytes.starts_with(b"PK\x03\x04"));
        let mut z = zip::ZipArchive::new(Cursor::new(bytes)).expect("valid zip");
        assert!(z.by_name("word/document.xml").is_ok());
    }

    #[test]
    fn the_pdf_builder_produces_a_loadable_document() {
        let bytes = pdf(&["hello"]);
        assert!(bytes.starts_with(b"%PDF-"));
        assert!(bytes.ends_with(b"%%EOF\n"));
    }
}
