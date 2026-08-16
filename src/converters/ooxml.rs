//! DOCX and PPTX — hand-written OOXML walkers on `zip` + `quick-xml`.
//!
//! Both formats are a zip of XML, and both of those crates are already in the
//! tree, so this adds no dependency at all. Writing the walkers also keeps the
//! OOXML→IR mapping under our control, which is where the output quality
//! actually lives: which paragraph styles become headings, whether a table
//! cell keeps its line breaks, what happens to a hyperlink.
//!
//! Entity handling comes from [`crate::converters::xml::read_events`], so the
//! XXE and expansion posture is the same one every other XML path here has.

use std::io::{Cursor, Read};

use crate::converters::squeeze;
use crate::converters::xml::read_events;
use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document, Inline, Span, Table};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

type Archive = zip::ZipArchive<Cursor<Vec<u8>>>;

/// Open the container. A password-protected OOXML file is an OLE wrapper
/// around an encrypted stream, so it never even parses as a zip — the
/// distinction matters because the operator remedy is completely different.
pub(crate) fn open(bytes: &[u8], format: &'static str) -> Result<Archive, ConvertError> {
    zip::ZipArchive::new(Cursor::new(bytes.to_vec())).map_err(|e| match e {
        zip::result::ZipError::InvalidArchive(_) => ConvertError::Malformed {
            format,
            message: "not a readable zip container".to_owned(),
        },
        other => ConvertError::Malformed {
            format,
            message: other.to_string(),
        },
    })
}

