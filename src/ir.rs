//! The document IR — what converters produce and renderers consume.
//!
//! Deliberately small. This is a rendering contract, not a document object
//! model: if it grows toward "represent all of OOXML" it has failed. Anything
//! a converter can express but the IR cannot goes through [`Block::Raw`].
//!
//! Converters emit an IR rather than a Markdown string for three reasons:
//! operator templates need addressable structure (`doc.blocks`, `block.rows`)
//! rather than a finished blob; the LLM enrichment pass needs somewhere to
//! attach captions after conversion; and the golden-corpus suite needs a
//! stable thing to diff.

use serde::{Deserialize, Serialize};

use crate::error::Warning;

/// A converted document, before rendering.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    /// Document title, when the source carries one (OOXML core properties,
    /// `<title>`, the first heading, the mail subject).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub blocks: Vec<Block>,
    /// Non-fatal degradations. See [`Warning`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl Document {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style title set, ignoring blank strings so converters can pass
    /// whatever the source gave them without pre-checking.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        let t = title.into();
        if !t.trim().is_empty() {
            self.title = Some(t);
        }
        self
    }

    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    pub fn warn(&mut self, warning: Warning) {
        self.warnings.push(warning);
    }

    /// Total count of blocks including those nested in lists, quotes and
    /// embedded documents. Used by the budget checks and by tests.
    #[must_use]
    pub fn block_count(&self) -> usize {
        fn count(blocks: &[Block]) -> usize {
            blocks
                .iter()
                .map(|b| {
                    1 + match b {
                        Block::List { items, .. } => items.iter().map(|i| count(i)).sum(),
                        Block::Quote(inner) => count(inner),
                        Block::Embedded { doc, .. } => count(&doc.blocks),
                        _ => 0,
                    }
                })
                .sum()
        }
        count(&self.blocks)
    }
}

/// Source-derived facts that belong in front matter rather than the body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Everything else the source offered, in source order. A map would lose
    /// ordering and collide on repeated EXIF keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<(String, String)>,
}

impl Metadata {
    /// Record a key/value, skipping empties so converters can forward
    /// optional fields unconditionally.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let v = value.into();
        if v.trim().is_empty() {
            return;
        }
        self.extra.push((key.into(), v));
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.extra
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A block-level construct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Heading {
        level: u8,
        text: Inline,
    },
    Paragraph(Inline),
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Table(Table),
    Code {
        language: Option<String>,
        text: String,
    },
    Quote(Vec<Block>),
    Image(Image),
    Rule,
    /// Markdown the converter produced directly, emitted verbatim.
    ///
    /// For sources that are *already* Markdown — a `.md` file, an `htmd`
    /// conversion — where re-escaping would corrupt the content. A converter
    /// must only put text here that it produced or that arrived as Markdown;
    /// anything else belongs in a [`Span::Text`], which is escaped.
    Raw {
        markdown: String,
    },
    /// An HTML fragment Markdown cannot express (`htmd` faithful mode).
    ///
    /// Separate from [`Block::Raw`] because it is subject to
    /// `preserve_unsupported_html`: an operator whose consumer rejects inline
    /// HTML gets it dropped with a warning, while Markdown pass-through keeps
    /// working.
    RawHtml {
        html: String,
    },
    /// A nested document: a zip member, an e-mail attachment, an EPUB
    /// chapter. `name` is already sanitised (see `sanitize_member_name`).
    Embedded {
        name: String,
        doc: Box<Document>,
    },
}

/// A table. Header is optional because CSV without a header row, and XLSX
/// sheets whose first row is data, both exist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<Vec<Inline>>,
    #[serde(default)]
    pub rows: Vec<Vec<Inline>>,
}

impl Table {
    /// Column count, taken as the widest row so a ragged source still renders
    /// a valid GFM table.
    #[must_use]
    pub fn width(&self) -> usize {
        let header_w = self.header.as_ref().map_or(0, Vec::len);
        self.rows
            .iter()
            .map(Vec::len)
            .chain([header_w])
            .max()
            .unwrap_or(0)
    }
}

/// An image reference plus whatever text describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    /// Filled by the LLM enrichment pass when it runs. Never populated by a
    /// converter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub source: ImageRef,
}

