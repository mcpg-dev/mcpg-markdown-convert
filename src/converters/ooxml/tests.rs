use std::io::Write;

use super::*;
use crate::cx::{Budget, Limits};

/// Build a zip in memory from `(name, body)` pairs. Real Office files are
/// large and binary; the parts under test are the XML, so hand-built
/// containers keep the fixtures readable and reviewable.
pub(crate) fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        for (name, body) in parts {
            w.start_file(*name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

fn run(c: &dyn Converter, bytes: &[u8], ext: &str) -> Document {
    let b = Budget::new(Limits::default());
    let cx = ConvertCx::new(&b);
    c.convert(bytes, &StreamInfo::new().with_extension(ext), &cx)
        .expect("converts")
}

fn err(c: &dyn Converter, bytes: &[u8], ext: &str) -> ConvertError {
    let b = Budget::new(Limits::default());
    let cx = ConvertCx::new(&b);
    c.convert(bytes, &StreamInfo::new().with_extension(ext), &cx)
        .unwrap_err()
}

// ---------------------------------------------------------------------------
// DOCX
// ---------------------------------------------------------------------------

fn docx_body(body: &str) -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0"?>
        <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
          <w:body>{body}</w:body>
        </w:document>"#
    );
    zip_of(&[
        ("[Content_Types].xml", "<Types/>"),
        ("word/document.xml", &doc),
    ])
}

fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
}

fn styled(style: &str, text: &str) -> String {
    format!("<w:p><w:pPr><w:pStyle w:val=\"{style}\"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>")
}

fn blocks_of(doc: &Document) -> String {
    format!("{:?}", doc.blocks)
}

#[test]
fn paragraphs_come_out_in_document_order() {
    let doc = run(
        &DocxConverter,
        &docx_body(&(para("first") + &para("second"))),
        "docx",
    );
    let texts: Vec<String> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Paragraph(i) => Some(i.to_plain()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first", "second"]);
}

#[test]
fn heading_styles_map_to_heading_levels() {
    let body = styled("Title", "T") + &styled("Heading1", "H1") + &styled("Heading 3", "H3");
    let doc = run(&DocxConverter, &docx_body(&body), "docx");
    let levels: Vec<u8> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Heading { level, .. } => Some(*level),
            _ => None,
        })
        .collect();
    assert_eq!(levels, vec![1, 1, 3]);
}