/// Read one entry as UTF-8, charging its decompressed size to the budget.
///
/// Every read in this module goes through here: an OOXML part is attacker-
/// controlled and its declared size is a claim, so the bytes are counted as
/// they arrive rather than trusted up front.
pub(crate) fn read_part(
    zip: &mut Archive,
    name: &str,
    cx: &ConvertCx<'_>,
) -> Result<Option<String>, ConvertError> {
    let Ok(entry) = zip.by_name(name) else {
        return Ok(None);
    };
    let remaining = cx
        .limits()
        .max_expanded_bytes
        .saturating_sub(cx.budget().expanded_bytes());
    let mut buf = Vec::new();
    // One byte past the allowance, so an oversized part trips the budget
    // below instead of arriving silently truncated — a half-read
    // `document.xml` would otherwise surface as a confusing parse error.
    entry
        .take(remaining + 1)
        .read_to_end(&mut buf)
        .map_err(|e| ConvertError::Malformed {
            format: "ooxml",
            message: format!("{name}: {e}"),
        })?;
    cx.budget().charge_expanded(buf.len() as u64)?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// `docProps/core.xml` — title, author, dates. Absent in files written by
/// some generators, which is not an error.
pub(crate) fn core_properties(zip: &mut Archive, doc: &mut Document, cx: &ConvertCx<'_>) {
    let Ok(Some(xml)) = read_part(zip, "docProps/core.xml", cx) else {
        return;
    };
    let mut title = None;
    let mut creator = None;
    let mut created = None;
    let mut modified = None;
    let _ = read_events(&xml, "ooxml", |n| {
        let v = squeeze(&n.own_text);
        if v.is_empty() {
            return;
        }
        match n.name() {
            "title" => title = Some(v),
            "creator" => creator = Some(v),
            "created" => created = Some(v),
            "modified" => modified = Some(v),
            "language" => doc.metadata.language = Some(v),
            "keywords" | "subject" | "category" => doc.metadata.set(n.name(), v),
            _ => {}
        }
    });
    if let Some(t) = title {
        doc.title = Some(t).filter(|t| !t.is_empty());
    }
    doc.metadata.author = creator;
    doc.metadata.created = created;
    doc.metadata.modified = modified;
}

/// Relationship id → target, from a `_rels` part. Used to turn a DOCX
/// hyperlink's `r:id` into an actual URL.
fn relationships(zip: &mut Archive, name: &str, cx: &ConvertCx<'_>) -> Vec<(String, String)> {
    let Ok(Some(xml)) = read_part(zip, name, cx) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let _ = read_events(&xml, "ooxml", |n| {
        if n.name() != "relationship" {
            return;
        }
        let get = |k: &str| n.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        if let (Some(id), Some(target)) = (get("id"), get("target")) {
            out.push((id, target));
        }
    });
    out
}

// ---------------------------------------------------------------------------
// DOCX
// ---------------------------------------------------------------------------

pub struct DocxConverter;

impl Converter for DocxConverter {
    fn name(&self) -> &'static str {
        "docx"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        probe.starts_with(b"PK\x03\x04")
            && (info.is_ext("docx")
                || info.is_mime(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                ))
    }

    fn convert(
        &self,
        bytes: &[u8],
        _info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let mut zip = open(bytes, "docx")?;
        let mut doc = Document::new();
        core_properties(&mut zip, &mut doc, cx);

        let rels = relationships(&mut zip, "word/_rels/document.xml.rels", cx);
        let xml = read_part(&mut zip, "word/document.xml", cx)?.ok_or(ConvertError::Malformed {
            format: "docx",
            message: "no word/document.xml — not a Word document".to_owned(),
        })?;
        cx.budget().check_deadline()?;

        let mut w = DocxWalker {
            blocks: Vec::new(),
            runs: Vec::new(),
            style: None,
            list_level: None,
            pending_bold: false,
            pending_italic: false,
            pending_cell_runs: None,
            tables: Vec::new(),
            rels: &rels,
        };
        read_events(&xml, "docx", |n| w.visit(n))?;
        doc.blocks = w.blocks;

        // Footnotes and endnotes are separate parts. They carry real content,
        // so losing them silently would be a quiet fidelity loss.
        for (part, label) in [
            ("word/footnotes.xml", "Footnotes"),
            ("word/endnotes.xml", "Endnotes"),
        ] {
            if let Some(xml) = read_part(&mut zip, part, cx)? {
                let notes = note_paragraphs(&xml)?;
                if !notes.is_empty() {
                    doc.push(Block::Heading {
                        level: 2,
                        text: Inline::text(label),
                    });
                    for n in notes {
                        doc.push(Block::Paragraph(Inline::text(n)));
                    }
                }
            }
        }

        if doc.title.is_none() {
            doc.title = doc.blocks.iter().find_map(|b| match b {
                Block::Heading { text, .. } => Some(text.to_plain()),
                _ => None,
            });
        }
        if doc.blocks.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                "the document body carried no text",
            ));
        }
        Ok(doc)
    }
}

/// One text run, and whether it sat inside a hyperlink.
struct Run {
    text: String,
    linked: bool,
    bold: bool,
    italic: bool,
}

struct DocxWalker<'a> {
    blocks: Vec<Block>,
    runs: Vec<Run>,
    style: Option<String>,
    list_level: Option<u8>,
    pending_bold: bool,
    pending_italic: bool,
    /// Text of the paragraphs seen since the current cell opened. A cell
    /// closes after its paragraphs, so this is complete by then.
    pending_cell_runs: Option<String>,
    /// One accumulator per table nesting level, so a table inside a cell does
    /// not merge into its parent.
    tables: Vec<TableAcc>,
    rels: &'a [(String, String)],
}

#[derive(Default)]
struct TableAcc {
    rows: Vec<Vec<Inline>>,
    row: Vec<Inline>,
}