/// Where an image lives. Converters never fetch any of these — an image
/// reference in a document is untrusted input, and following it would make
/// the converter a request forgery primitive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageRef {
    /// A URL the source document named. Rendered as a link, never fetched.
    Url(String),
    /// A name inside the source container (`word/media/image1.png`).
    Embedded(String),
    /// A gateway content-store URI. The only variant enrichment can read.
    Resource(String),
    /// The source gave no usable reference.
    #[default]
    None,
}

/// Inline content. Flat rather than a tree: nesting emphasis inside links
/// inside emphasis buys nothing for an LLM reader and costs a lot of
/// converter complexity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Inline(pub Vec<Span>);

impl Inline {
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Inline(vec![Span::Text(s.into())])
    }

    #[must_use]
    pub fn empty() -> Self {
        Inline(Vec::new())
    }

    pub fn push(&mut self, span: Span) {
        self.0.push(span);
    }

    /// Plain-text projection, for titles, alt text and length checks.
    #[must_use]
    pub fn to_plain(&self) -> String {
        let mut out = String::new();
        for span in &self.0 {
            match span {
                Span::Text(t) | Span::Code(t) => out.push_str(t),
                Span::Emphasis(i) | Span::Strong(i) => out.push_str(&i.to_plain()),
                Span::Link { text, .. } => out.push_str(&text.to_plain()),
                Span::LineBreak => out.push(' '),
            }
        }
        out
    }

    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.to_plain().trim().is_empty()
    }
}

impl From<&str> for Inline {
    fn from(s: &str) -> Self {
        Inline::text(s)
    }
}

impl From<String> for Inline {
    fn from(s: String) -> Self {
        Inline::text(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "span", rename_all = "snake_case")]
pub enum Span {
    Text(String),
    Emphasis(Inline),
    Strong(Inline),
    Code(String),
    Link { text: Inline, href: String },
    LineBreak,
}

/// Strip a container member name down to something safe to put in
/// `Embedded.name`, a log line, or a template.
///
/// We never write these to disk, so this is not about path traversal on our
/// filesystem — it is that the name flows onward into text an operator reads
/// and a model consumes, and `../../etc/passwd` in a heading is a lie about
/// where the content came from.
#[must_use]
pub fn sanitize_member_name(raw: &str) -> String {
    let cleaned: String = raw
        .replace('\\', "/")
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
        // NTFS alternate data streams, and control characters that would
        // corrupt a terminal or a log aggregator.
        .map(|seg| {
            seg.split(':')
                .next()
                .unwrap_or(seg)
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
        })
        .filter(|seg| !seg.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if cleaned.is_empty() {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_traversal_and_streams() {
        assert_eq!(sanitize_member_name("../../etc/passwd"), "etc/passwd");
        assert_eq!(sanitize_member_name("/abs/path.txt"), "abs/path.txt");
        assert_eq!(sanitize_member_name("a\\b\\c.txt"), "a/b/c.txt");
        assert_eq!(sanitize_member_name("file.txt:$DATA"), "file.txt");
        assert_eq!(sanitize_member_name("../.."), "unnamed");
        assert_eq!(sanitize_member_name(""), "unnamed");
    }

    #[test]
    fn sanitize_drops_control_characters() {
        assert_eq!(sanitize_member_name("ev\u{1b}[2Jil.txt"), "ev[2Jil.txt");
    }

    #[test]
    fn table_width_takes_widest_row() {
        let t = Table {
            caption: None,
            header: Some(vec![Inline::text("a"), Inline::text("b")]),
            rows: vec![
                vec![Inline::text("1")],
                vec![Inline::text("1"), Inline::text("2"), Inline::text("3")],
            ],
        };
        assert_eq!(t.width(), 3);
    }

    #[test]
    fn inline_plain_projection_walks_nesting() {
        let inline = Inline(vec![
            Span::Text("a ".into()),
            Span::Strong(Inline::text("bold")),
            Span::Link {
                text: Inline::text(" link"),
                href: "https://example.invalid".into(),
            },
        ]);
        assert_eq!(inline.to_plain(), "a bold link");
    }

    #[test]
    fn block_count_descends_into_nesting() {
        let mut doc = Document::new();
        doc.push(Block::List {
            ordered: false,
            items: vec![vec![Block::Paragraph(Inline::text("x"))]],
        });
        // the list itself + one nested paragraph
        assert_eq!(doc.block_count(), 2);
    }
}
