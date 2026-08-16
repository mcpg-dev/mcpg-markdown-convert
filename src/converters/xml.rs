//! XML, and the RSS / Atom feeds built on it.
//!
//! **External entities are never resolved.** `quick-xml` does not fetch them
//! and we never hand it a resolver, so `<!ENTITY xxe SYSTEM "file:///etc/passwd">`
//! expands to nothing. DOCTYPE declarations are read and discarded, which also
//! closes the billion-laughs expansion. Every XML-derived converter in this
//! crate — OOXML, EPUB, feeds — goes through [`read_events`] for that reason.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::converters::{decode_text, squeeze};
use crate::cx::ConvertCx;
use crate::error::ConvertError;
use crate::ir::{Block, Document, Inline, Span};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

/// One flattened XML node.
#[derive(Debug, Clone, Default)]
pub struct Node {
    /// Element names from the root down, lower-cased and namespace-stripped.
    pub path: Vec<String>,
    /// Text directly inside this element, excluding its children's.
    pub own_text: String,
    /// This element's text plus every descendant's, in document order. What
    /// a feed's `<description>` needs when it contains inline markup.
    pub text: String,
    pub attrs: Vec<(String, String)>,
}

impl Node {
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.path.len()
    }

    /// True when any ancestor (or this element) is named `name`.
    ///
    /// Feeds nest their entry fields at different depths — RSS puts a title
    /// at `rss/channel/item/title`, Atom at `feed/entry/title` — so asking
    /// about the path is right where asking about the depth is a guess.
    #[must_use]
    pub fn under(&self, name: &str) -> bool {
        self.path.iter().any(|p| p == name)
    }
}

