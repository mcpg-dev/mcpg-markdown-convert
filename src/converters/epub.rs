//! EPUB — chapters in spine order.
//!
//! An EPUB is a zip of XHTML plus an OPF manifest. Reading the spine matters:
//! the zip's entry order says nothing about reading order, so converting
//! members as they appear (which is what a generic archive walk would do)
//! produces a book with its chapters shuffled.

use crate::converters::ooxml::{open, read_part};
use crate::converters::squeeze;
use crate::converters::xml::read_events;
use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document, Inline};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct EpubConverter;

impl Converter for EpubConverter {
    fn name(&self) -> &'static str {
        "epub"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        probe.starts_with(b"PK\x03\x04")
            && (info.is_ext("epub") || info.is_mime("application/epub+zip"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        _info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let mut zip = open(bytes, "epub")?;

        let container =
            read_part(&mut zip, "META-INF/container.xml", cx)?.ok_or(ConvertError::Malformed {
                format: "epub",
                message: "no META-INF/container.xml — not an EPUB".to_owned(),
            })?;
        let opf_path = rootfile_path(&container)?.ok_or(ConvertError::Malformed {
            format: "epub",
            message: "container.xml names no rootfile".to_owned(),
        })?;

        let opf = read_part(&mut zip, &opf_path, cx)?.ok_or(ConvertError::Malformed {
            format: "epub",
            message: format!("rootfile {opf_path} is missing"),
        })?;
        let base = opf_path
            .rsplit_once('/')
            .map_or(String::new(), |(dir, _)| format!("{dir}/"));

        let mut doc = Document::new();
        let package = parse_package(&opf)?;
        if let Some(t) = package.title {
            doc = doc.with_title(t);
        }
        doc.metadata.author = package.creator;
        doc.metadata.language = package.language;
        if let Some(d) = package.date {
            doc.metadata.created = Some(d);
        }

        if package.spine.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                "the EPUB spine is empty",
            ));
            return Ok(doc);
        }

        for idref in &package.spine {
            cx.budget().check_deadline()?;
            // The nav document is in the spine but is not content. Skipping it
            // silently: it is not a degradation, it is the correct reading.
            if package.nav_ids.iter().any(|id| id == idref) {
                continue;
            }
            let Some(href) = package
                .manifest
                .iter()
                .find(|(id, _)| id == idref)
                .map(|(_, h)| h.clone())
            else {
                doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!("spine item {idref:?} is not in the manifest"),
                ));
                continue;
            };

            let path = normalise(&format!("{base}{href}"));
            let Some(xhtml) = read_part(&mut zip, &path, cx)? else {
                doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!("chapter {path} is missing from the container"),
                ));
                continue;
            };

            for b in chapter_blocks(&xhtml)? {
                doc.push(b);
            }
        }

        if doc.blocks.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                "no chapter in the spine carried text",
            ));
        }
        Ok(doc)
    }
}

/// `<rootfile full-path="...">` from the container.
fn rootfile_path(xml: &str) -> Result<Option<String>, ConvertError> {
    let mut path = None;
    read_events(xml, "epub", |n| {
        if n.name() == "rootfile" && path.is_none() {
            path = n
                .attrs
                .iter()
                .find(|(k, _)| k == "full-path")
                .map(|(_, v)| v.clone());
        }
    })?;
    Ok(path)
}

#[derive(Default)]
struct Package {
    title: Option<String>,
    creator: Option<String>,
    language: Option<String>,
    date: Option<String>,
    /// id → href, excluding the navigation document.
    manifest: Vec<(String, String)>,
    /// Manifest ids that are navigation, not content.
    nav_ids: Vec<String>,
    /// idrefs, in reading order.
    spine: Vec<String>,
}

