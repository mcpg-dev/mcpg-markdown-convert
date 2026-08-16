//! HTML → Markdown, via `htmd`.
//!
//! `htmd` is turndown.js-shaped, which is the closest available match to
//! markitdown's `markdownify` and therefore the shortest path to comparable
//! output. Its result is already Markdown, so it rides in a [`Block::Raw`]
//! rather than being re-escaped.

use crate::converters::decode_text;
use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document};
use crate::registry::{Converter, PRIORITY_GENERIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct HtmlConverter;

impl Converter for HtmlConverter {
    fn name(&self) -> &'static str {
        "html"
    }

    fn priority(&self) -> i32 {
        // Generic: an HTML fragment can appear inside almost anything, so
        // format-specific converters get first refusal.
        PRIORITY_GENERIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        if !probe.looks_textual() {
            return false;
        }
        if info.is_mime("text/html") || info.is_ext("html") || info.is_ext("htm") {
            return true;
        }
        if info.is_ext("xhtml") {
            return true;
        }
        // Unlabelled input that opens with a doctype or an <html> tag.
        if info.mimetype.is_none() && info.extension.is_none() {
            let head = probe.leading_text(256);
            return head.starts_with("<!doctype html") || head.starts_with("<html");
        }
        false
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let html = decode_text(bytes, info.charset.as_deref());
        cx.budget().check_deadline()?;

        let mut doc = Document::new();
        if let Some(t) = extract_title(&html) {
            doc = doc.with_title(t);
        }
        for (k, v) in extract_meta(&html) {
            match k.as_str() {
                "author" => doc.metadata.author = Some(v),
                "description" | "keywords" => doc.metadata.set(k, v),
                _ => {}
            }
        }

        // Script, style and head content is not prose. htmd drops the tags,
        // but stripping the bodies first keeps them out of the output on
        // malformed markup where the parser recovers oddly — and keeps the
        // title from appearing twice, once as metadata and once as text.
        let cleaned = strip_non_prose(&html);

        let markdown = htmd::convert(&cleaned).map_err(|e| ConvertError::Malformed {
            format: "html",
            message: e.to_string(),
        })?;

        let markdown = markdown.trim();
        if markdown.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                "the HTML carried no extractable prose",
            ));
            return Ok(doc);
        }

        doc.push(Block::Raw {
            markdown: markdown.to_owned(),
        });
        Ok(doc)
    }
}

/// The contents of the first `<title>`.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let end = lower[open_end..].find("</title>")? + open_end;
    let raw = html.get(open_end..end)?;
    let t = crate::converters::squeeze(&decode_entities(raw));
    if t.is_empty() { None } else { Some(t) }
}

/// `<meta name=... content=...>` pairs. A deliberately small scan rather than
/// a second HTML parser: we want four fields, not a DOM.
fn extract_meta(html: &str) -> Vec<(String, String)> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut idx = 0usize;
    while let Some(rel) = lower[idx..].find("<meta") {
        let start = idx + rel;
        let Some(rel_end) = lower[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + rel_end];
        let name = attr_value(tag, "name").or_else(|| attr_value(tag, "property"));
        let content = attr_value(tag, "content");
        if let (Some(n), Some(c)) = (name, content) {
            out.push((n.to_ascii_lowercase(), decode_entities(&c)));
        }
        idx = start + rel_end + 1;
    }
    out
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{attr}="))? + attr.len() + 1;
    let rest = tag.get(at..)?;
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)? + 1;
        Some(rest[1..end].to_owned())
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_owned())
    }
}

/// Remove `<script>`, `<style>`, `<head>` and comment bodies.
fn strip_non_prose(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    while i < html.len() {
        let next = ["<script", "<style", "<head", "<!--"]
            .iter()
            .filter_map(|tag| lower[i..].find(tag).map(|p| (i + p, *tag)))
            .min_by_key(|(p, _)| *p);
        let Some((start, tag)) = next else {
            out.push_str(&html[i..]);
            break;
        };
        out.push_str(&html[i..start]);
        let close = match tag {
            "<script" => "</script>",
            "<style" => "</style>",
            "<head" => "</head>",
            _ => "-->",
        };
        match lower[start..].find(close) {
            Some(rel) => i = start + rel + close.len(),
            // Unterminated: drop the remainder rather than emit a half tag.
            None => break,
        }
    }
    out
}

/// The five predefined entities plus the numeric forms, which is all a title
/// or a meta attribute realistically carries.
fn decode_entities(s: &str) -> String {
    let mut out = s
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");
    // Ampersand last, so `&amp;lt;` does not become `<`.
    out = out.replace("&amp;", "&");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    fn convert(html: &str) -> Document {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        HtmlConverter
            .convert(
                html.as_bytes(),
                &StreamInfo::new().with_extension("html"),
                &cx,
            )
            .expect("converts")
    }

    fn markdown(doc: &Document) -> String {
        doc.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Raw { markdown } => Some(markdown.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headings_and_lists_survive() {
        let out = markdown(&convert(
            "<h1>Title</h1><p>Body <strong>bold</strong></p><ul><li>one</li><li>two</li></ul>",
        ));
        assert!(out.contains("# Title"), "{out}");
        assert!(out.contains("**bold**"), "{out}");
        assert!(out.contains("one"), "{out}");
    }

    #[test]
    fn tables_become_markdown_not_prose() {
        let out = markdown(&convert(
            "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>",
        ));
        assert!(out.contains('|'), "table lost its structure: {out}");
    }

    #[test]
    fn the_title_element_becomes_the_document_title() {
        let doc = convert("<html><head><title> My  Page </title></head><body>x</body></html>");
        assert_eq!(doc.title.as_deref(), Some("My Page"));
    }

    #[test]
    fn meta_author_is_captured() {
        let doc = convert(
            r#"<html><head><meta name="author" content="Ada"></head><body>x</body></html>"#,
        );
        assert_eq!(doc.metadata.author.as_deref(), Some("Ada"));
    }

    #[test]
    fn script_and_style_bodies_never_reach_the_output() {
        let out = markdown(&convert(
            "<style>.a{color:red}</style><script>alert('pwn')</script><p>real</p>",
        ));
        assert!(!out.contains("alert"), "{out}");
        assert!(!out.contains("color:red"), "{out}");
        assert!(out.contains("real"), "{out}");
    }

    #[test]
    fn comments_are_dropped() {
        let out = markdown(&convert("<!-- secret --><p>visible</p>"));
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("visible"));
    }

    #[test]
    fn an_unterminated_script_does_not_leak_its_body() {
        let out = markdown(&convert("<p>before</p><script>while(1){}"));
        assert!(!out.contains("while(1)"), "{out}");
        assert!(out.contains("before"));
    }

    #[test]
    fn prose_free_html_warns_rather_than_returning_nothing() {
        let doc = convert("<html><head><title>t</title></head><body></body></html>");
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::NoTextLayer),
            "{:?}",
            doc.warnings
        );
    }

    #[test]
    fn entities_in_a_title_are_decoded_once() {
        let doc = convert("<title>a &amp;lt; b</title><p>x</p>");
        assert_eq!(doc.title.as_deref(), Some("a &lt; b"));
    }

    #[test]
    fn unlabelled_html_is_sniffed_from_the_doctype() {
        let p = Probe::new(b"<!DOCTYPE html><html><body>hi</body></html>");
        assert!(HtmlConverter.accepts(&p, &StreamInfo::new()));
    }
}