#[test]
fn pretty_printed_ooxml_does_not_gain_phantom_whitespace() {
    // Every fixture in this file is single-line XML, which is what a
    // hand-built one looks like. Real producers pretty-print, so the newline
    // and indentation between <w:r> elements become text nodes — and text
    // nodes are no longer trimmed at the reader. This asserts that the
    // indentation does not reach the output as content.
    let body = "
      <w:p>
        <w:r>
          <w:t>Revenue </w:t>
        </w:r>
        <w:r>
          <w:rPr><w:b/></w:rPr>
          <w:t>rose</w:t>
        </w:r>
      </w:p>
      <w:tbl>
        <w:tr>
          <w:tc>
            <w:p><w:r><w:t>cell</w:t></w:r></w:p>
          </w:tc>
        </w:tr>
      </w:tbl>";
    let doc = run(&DocxConverter, &docx_body(body), "docx");

    match &doc.blocks[0] {
        Block::Paragraph(i) => assert_eq!(i.to_plain(), "Revenue rose"),
        other => panic!("{other:?}"),
    }
    match doc.blocks.iter().find(|b| matches!(b, Block::Table(_))) {
        Some(Block::Table(t)) => assert_eq!(t.header.as_ref().unwrap()[0].to_plain(), "cell"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unknown_style_stays_a_paragraph() {
    let doc = run(&DocxConverter, &docx_body(&styled("BodyText", "x")), "docx");
    assert!(
        matches!(doc.blocks[0], Block::Paragraph(_)),
        "{:?}",
        doc.blocks
    );
}

#[test]
fn list_paragraph_styles_are_recognised_as_lists() {
    // Word and python-docx mark a bulleted list with a paragraph STYLE and
    // put the numbering in styles.xml — there is no inline <w:numPr>. Keying
    // only on numPr read a real bulleted list as plain paragraphs.
    let body = styled("List Bullet", "EMEA up")
        + &styled("List Bullet", "APAC flat")
        + &styled("ListParagraph", "AMER down");
    let doc = run(&DocxConverter, &docx_body(&body), "docx");
    match doc.blocks.iter().find(|b| matches!(b, Block::List { .. })) {
        Some(Block::List { ordered, items }) => {
            assert!(!ordered, "a bulleted list must not become numbered");
            assert_eq!(items.len(), 3, "{items:?}");
        }
        other => panic!("expected a list, got {other:?}"),
    }
}

#[test]
fn numbered_list_styles_are_ordered() {
    let body = styled("ListNumber", "first") + &styled("List Number", "second");
    let doc = run(&DocxConverter, &docx_body(&body), "docx");
    match doc.blocks.iter().find(|b| matches!(b, Block::List { .. })) {
        Some(Block::List { ordered, items }) => {
            assert!(ordered);
            assert_eq!(items.len(), 2);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn body_text_styles_are_not_mistaken_for_lists() {
    for style in ["BodyText", "Normal", "Caption", "Quote"] {
        let doc = run(&DocxConverter, &docx_body(&styled(style, "x")), "docx");
        assert!(
            matches!(doc.blocks[0], Block::Paragraph(_)),
            "{style} became {:?}",
            doc.blocks[0]
        );
    }
}

#[test]
fn consecutive_list_items_merge_into_one_list() {
    let item = |t: &str| {
        format!(
            "<w:p><w:pPr><w:numPr><w:ilvl w:val=\"0\"/></w:numPr></w:pPr><w:r><w:t>{t}</w:t></w:r></w:p>"
        )
    };
    let doc = run(
        &DocxConverter,
        &docx_body(&(item("a") + &item("b"))),
        "docx",
    );
    let lists: Vec<usize> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::List { items, .. } => Some(items.len()),
            _ => None,
        })
        .collect();
    assert_eq!(lists, vec![2], "{:?}", doc.blocks);
}

#[test]
fn spaces_at_run_boundaries_survive() {
    // Word splits a sentence into one <w:t> per formatting change, so the
    // space before a bold word is the last character of the *preceding* run.
    // Trimming text nodes globally welded them together into "Revenuerose".
    let body = "<w:p><w:r><w:t>Revenue </w:t></w:r>\
                <w:r><w:rPr><w:b/></w:rPr><w:t>rose</w:t></w:r>\
                <w:r><w:t> sharply</w:t></w:r></w:p>";
    let doc = run(&DocxConverter, &docx_body(body), "docx");
    match &doc.blocks[0] {
        Block::Paragraph(i) => assert_eq!(i.to_plain(), "Revenue rose sharply"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bold_and_italic_runs_carry_their_emphasis() {
    let body = "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r>\
                <w:r><w:rPr><w:i/></w:rPr><w:t>ital</w:t></w:r></w:p>";
    let out = blocks_of(&run(&DocxConverter, &docx_body(body), "docx"));
    assert!(out.contains("Strong"), "{out}");
    assert!(out.contains("Emphasis"), "{out}");
}

#[test]
fn tables_become_tables_with_the_first_row_as_header() {
    let body = "<w:tbl>\
        <w:tr><w:tc><w:p><w:r><w:t>h1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>h2</w:t></w:r></w:p></w:tc></w:tr>\
        <w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr>\
        </w:tbl>";
    let doc = run(&DocxConverter, &docx_body(body), "docx");
    match doc.blocks.iter().find(|b| matches!(b, Block::Table(_))) {
        Some(Block::Table(t)) => {
            assert_eq!(t.width(), 2, "{t:?}");
            assert_eq!(t.rows.len(), 1);
            assert_eq!(t.header.as_ref().unwrap()[0].to_plain(), "h1");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn table_paragraphs_do_not_also_leak_out_as_body_text() {
    let body = "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>incell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>";
    let doc = run(&DocxConverter, &docx_body(body), "docx");
    let stray = doc.blocks.iter().any(|b| match b {
        Block::Paragraph(i) => i.to_plain().contains("incell"),
        _ => false,
    });
    assert!(
        !stray,
        "cell text duplicated into the body: {:?}",
        doc.blocks
    );
}

#[test]
fn a_cell_with_several_paragraphs_keeps_both() {
    let body = "<w:tbl><w:tr><w:tc>\
        <w:p><w:r><w:t>one</w:t></w:r></w:p>\
        <w:p><w:r><w:t>two</w:t></w:r></w:p>\
        </w:tc></w:tr></w:tbl>";
    let doc = run(&DocxConverter, &docx_body(body), "docx");
    let out = blocks_of(&doc);
    assert!(out.contains("one"), "{out}");
    assert!(out.contains("two"), "{out}");
}

#[test]
fn hyperlinks_resolve_through_the_relationship_part() {
    let doc_xml = r#"<?xml version="1.0"?>
        <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
                    xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
          <w:body><w:p>
            <w:r><w:t>see </w:t></w:r>
            <w:hyperlink r:id="rId7"><w:r><w:t>the docs</w:t></w:r></w:hyperlink>
          </w:p></w:body>
        </w:document>"#;
    let rels = r#"<?xml version="1.0"?>
        <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
          <Relationship Id="rId7" Type="hyperlink" Target="https://example.invalid/docs"/>
        </Relationships>"#;
    let bytes = zip_of(&[
        ("word/document.xml", doc_xml),
        ("word/_rels/document.xml.rels", rels),
    ]);
    let out = blocks_of(&run(&DocxConverter, &bytes, "docx"));
    assert!(out.contains("example.invalid/docs"), "{out}");
    assert!(out.contains("the docs"), "{out}");
}

#[test]
fn an_unresolvable_hyperlink_keeps_its_text() {
    let doc_xml = r#"<?xml version="1.0"?>
        <w:document xmlns:w="http://x" xmlns:r="http://y"><w:body><w:p>
          <w:hyperlink r:id="missing"><w:r><w:t>label</w:t></w:r></w:hyperlink>
        </w:p></w:body></w:document>"#;
    let bytes = zip_of(&[("word/document.xml", doc_xml)]);
    assert!(blocks_of(&run(&DocxConverter, &bytes, "docx")).contains("label"));
}

#[test]
fn core_properties_populate_the_metadata() {
    let core = r#"<?xml version="1.0"?>
        <cp:coreProperties xmlns:cp="http://x" xmlns:dc="http://purl.org/dc/elements/1.1/"
                           xmlns:dcterms="http://purl.org/dc/terms/">
          <dc:title>Quarterly</dc:title>
          <dc:creator>Ada</dc:creator>
          <dcterms:created>2026-01-02T03:04:05Z</dcterms:created>
        </cp:coreProperties>"#;
    let bytes = zip_of(&[
        ("docProps/core.xml", core),
        (
            "word/document.xml",
            r#"<w:document xmlns:w="http://x"><w:body><w:p><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>"#,
        ),
    ]);
    let doc = run(&DocxConverter, &bytes, "docx");
    assert_eq!(doc.title.as_deref(), Some("Quarterly"));
    assert_eq!(doc.metadata.author.as_deref(), Some("Ada"));
    assert_eq!(
        doc.metadata.created.as_deref(),
        Some("2026-01-02T03:04:05Z")
    );
}

#[test]
fn footnotes_are_carried_across() {
    let bytes = zip_of(&[
        (
            "word/document.xml",
            r#"<w:document xmlns:w="http://x"><w:body><w:p><w:r><w:t>body</w:t></w:r></w:p></w:body></w:document>"#,
        ),
        (
            "word/footnotes.xml",
            r#"<w:footnotes xmlns:w="http://x"><w:footnote><w:p><w:r><w:t>a note</w:t></w:r></w:p></w:footnote></w:footnotes>"#,
        ),
    ]);
    assert!(blocks_of(&run(&DocxConverter, &bytes, "docx")).contains("a note"));
}

#[test]
fn a_zip_without_a_document_part_is_a_clear_error() {
    let bytes = zip_of(&[("hello.txt", "hi")]);
    let e = err(&DocxConverter, &bytes, "docx");
    assert_eq!(e.code(), "malformed");
    assert!(format!("{e}").contains("word/document.xml"), "{e}");
}

#[test]
fn a_truncated_container_is_malformed_not_a_panic() {
    let e = err(&DocxConverter, b"PK\x03\x04garbage", "docx");
    assert_eq!(e.code(), "malformed");
}

#[test]
fn an_empty_body_warns_rather_than_returning_silence() {
    let doc = run(&DocxConverter, &docx_body(""), "docx");
    assert!(
        doc.warnings
            .iter()
            .any(|w| w.kind == WarningKind::NoTextLayer),
        "{:?}",
        doc.warnings
    );
}

#[test]
fn expansion_is_charged_against_the_budget() {
    let big = "x".repeat(200_000);
    let bytes = docx_body(&para(&big));
    let b = Budget::new(Limits {
        max_expanded_bytes: 1024,
        ..Limits::default()
    });
    let cx = ConvertCx::new(&b);
    let e = DocxConverter
        .convert(&bytes, &StreamInfo::new().with_extension("docx"), &cx)
        .unwrap_err();
    assert_eq!(e.code(), "limit_exceeded");
}

#[test]
fn docx_declines_a_non_zip() {
    assert!(!DocxConverter.accepts(
        &Probe::new(b"not a zip"),
        &StreamInfo::new().with_extension("docx")
    ));
}

// ---------------------------------------------------------------------------
// PPTX
// ---------------------------------------------------------------------------

fn slide(text: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
        <p:sld xmlns:p="http://x" xmlns:a="http://y"><p:cSld><p:spTree>
          <p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>
        </p:spTree></p:cSld></p:sld>"#
    )
}

#[test]
fn slides_are_ordered_numerically_not_lexically() {
    let s2 = slide("second");
    let s10 = slide("tenth");
    let s1 = slide("first");
    let bytes = zip_of(&[
        ("ppt/slides/slide10.xml", &s10),
        ("ppt/slides/slide2.xml", &s2),
        ("ppt/slides/slide1.xml", &s1),
    ]);
    let doc = run(&PptxConverter, &bytes, "pptx");
    let headings: Vec<String> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Heading { text, .. } => Some(text.to_plain()),
            _ => None,
        })
        .collect();
    assert_eq!(headings, vec!["Slide 1", "Slide 2", "Slide 10"]);
}

#[test]
fn slide_text_is_extracted() {
    let s = slide("Agenda");
    let bytes = zip_of(&[("ppt/slides/slide1.xml", &s)]);
    assert!(blocks_of(&run(&PptxConverter, &bytes, "pptx")).contains("Agenda"));
}

#[test]
fn speaker_notes_ride_along_as_a_quote() {
    let s = slide("Body");
    let notes = r#"<?xml version="1.0"?>
        <p:notes xmlns:p="http://x" xmlns:a="http://y">
          <a:p><a:r><a:t>remember the demo</a:t></a:r></a:p>
        </p:notes>"#;
    let bytes = zip_of(&[
        ("ppt/slides/slide1.xml", &s),
        ("ppt/notesSlides/notesSlide1.xml", notes),
    ]);
    let doc = run(&PptxConverter, &bytes, "pptx");
    let out = blocks_of(&doc);
    assert!(out.contains("remember the demo"), "{out}");
    assert!(doc.blocks.iter().any(|b| matches!(b, Block::Quote(_))));
}

#[test]
fn slide_tables_become_tables() {
    let s = r#"<?xml version="1.0"?>
        <p:sld xmlns:p="http://x" xmlns:a="http://y"><a:tbl>
          <a:tr><a:tc><a:p><a:r><a:t>h</a:t></a:r></a:p></a:tc></a:tr>
          <a:tr><a:tc><a:p><a:r><a:t>v</a:t></a:r></a:p></a:tc></a:tr>
        </a:tbl></p:sld>"#;
    let bytes = zip_of(&[("ppt/slides/slide1.xml", s)]);
    let doc = run(&PptxConverter, &bytes, "pptx");
    assert!(
        doc.blocks.iter().any(|b| matches!(b, Block::Table(_))),
        "{:?}",
        doc.blocks
    );
}

#[test]
fn a_zip_with_no_slides_is_a_clear_error() {
    let bytes = zip_of(&[("word/document.xml", "<w:document/>")]);
    let e = err(&PptxConverter, &bytes, "pptx");
    assert!(format!("{e}").contains("slide"), "{e}");
}