fn parse_package(xml: &str) -> Result<Package, ConvertError> {
    let mut p = Package::default();
    read_events(xml, "epub", |n| {
        let get = |k: &str| n.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        match n.name() {
            "title" if p.title.is_none() => p.title = Some(squeeze(&n.own_text)),
            "creator" if p.creator.is_none() => p.creator = Some(squeeze(&n.own_text)),
            "language" if p.language.is_none() => p.language = Some(squeeze(&n.own_text)),
            "date" if p.date.is_none() => p.date = Some(squeeze(&n.own_text)),
            "item" => {
                if let (Some(id), Some(href)) = (get("id"), get("href")) {
                    // EPUB 3 puts the navigation document in the spine like
                    // any chapter. Converting it emits the table of contents
                    // as prose, immediately followed by the chapters it lists.
                    let is_nav = get("properties")
                        .is_some_and(|p| p.split_whitespace().any(|t| t == "nav"))
                        || get("media-type").as_deref() == Some("application/x-dtbncx+xml");
                    if is_nav {
                        p.nav_ids.push(id);
                    } else {
                        p.manifest.push((id, href));
                    }
                }
            }
            "itemref" => {
                if let Some(idref) = get("idref") {
                    p.spine.push(idref);
                }
            }
            _ => {}
        }
    })?;
    p.title = p.title.filter(|t| !t.is_empty());
    p.creator = p.creator.filter(|t| !t.is_empty());
    p.language = p.language.filter(|t| !t.is_empty());
    p.date = p.date.filter(|t| !t.is_empty());
    Ok(p)
}

/// One chapter's XHTML → blocks.
///
/// Headings and paragraphs only. Going through `htmd` would give richer
/// output but would also make EPUB depend on the `web` feature; keeping the
/// two independent means a build with `office` but not `web` still reads
/// books.
fn chapter_blocks(xhtml: &str) -> Result<Vec<Block>, ConvertError> {
    let mut blocks = Vec::new();
    read_events(xhtml, "epub", |n| {
        let name = n.name();
        let text = squeeze(&n.text);
        if text.is_empty() {
            return;
        }
        if let Some(level) = name
            .strip_prefix('h')
            .and_then(|d| d.parse::<u8>().ok())
            .filter(|l| (1..=6).contains(l))
        {
            blocks.push(Block::Heading {
                level,
                text: Inline::text(text),
            });
            return;
        }
        if matches!(name, "p" | "blockquote" | "li") {
            // Leaf paragraphs only: a `<div>` wrapping ten of them would
            // otherwise repeat all ten.
            blocks.push(Block::Paragraph(Inline::text(text)));
        }
    })?;
    Ok(blocks)
}