impl DocxWalker<'_> {
    fn visit(&mut self, n: &crate::converters::xml::Node) {
        let depth = n.path.iter().filter(|p| *p == "tbl").count();
        match n.name() {
            // Paragraph properties close before the paragraph they describe.
            "pstyle" => self.style = attr(n, "val"),
            "ilvl" => {
                self.list_level = attr(n, "val").and_then(|v| v.parse::<u8>().ok());
            }
            "numpr" => {
                // Present with no <w:ilvl> means a top-level list item.
                self.list_level.get_or_insert(0);
            }
            // Run properties sit *before* the text they describe
            // (`<w:r><w:rPr><w:b/></w:rPr><w:t>…`), so they are held pending
            // and consumed by the next `<w:t>` rather than applied backwards.
            "b" => self.pending_bold = true,
            "i" => self.pending_italic = true,
            "t" => {
                if !n.own_text.is_empty() {
                    self.runs.push(Run {
                        text: n.own_text.clone(),
                        linked: n.under("hyperlink"),
                        bold: self.pending_bold,
                        italic: self.pending_italic,
                    });
                }
            }
            "r" => {
                self.pending_bold = false;
                self.pending_italic = false;
            }
            "tab" => self.runs.push(Run {
                text: " ".to_owned(),
                linked: false,
                bold: false,
                italic: false,
            }),
            "br" => self.runs.push(Run {
                text: "\n".to_owned(),
                linked: false,
                bold: false,
                italic: false,
            }),
            // A hyperlink closes immediately after its own runs, so the
            // trailing flagged runs are exactly its content.
            "hyperlink" => self.close_hyperlink(n),
            "p" => self.close_paragraph(depth),
            "tc" => {
                let cell = Inline::text(self.take_cell_text().unwrap_or_else(|| squeeze(&n.text)));
                self.table_at(depth).row.push(cell);
            }
            "tr" => {
                let acc = self.table_at(depth);
                let row = std::mem::take(&mut acc.row);
                if !row.is_empty() {
                    acc.rows.push(row);
                }
            }
            "tbl" => self.close_table(depth),
            _ => {}
        }
    }

    /// Wrap the trailing hyperlink-flagged runs into a link span.
    fn close_hyperlink(&mut self, n: &crate::converters::xml::Node) {
        let Some(id) = attr(n, "id") else {
            return;
        };
        let Some((_, target)) = self.rels.iter().find(|(rid, _)| *rid == id) else {
            return;
        };
        let start = self
            .runs
            .iter()
            .rposition(|r| !r.linked)
            .map_or(0, |p| p + 1);
        if start >= self.runs.len() {
            return;
        }
        let text: String = self.runs[start..].iter().map(|r| r.text.as_str()).collect();
        self.runs.truncate(start);
        self.runs.push(Run {
            // The href rides in the text with a sentinel the paragraph
            // builder recognises; a Run holds a string, and a link is the one
            // construct that needs two.
            text: format!("\u{1}{target}\u{1}{text}"),
            linked: false,
            bold: false,
            italic: false,
        });
    }

    fn close_paragraph(&mut self, table_depth: usize) {
        let runs = std::mem::take(&mut self.runs);
        let style = self.style.take();
        let level = self.list_level.take();

        // Inside a table, paragraphs are cell content, not document blocks.
        // A cell may hold several, so they accumulate; the renderer turns the
        // newline into a `<br>` rather than breaking the row.
        if table_depth > 0 {
            let text = runs_to_plain(&runs);
            if !text.is_empty() {
                match &mut self.pending_cell_runs {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(&text);
                    }
                    None => self.pending_cell_runs = Some(text),
                }
            }
            return;
        }

        let inline = runs_to_inline(&runs);
        if inline.is_blank() {
            return;
        }

        if let Some(h) = heading_level(style.as_deref()) {
            self.blocks.push(Block::Heading {
                level: h,
                text: inline,
            });
            return;
        }
        // A list item is marked EITHER by an inline `<w:numPr>` or by a list
        // paragraph style. Word and python-docx both use the style and put
        // the numbering in styles.xml, so checking only for `numPr` reads a
        // real bulleted list as a run of plain paragraphs.
        if let Some(ordered) = list_style(style.as_deref()).or(level.map(|_| false)) {
            // Consecutive items merge into one list rather than producing a
            // one-item list per paragraph.
            if let Some(Block::List { items, .. }) = self.blocks.last_mut() {
                items.push(vec![Block::Paragraph(inline)]);
                return;
            }
            self.blocks.push(Block::List {
                ordered,
                items: vec![vec![Block::Paragraph(inline)]],
            });
            return;
        }
        self.blocks.push(Block::Paragraph(inline));
    }

    fn close_table(&mut self, depth: usize) {
        let Some(acc) = self.tables.get_mut(depth.saturating_sub(1)) else {
            return;
        };
        let acc = std::mem::take(acc);
        if acc.rows.is_empty() {
            return;
        }
        let mut rows = acc.rows;
        // A Word table's first row is a header far more often than not, and
        // GFM needs one regardless.
        let header = Some(rows.remove(0));
        self.blocks.push(Block::Table(Table {
            caption: None,
            header,
            rows,
        }));
    }

    fn table_at(&mut self, depth: usize) -> &mut TableAcc {
        let idx = depth.saturating_sub(1);
        if self.tables.len() <= idx {
            self.tables.resize_with(idx + 1, TableAcc::default);
        }
        &mut self.tables[idx]
    }

    fn take_cell_text(&mut self) -> Option<String> {
        self.pending_cell_runs.take().filter(|s| !s.is_empty())
    }
}