/// Walk an XML document, calling `visit` for every element that closed.
///
/// The single entry point for XML in this crate, so the entity posture above
/// is stated and enforced in one place rather than repeated per converter.
pub fn read_events(
    xml: &str,
    format: &'static str,
    mut visit: impl FnMut(&Node),
) -> Result<(), ConvertError> {
    let mut reader = Reader::from_str(xml);
    let config = reader.config_mut();
    // Malformed markup is the norm in the wild; a mismatched close tag should
    // not throw away a document we could otherwise read.
    config.check_end_names = false;
    // Text is NOT trimmed here. OOXML splits a sentence into one `<w:t>` per
    // formatting change, so the space before a bold word lives at the end of
    // the preceding run — trimming globally welds "Revenue " and "rose" into
    // "Revenuerose". Callers that want whitespace collapsed apply `squeeze`
    // to the text they actually use; callers that need it verbatim, like the
    // DOCX run walker, now get it.
    config.trim_text(false);

    let mut stack: Vec<Node> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                let mut path: Vec<String> =
                    stack.last().map(|n| n.path.clone()).unwrap_or_default();
                path.push(name);
                stack.push(Node {
                    path,
                    own_text: String::new(),
                    text: String::new(),
                    attrs: attributes(&e),
                });
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                let mut path: Vec<String> =
                    stack.last().map(|n| n.path.clone()).unwrap_or_default();
                path.push(name);
                visit(&Node {
                    path,
                    own_text: String::new(),
                    text: String::new(),
                    attrs: attributes(&e),
                });
            }
            Ok(Event::Text(t)) => {
                if let Some(top) = stack.last_mut() {
                    let raw = t.decode().unwrap_or_default();
                    top.own_text.push_str(raw.as_ref());
                    top.text.push_str(raw.as_ref());
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(top) = stack.last_mut() {
                    let raw = String::from_utf8_lossy(t.into_inner().as_ref()).into_owned();
                    top.own_text.push_str(&raw);
                    top.text.push_str(&raw);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(node) = stack.pop() {
                    // A parent with mixed content still wants its children's
                    // words — otherwise `<p>a<b>c</b>d</p>` loses "c" — but
                    // only in `text`, so a converter that wants leaves can
                    // still find them via `own_text`.
                    if let Some(parent) = stack.last_mut()
                        && !node.text.trim().is_empty()
                    {
                        parent.text.push(' ');
                        parent.text.push_str(&node.text);
                    }
                    visit(&node);
                }
            }
            Ok(Event::Eof) => break,
            // DOCTYPE, comments and processing instructions are read and
            // dropped. Dropping the DTD is what closes XXE and billion-laughs.
            Ok(_) => {}
            Err(e) => {
                return Err(ConvertError::Malformed {
                    format,
                    message: e.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or(&s).to_ascii_lowercase()
}

fn attributes(e: &quick_xml::events::BytesStart<'_>) -> Vec<(String, String)> {
    e.attributes()
        .filter_map(Result::ok)
        .map(|a| {
            (
                local_name(a.key.as_ref()),
                a.unescape_value()
                    .map(|v| v.into_owned())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// Generic XML → nested headings plus the leaf text under them.
pub struct XmlConverter;

impl Converter for XmlConverter {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC + 10
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        if !probe.looks_textual() {
            return false;
        }
        if is_feed_like(info) {
            return false;
        }
        info.is_ext("xml")
            || info.is_mime("application/xml")
            || info.is_mime("text/xml")
            || (info.extension.is_none() && probe.leading_text(64).starts_with("<?xml"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let text = decode_text(bytes, info.charset.as_deref());
        let mut doc = Document::new();
        let mut blocks: Vec<Block> = Vec::new();

        read_events(&text, "xml", |node| {
            // Leaves only, keyed on `own_text`. Emitting every element would
            // repeat each leaf's words once per ancestor, because a parent
            // accumulates its children's text.
            let content = squeeze(&node.own_text);
            if content.is_empty() && node.attrs.is_empty() {
                return;
            }
            if !content.is_empty() {
                blocks.push(Block::Paragraph(Inline(vec![
                    Span::Strong(Inline::text(node.name().to_owned())),
                    Span::Text(": ".to_owned()),
                    Span::Text(content),
                ])));
            } else {
                // Elements with only attributes are still worth a line — XML
                // often puts the actual data there.
                let attrs = node
                    .attrs
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                blocks.push(Block::Paragraph(Inline(vec![
                    Span::Strong(Inline::text(node.name().to_owned())),
                    Span::Text(format!(" ({attrs})")),
                ])));
            }
        })?;

        cx.budget().check_deadline()?;
        // Leaves close in document order, so no reordering is needed.
        for b in blocks {
            doc.push(b);
        }
        Ok(doc)
    }
}

fn is_feed_like(info: &StreamInfo) -> bool {
    info.is_ext("rss")
        || info.is_ext("atom")
        || info.is_mime("application/rss+xml")
        || info.is_mime("application/atom+xml")
}

/// RSS 2.0 and Atom. One heading per entry, with the link and date beneath.
pub struct FeedConverter;

impl Converter for FeedConverter {
    fn name(&self) -> &'static str {
        "feed"
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        if !probe.looks_textual() {
            return false;
        }
        if is_feed_like(info) {
            return true;
        }
        // A `.xml` that is actually a feed is common enough to sniff for.
        (info.is_ext("xml") || info.is_mime("application/xml") || info.is_mime("text/xml"))
            && (probe.contains(b"<rss") || probe.contains(b"<feed") || probe.contains(b"<channel"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let text = decode_text(bytes, info.charset.as_deref());

        let mut feed_title = None;
        let mut entries: Vec<Entry> = Vec::new();
        let mut current = Entry::default();

        read_events(&text, "feed", |node| {
            let name = node.name();
            let content = squeeze(&node.text);
            // RSS nests an entry at rss/channel/item, Atom at feed/entry —
            // different depths, same question.
            let in_entry = node.under("item") || node.under("entry");
            match name {
                // An entry closes after its own children, so `current` is
                // already complete here. That also keeps entries in document
                // order without any post-hoc reversal.
                "item" | "entry" => {
                    if !current.is_empty() {
                        entries.push(std::mem::take(&mut current));
                    }
                }
                "title" => {
                    if in_entry {
                        if current.title.is_empty() {
                            current.title = content;
                        }
                    } else if feed_title.is_none() {
                        feed_title = Some(content);
                    }
                }
                "link" => {
                    // Atom carries the target in `href`; RSS in the body.
                    let href = node
                        .attrs
                        .iter()
                        .find(|(k, _)| k == "href")
                        .map(|(_, v)| v.clone())
                        .unwrap_or(content);
                    if in_entry && current.link.is_empty() {
                        current.link = href;
                    }
                }
                "description" | "summary" | "content" => {
                    if in_entry && current.summary.is_empty() {
                        current.summary = content;
                    }
                }
                "pubdate" | "published" | "updated" if in_entry && current.date.is_empty() => {
                    current.date = content;
                }
                _ => {}
            }
        })?;
        if !current.is_empty() {
            entries.push(current);
        }
        cx.budget().check_deadline()?;

        let mut doc = Document::new();
        if let Some(t) = feed_title {
            doc = doc.with_title(t);
        }
        for e in entries {
            if !e.title.is_empty() {
                doc.push(Block::Heading {
                    level: 2,
                    text: Inline::text(e.title),
                });
            }
            if !e.link.is_empty() {
                doc.push(Block::Paragraph(Inline(vec![Span::Link {
                    text: Inline::text(e.link.clone()),
                    href: e.link,
                }])));
            }
            if !e.date.is_empty() {
                doc.push(Block::Paragraph(Inline(vec![Span::Emphasis(
                    Inline::text(e.date),
                )])));
            }
            if !e.summary.is_empty() {
                doc.push(Block::Paragraph(Inline::text(e.summary)));
            }
        }
        Ok(doc)
    }
}

#[derive(Debug, Default)]
struct Entry {
    title: String,
    link: String,
    summary: String,
    date: String,
}

impl Entry {
    fn is_empty(&self) -> bool {
        self.title.is_empty() && self.link.is_empty() && self.summary.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    fn run(c: &dyn Converter, bytes: &[u8], info: &StreamInfo) -> Document {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        c.convert(bytes, info, &cx).expect("converts")
    }

    fn plain(doc: &Document) -> String {
        doc.blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph(i) | Block::Heading { text: i, .. } => i.to_plain(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn external_entities_are_never_resolved() {
        let xxe = r#"<?xml version="1.0"?>
            <!DOCTYPE foo [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
            <root><data>&xxe;</data></root>"#;
        let doc = run(
            &XmlConverter,
            xxe.as_bytes(),
            &StreamInfo::new().with_extension("xml"),
        );
        let out = plain(&doc);
        assert!(!out.contains("root:"), "entity was expanded: {out}");
        assert!(!out.contains("/etc/passwd"), "entity was expanded: {out}");
    }

    #[test]
    fn billion_laughs_does_not_expand() {
        let bomb = r#"<?xml version="1.0"?>
            <!DOCTYPE lolz [
              <!ENTITY lol "lol">
              <!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
              <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
            ]>
            <lolz>&lol3;</lolz>"#;
        let doc = run(
            &XmlConverter,
            bomb.as_bytes(),
            &StreamInfo::new().with_extension("xml"),
        );
        assert!(plain(&doc).len() < 500, "expansion occurred");
    }

    #[test]
    fn nested_text_reaches_the_output_in_document_order() {
        let xml = "<root><a>first</a><b>second</b></root>";
        let doc = run(
            &XmlConverter,
            xml.as_bytes(),
            &StreamInfo::new().with_extension("xml"),
        );
        let out = plain(&doc);
        let ia = out.find("first").expect("first present");
        let ib = out.find("second").expect("second present");
        assert!(ia < ib, "document order lost: {out}");
    }

    #[test]
    fn attribute_only_elements_are_not_dropped() {
        let xml = r#"<root><item id="7" state="open"/></root>"#;
        let doc = run(
            &XmlConverter,
            xml.as_bytes(),
            &StreamInfo::new().with_extension("xml"),
        );
        let out = plain(&doc);
        assert!(out.contains("id=7"), "{out}");
    }

    #[test]
    fn malformed_close_tags_do_not_lose_the_document() {
        let xml = "<root><a>text</b></root>";
        let doc = run(
            &XmlConverter,
            xml.as_bytes(),
            &StreamInfo::new().with_extension("xml"),
        );
        assert!(plain(&doc).contains("text"));
    }

    // --- feeds ------------------------------------------------------------

    const RSS: &str = r#"<?xml version="1.0"?>
        <rss version="2.0"><channel>
          <title>My Feed</title>
          <item>
            <title>First post</title>
            <link>https://example.invalid/1</link>
            <pubDate>Mon, 01 Jan 2026 00:00:00 GMT</pubDate>
            <description>Hello there</description>
          </item>
          <item>
            <title>Second post</title>
            <link>https://example.invalid/2</link>
          </item>
        </channel></rss>"#;

    #[test]
    fn rss_entries_become_headings_in_feed_order() {
        let si = StreamInfo::new().with_extension("rss");
        let doc = run(&FeedConverter, RSS.as_bytes(), &si);
        assert_eq!(doc.title.as_deref(), Some("My Feed"));
        let headings: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { text, .. } => Some(text.to_plain()),
                _ => None,
            })
            .collect();
        assert_eq!(headings, vec!["First post", "Second post"]);
    }

    #[test]
    fn rss_carries_link_and_summary() {
        let si = StreamInfo::new().with_extension("rss");
        let doc = run(&FeedConverter, RSS.as_bytes(), &si);
        let out = format!("{:?}", doc.blocks);
        assert!(out.contains("example.invalid/1"));
        assert!(out.contains("Hello there"));
    }

    #[test]
    fn atom_link_href_attributes_are_read() {
        let atom = r#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
              <title>Atomic</title>
              <entry>
                <title>Entry one</title>
                <link href="https://example.invalid/a"/>
                <summary>Body</summary>
              </entry>
            </feed>"#;
        let si = StreamInfo::new().with_extension("atom");
        let doc = run(&FeedConverter, atom.as_bytes(), &si);
        assert!(format!("{:?}", doc.blocks).contains("example.invalid/a"));
    }

    #[test]
    fn a_feed_in_a_dot_xml_file_is_still_recognised() {
        let p = Probe::new(RSS.as_bytes());
        let si = StreamInfo::new().with_extension("xml");
        assert!(FeedConverter.accepts(&p, &si));
        // ...and the generic converter stands aside once the feed one has it.
        let feed_si = StreamInfo::new().with_extension("rss");
        assert!(!XmlConverter.accepts(&p, &feed_si));
    }
}