/// Resolve `../` inside a manifest href. EPUB paths are container-relative
/// and hand-written by authoring tools, so `OEBPS/../text/ch1.xhtml` happens.
fn normalise(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converters::ooxml::tests::zip_of;
    use crate::cx::{Budget, Limits};

    const CONTAINER: &str = r#"<?xml version="1.0"?>
        <container xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
          <rootfiles><rootfile full-path="OEBPS/content.opf"
            media-type="application/oebps-package+xml"/></rootfiles>
        </container>"#;

    fn opf(spine: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
            <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>A Book</dc:title>
                <dc:creator>Ada</dc:creator>
                <dc:language>en</dc:language>
              </metadata>
              <manifest>
                <item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
                <item id="c2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
              </manifest>
              <spine>{spine}</spine>
            </package>"#
        )
    }

    fn chapter(title: &str, body: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
            <html xmlns="http://www.w3.org/1999/xhtml"><body>
              <h1>{title}</h1><p>{body}</p>
            </body></html>"#
        )
    }

    fn book(spine: &str) -> Vec<u8> {
        let one = chapter("Chapter One", "First words.");
        let two = chapter("Chapter Two", "Second words.");
        let package = opf(spine);
        zip_of(&[
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", &package),
            ("OEBPS/ch1.xhtml", &one),
            ("OEBPS/ch2.xhtml", &two),
        ])
    }

    fn convert(bytes: &[u8]) -> Document {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        EpubConverter
            .convert(bytes, &StreamInfo::new().with_extension("epub"), &cx)
            .expect("converts")
    }

    fn headings(doc: &Document) -> Vec<String> {
        doc.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { text, .. } => Some(text.to_plain()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn chapters_follow_the_spine_not_the_zip_order() {
        // Spine says chapter two first. A generic archive walk would give the
        // opposite, which is exactly the bug this converter exists to avoid.
        let doc = convert(&book(r#"<itemref idref="c2"/><itemref idref="c1"/>"#));
        assert_eq!(headings(&doc), vec!["Chapter Two", "Chapter One"]);
    }

    #[test]
    fn the_navigation_document_is_not_emitted_as_a_chapter() {
        // EPUB 3 puts nav in the spine like any chapter. Converting it emits
        // the table of contents as prose, immediately followed by the very
        // chapters it lists.
        let one = chapter("Chapter One", "First words.");
        let nav = r#"<?xml version="1.0"?>
            <html xmlns:epub="http://www.idpf.org/2007/ops"><body>
              <nav epub:type="toc"><ol><li>Chapter One</li></ol></nav>
            </body></html>"#;
        let package = r#"<?xml version="1.0"?>
            <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
              <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
                <dc:title>A Book</dc:title>
              </metadata>
              <manifest>
                <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml"
                      properties="nav"/>
                <item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
              </manifest>
              <spine><itemref idref="nav"/><itemref idref="c1"/></spine>
            </package>"#;
        let bytes = zip_of(&[
            ("META-INF/container.xml", CONTAINER),
            ("OEBPS/content.opf", package),
            ("OEBPS/nav.xhtml", nav),
            ("OEBPS/ch1.xhtml", &one),
        ]);
        let doc = convert(&bytes);
        assert_eq!(headings(&doc), vec!["Chapter One"], "{:?}", doc.blocks);
        // Skipping nav is the correct reading, not a degradation.
        assert!(
            !doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::SkippedMember),
            "{:?}",
            doc.warnings
        );
    }

    #[test]
    fn metadata_comes_from_the_opf() {
        let doc = convert(&book(r#"<itemref idref="c1"/>"#));
        assert_eq!(doc.title.as_deref(), Some("A Book"));
        assert_eq!(doc.metadata.author.as_deref(), Some("Ada"));
        assert_eq!(doc.metadata.language.as_deref(), Some("en"));
    }

    #[test]
    fn chapter_prose_is_extracted_once() {
        let doc = convert(&book(r#"<itemref idref="c1"/>"#));
        let paras: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(i) => Some(i.to_plain()),
                _ => None,
            })
            .collect();
        assert_eq!(paras, vec!["First words."], "{paras:?}");
    }

    #[test]
    fn a_missing_spine_target_is_a_warning_not_a_failure() {
        let doc = convert(&book(r#"<itemref idref="nope"/><itemref idref="c1"/>"#));
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::SkippedMember),
            "{:?}",
            doc.warnings
        );
        assert_eq!(headings(&doc), vec!["Chapter One"]);
    }

    #[test]
    fn an_empty_spine_warns() {
        let doc = convert(&book(""));
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::NoTextLayer)
        );
    }

    #[test]
    fn a_zip_without_a_container_is_a_clear_error() {
        let bytes = zip_of(&[("random.txt", "x")]);
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        let e = EpubConverter
            .convert(&bytes, &StreamInfo::new().with_extension("epub"), &cx)
            .unwrap_err();
        assert!(format!("{e}").contains("container.xml"), "{e}");
    }

    #[test]
    fn relative_segments_in_an_href_resolve() {
        assert_eq!(normalise("OEBPS/../text/ch1.xhtml"), "text/ch1.xhtml");
        assert_eq!(normalise("OEBPS/./ch1.xhtml"), "OEBPS/ch1.xhtml");
    }
}