fn attr(n: &crate::converters::xml::Node, key: &str) -> Option<String> {
    n.attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// Whether a paragraph style names a list, and if so whether it is ordered.
///
/// `ListParagraph` is the style Word applies to *both* bulleted and numbered
/// lists — the distinction lives in the numbering definition, which needs
/// `numbering.xml`. Unordered is the safer reading: a wrong bullet is a
/// cosmetic loss, while numbering a list that was not numbered invents
/// ordering the document never claimed.
fn list_style(style: Option<&str>) -> Option<bool> {
    let s = style?.to_ascii_lowercase().replace([' ', '-', '_'], "");
    let s = s.trim_end_matches(|c: char| c.is_ascii_digit());
    match s {
        "listnumber" | "listnum" | "numberedlist" => Some(true),
        "listbullet" | "listparagraph" | "list" | "bulletlist" | "bullet" => Some(false),
        _ => None,
    }
}

/// `Heading1`, `Heading 1`, `heading1` and `Title` all appear in the wild.
fn heading_level(style: Option<&str>) -> Option<u8> {
    let s = style?.to_ascii_lowercase().replace([' ', '-', '_'], "");
    if s == "title" {
        return Some(1);
    }
    if s == "subtitle" {
        return Some(2);
    }
    let n = s.strip_prefix("heading")?;
    n.parse::<u8>().ok().filter(|l| (1..=6).contains(l))
}

fn runs_to_plain(runs: &[Run]) -> String {
    squeeze(
        &runs
            .iter()
            .map(|r| strip_link_sentinel(&r.text))
            .collect::<String>(),
    )
}

fn runs_to_inline(runs: &[Run]) -> Inline {
    let mut spans = Vec::new();
    for r in runs {
        if let Some((href, text)) = split_link_sentinel(&r.text) {
            spans.push(Span::Link {
                text: Inline::text(text),
                href: href.to_owned(),
            });
            continue;
        }
        if r.text.is_empty() {
            continue;
        }
        let inner = Inline::text(r.text.clone());
        spans.push(match (r.bold, r.italic) {
            (true, _) => Span::Strong(inner),
            (false, true) => Span::Emphasis(inner),
            _ => Span::Text(r.text.clone()),
        });
    }
    Inline(spans)
}

/// A run carrying a hyperlink is encoded `\x01href\x01text` — see
/// [`DocxWalker::close_hyperlink`].
fn split_link_sentinel(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('\u{1}')?;
    rest.split_once('\u{1}')
}

fn strip_link_sentinel(s: &str) -> String {
    match split_link_sentinel(s) {
        Some((_, text)) => text.to_owned(),
        None => s.to_owned(),
    }
}

/// Footnote / endnote bodies, skipping the separator pseudo-notes Word adds.
fn note_paragraphs(xml: &str) -> Result<Vec<String>, ConvertError> {
    let mut out = Vec::new();
    read_events(xml, "docx", |n| {
        if n.name() == "p" {
            let t = squeeze(&n.text);
            if !t.is_empty() {
                out.push(t);
            }
        }
    })?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// PPTX
// ---------------------------------------------------------------------------

pub struct PptxConverter;

impl Converter for PptxConverter {
    fn name(&self) -> &'static str {
        "pptx"
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        probe.starts_with(b"PK\x03\x04")
            && (info.is_ext("pptx")
                || info.is_mime(
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                ))
    }

    fn convert(
        &self,
        bytes: &[u8],
        _info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let mut zip = open(bytes, "pptx")?;
        let mut doc = Document::new();
        core_properties(&mut zip, &mut doc, cx);

        let mut slides: Vec<(u32, String)> = zip
            .file_names()
            .filter_map(|n| slide_number(n).map(|i| (i, n.to_owned())))
            .collect();
        if slides.is_empty() {
            return Err(ConvertError::Malformed {
                format: "pptx",
                message: "no ppt/slides/slideN.xml parts — not a presentation".to_owned(),
            });
        }
        // Lexical order puts slide10 before slide2.
        slides.sort_by_key(|(i, _)| *i);

        for (index, part) in slides {
            cx.budget().check_deadline()?;
            let Some(xml) = read_part(&mut zip, &part, cx)? else {
                continue;
            };
            doc.push(Block::Heading {
                level: 2,
                text: Inline::text(format!("Slide {index}")),
            });
            for b in slide_blocks(&xml)? {
                doc.push(b);
            }
            let notes = format!("ppt/notesSlides/notesSlide{index}.xml");
            if let Some(nxml) = read_part(&mut zip, &notes, cx)? {
                let text = slide_paragraphs(&nxml)?;
                if !text.is_empty() {
                    doc.push(Block::Quote(
                        std::iter::once(Block::Paragraph(Inline(vec![Span::Strong(
                            Inline::text("Speaker notes"),
                        )])))
                        .chain(text.into_iter().map(|p| Block::Paragraph(Inline::text(p))))
                        .collect(),
                    ));
                }
            }
        }

        if doc.title.is_none() {
            doc.title = doc.blocks.iter().find_map(|b| match b {
                Block::Paragraph(i) if !i.is_blank() => Some(i.to_plain()),
                _ => None,
            });
        }
        Ok(doc)
    }
}

fn slide_number(name: &str) -> Option<u32> {
    name.strip_prefix("ppt/slides/slide")?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

/// A slide's paragraphs and tables, in document order.
fn slide_blocks(xml: &str) -> Result<Vec<Block>, ConvertError> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut row: Vec<Inline> = Vec::new();
    let mut rows: Vec<Vec<Inline>> = Vec::new();
    read_events(xml, "pptx", |n| match n.name() {
        "p" if !n.under("tbl") => {
            let t = squeeze(&n.text);
            if !t.is_empty() {
                blocks.push(Block::Paragraph(Inline::text(t)));
            }
        }
        "tc" => row.push(Inline::text(squeeze(&n.text))),
        "tr" => {
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
        }
        "tbl" if !rows.is_empty() => {
            let mut r = std::mem::take(&mut rows);
            let header = Some(r.remove(0));
            blocks.push(Block::Table(Table {
                caption: None,
                header,
                rows: r,
            }));
        }
        _ => {}
    })?;
    Ok(blocks)
}

fn slide_paragraphs(xml: &str) -> Result<Vec<String>, ConvertError> {
    let mut out = Vec::new();
    read_events(xml, "pptx", |n| {
        if n.name() == "p" {
            let t = squeeze(&n.text);
            if !t.is_empty() {
                out.push(t);
            }
        }
    })?;
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests;
