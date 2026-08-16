//! Operator templates.
//!
//! MiniJinja, the engine already used by `transform/template` and the LLM
//! bindings. Two levels, both optional:
//!
//! - **per-block** overrides change how one construct renders and inherit the
//!   default renderer for everything else;
//! - a **document** template owns the whole output and is where front matter
//!   lives.
//!
//! Templates see a *simplified* projection of the IR, not its serde form.
//! `block.text` is a string, `block.rows` is a list of lists of strings. The
//! real IR nests inline spans, which is right for a renderer and hostile to a
//! template author.
//!
//! Two escape hatches keep templates from becoming all-or-nothing: `body`
//! holds the default-rendered document, and `gfm_table(t)` renders one table
//! the way the built-in renderer would.

use std::collections::BTreeMap;

use minijinja::{Environment, Value as JinjaValue};
use serde_json::{Map, Value, json};

use crate::config::TemplateSpec;
use crate::cx::Limits;
use crate::error::ConvertError;
use crate::ir::{Block, Document, ImageRef, Inline, Table};
use crate::render::{RenderOptions, Rendered, escape_table_cell, render, render_block};
use crate::stream_info::StreamInfo;

/// Extra context a caller can put in front of a document template. Kept out
/// of the engine so this crate reads no clock and touches no environment —
/// a template that stamps `now` gets the value from the plugin, which keeps
/// the golden-corpus output reproducible.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderExtras<'a> {
    pub source: Option<&'a StreamInfo>,
    pub now: Option<&'a str>,
}

/// Compiled templates for one profile.
pub struct Templates {
    env: Environment<'static>,
    has_document: bool,
    block_names: Vec<String>,
}

impl std::fmt::Debug for Templates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Templates")
            .field("document", &self.has_document)
            .field("blocks", &self.block_names)
            .finish()
    }
}

const DOC_TEMPLATE: &str = "__document";

impl Templates {
    /// Compile a spec. Every parse error surfaces here — at
    /// `register_profile()` — rather than on the first conversion.
    pub fn compile(spec: &TemplateSpec) -> Result<Self, ConvertError> {
        let mut env = Environment::new();
        // No filesystem loader, no includes outside this map: a template is
        // operator config, but it is still config, and config should not be
        // able to read the disk.
        env.set_loader(|_name| Ok(None));

        if let Some(doc) = &spec.document {
            env.add_template_owned(DOC_TEMPLATE.to_owned(), doc.clone())
                .map_err(|e| ConvertError::Template {
                    stage: "parse document",
                    message: e.to_string(),
                })?;
        }

        let mut block_names = Vec::new();
        for (name, body) in &spec.blocks {
            if !KNOWN_BLOCK_TEMPLATES.contains(&name.as_str()) {
                return Err(ConvertError::Template {
                    stage: "parse blocks",
                    message: format!(
                        "unknown block template {name:?} (known: {})",
                        KNOWN_BLOCK_TEMPLATES.join(", ")
                    ),
                });
            }
            env.add_template_owned(name.clone(), body.clone())
                .map_err(|e| ConvertError::Template {
                    stage: "parse blocks",
                    message: format!("{name}: {e}"),
                })?;
            block_names.push(name.clone());
        }

        env.add_function("gfm_table", gfm_table_fn);

        Ok(Self {
            env,
            has_document: spec.document.is_some(),
            block_names,
        })
    }

    /// Render a document through the templates, falling back to the built-in
    /// renderer for anything not overridden.
    pub fn render(
        &self,
        doc: &Document,
        opts: &RenderOptions,
        limits: &Limits,
        extras: RenderExtras<'_>,
    ) -> Result<Rendered, ConvertError> {
        // Block overrides first: the document template's `body` must already
        // reflect them, or overriding a block would silently do nothing for
        // anyone who also has a document template.
        let mut warnings = Vec::new();
        let body = if self.block_names.is_empty() {
            let r = render(doc, &self.body_only(opts), limits);
            warnings.extend(r.warnings);
            r.markdown
        } else {
            self.render_blocks(&doc.blocks, opts, limits)?
        };

        if !self.has_document {
            let trimmed = truncate_at_line(&body, limits.max_output_bytes as usize, &mut warnings);
            return Ok(Rendered {
                markdown: trimmed,
                warnings,
            });
        }

        let tmpl = self
            .env
            .get_template(DOC_TEMPLATE)
            .map_err(|e| ConvertError::Template {
                stage: "load document",
                message: e.to_string(),
            })?;

        let ctx = json!({
            "doc": document_value(doc),
            "body": body,
            "source": source_value(extras.source),
            "now": extras.now.unwrap_or_default(),
        });

        let out =
            tmpl.render(JinjaValue::from_serialize(&ctx))
                .map_err(|e| ConvertError::Template {
                    stage: "render document",
                    message: e.to_string(),
                })?;

        let out = truncate_at_line(&out, limits.max_output_bytes as usize, &mut warnings);
        Ok(Rendered {
            markdown: out,
            warnings,
        })
    }

    /// Front matter belongs to the document template when there is one —
    /// emitting both would produce two front-matter blocks.
    fn body_only(&self, opts: &RenderOptions) -> RenderOptions {
        if self.has_document {
            RenderOptions {
                front_matter: crate::render::FrontMatter::None,
                ..opts.clone()
            }
        } else {
            opts.clone()
        }
    }

    fn render_blocks(
        &self,
        blocks: &[Block],
        opts: &RenderOptions,
        limits: &Limits,
    ) -> Result<String, ConvertError> {
        let mut out = String::new();
        for b in blocks {
            let name = block_template_name(b);
            let piece = match self.block_names.iter().find(|n| *n == name) {
                Some(n) => {
                    let tmpl = self
                        .env
                        .get_template(n)
                        .map_err(|e| ConvertError::Template {
                            stage: "load block",
                            message: e.to_string(),
                        })?;
                    let ctx = json!({ "block": block_value(b) });
                    tmpl.render(JinjaValue::from_serialize(&ctx)).map_err(|e| {
                        ConvertError::Template {
                            stage: "render block",
                            message: format!("{name}: {e}"),
                        }
                    })?
                }
                None => render_block(b, opts, limits),
            };
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            out.push_str(piece);
            out.push_str("\n\n");
            if out.len() >= limits.max_output_bytes as usize {
                break;
            }
        }
        Ok(out)
    }
}

/// The block-type names a template may override.
pub const KNOWN_BLOCK_TEMPLATES: &[&str] = &[
    "heading",
    "paragraph",
    "list",
    "table",
    "code",
    "quote",
    "image",
    "rule",
    "embedded",
];

fn block_template_name(b: &Block) -> &'static str {
    match b {
        Block::Heading { .. } => "heading",
        Block::Paragraph(_) => "paragraph",
        Block::List { .. } => "list",
        Block::Table(_) => "table",
        Block::Code { .. } => "code",
        Block::Quote(_) => "quote",
        Block::Image(_) => "image",
        Block::Rule => "rule",
        Block::Raw { .. } | Block::RawHtml { .. } => "raw",
        Block::Embedded { .. } => "embedded",
    }
}

/// The simplified projection a template sees.
fn block_value(b: &Block) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(block_template_name(b)));
    match b {
        Block::Heading { level, text } => {
            m.insert("level".into(), json!(level));
            m.insert("text".into(), json!(text.to_plain()));
        }
        Block::Paragraph(t) => {
            m.insert("text".into(), json!(t.to_plain()));
        }
        Block::List { ordered, items } => {
            m.insert("ordered".into(), json!(ordered));
            m.insert(
                "items".into(),
                json!(
                    items
                        .iter()
                        .map(|blocks| blocks
                            .iter()
                            .map(|b| match b {
                                Block::Paragraph(t) => t.to_plain(),
                                other => block_value(other)["text"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_owned(),
                            })
                            .collect::<Vec<_>>()
                            .join(" "))
                        .collect::<Vec<_>>()
                ),
            );
        }
        Block::Table(t) => {
            m.insert("caption".into(), json!(t.caption));
            m.insert(
                "header".into(),
                json!(t.header.as_ref().map(|r| plain_row(r))),
            );
            m.insert(
                "rows".into(),
                json!(t.rows.iter().map(|r| plain_row(r)).collect::<Vec<_>>()),
            );
        }
        Block::Code { language, text } => {
            m.insert("language".into(), json!(language));
            m.insert("text".into(), json!(text));
        }
        Block::Quote(inner) => {
            m.insert(
                "text".into(),
                json!(
                    inner
                        .iter()
                        .filter_map(|b| match b {
                            Block::Paragraph(t) => Some(t.to_plain()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n")
                ),
            );
        }
        Block::Image(img) => {
            m.insert("alt".into(), json!(img.alt));
            m.insert("caption".into(), json!(img.caption));
            let (kind, src) = match &img.source {
                ImageRef::Url(u) => ("url", u.clone()),
                ImageRef::Resource(u) => ("resource", u.clone()),
                ImageRef::Embedded(n) => ("embedded", n.clone()),
                ImageRef::None => ("none", String::new()),
            };
            m.insert("source_kind".into(), json!(kind));
            m.insert("source".into(), json!(src));
        }
        Block::Rule => {}
        Block::Raw { markdown } => {
            m.insert("text".into(), json!(markdown));
        }
        Block::RawHtml { html } => {
            m.insert("text".into(), json!(html));
        }
        Block::Embedded { name, doc } => {
            m.insert("name".into(), json!(name));
            m.insert("title".into(), json!(doc.title));
        }
    }
    Value::Object(m)
}

fn plain_row(cells: &[Inline]) -> Vec<String> {
    cells.iter().map(Inline::to_plain).collect()
}

fn document_value(doc: &Document) -> Value {
    let mut extra = Map::new();
    for (k, v) in &doc.metadata.extra {
        extra.insert(k.clone(), json!(v));
    }
    json!({
        "title": doc.title,
        "metadata": {
            "author": doc.metadata.author,
            "created": doc.metadata.created,
            "modified": doc.metadata.modified,
            "language": doc.metadata.language,
            "extra": Value::Object(extra),
        },
        "blocks": doc.blocks.iter().map(block_value).collect::<Vec<_>>(),
        "warnings": doc.warnings.iter().map(|w| json!({
            "kind": w.kind.as_str(),
            "message": w.message,
        })).collect::<Vec<_>>(),
    })
}

fn source_value(si: Option<&StreamInfo>) -> Value {
    match si {
        Some(s) => json!({
            "filename": s.filename,
            "mimetype": s.mimetype,
            "extension": s.extension,
            "charset": s.charset,
            "uri": s.url,
        }),
        None => json!({}),
    }
}

/// `gfm_table(t)` — render a table-shaped value the way the built-in
/// renderer would. Accepts either a whole table block or a bare
/// `{header, rows}` map, so a template can build one itself.
fn gfm_table_fn(value: JinjaValue) -> Result<String, minijinja::Error> {
    let json: Value = serde_json::to_value(&value).map_err(|e| {
        minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("gfm_table: {e}"),
        )
    })?;
    let header: Option<Vec<String>> = json
        .get("header")
        .and_then(|h| serde_json::from_value(h.clone()).ok());
    let rows: Vec<Vec<String>> = json
        .get("rows")
        .and_then(|r| serde_json::from_value(r.clone()).ok())
        .unwrap_or_default();

    let width = rows
        .iter()
        .map(Vec::len)
        .chain(header.as_ref().map(Vec::len))
        .max()
        .unwrap_or(0);
    if width == 0 {
        return Ok(String::new());
    }

    let mut out = String::new();
    let mut head = header.unwrap_or_default();
    head.resize(width, String::new());
    out.push_str(&format!(
        "| {} |\n",
        head.iter()
            .map(|c| escape_table_cell(c))
            .collect::<Vec<_>>()
            .join(" | ")
    ));
    out.push_str(&format!("| {} |\n", vec!["---"; width].join(" | ")));
    for row in rows {
        let mut r = row;
        r.resize(width, String::new());
        out.push_str(&format!(
            "| {} |\n",
            r.iter()
                .map(|c| escape_table_cell(c))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    Ok(out)
}

/// Cut at the last newline before the ceiling, so a truncated template output
/// never ends mid-construct.
fn truncate_at_line(s: &str, max: usize, warnings: &mut Vec<crate::error::Warning>) -> String {
    if s.len() <= max {
        return s.trim_end().to_owned() + "\n";
    }
    let cut = s[..max].rfind('\n').unwrap_or(max);
    warnings.push(crate::error::Warning::new(
        crate::error::WarningKind::Truncated,
        format!("templated output reached max_output_bytes ({max})"),
    ));
    s[..cut].trim_end().to_owned() + "\n"
}

/// Convert an operator's block-template map into the spec shape, rejecting
/// unknown keys early. Exposed for the plugin's boot-time validation.
pub fn validate_block_names(blocks: &BTreeMap<String, String>) -> Result<(), ConvertError> {
    for name in blocks.keys() {
        if !KNOWN_BLOCK_TEMPLATES.contains(&name.as_str()) {
            return Err(ConvertError::Template {
                stage: "validate",
                message: format!(
                    "unknown block template {name:?} (known: {})",
                    KNOWN_BLOCK_TEMPLATES.join(", ")
                ),
            });
        }
    }
    Ok(())
}

/// Render a table the way `gfm_table` would. Used by tests and available to
/// callers that build a table outside the IR.
#[must_use]
pub fn gfm_table(t: &Table) -> String {
    let v = json!({
        "header": t.header.as_ref().map(|r| plain_row(r)),
        "rows": t.rows.iter().map(|r| plain_row(r)).collect::<Vec<_>>(),
    });
    gfm_table_fn(JinjaValue::from_serialize(&v)).unwrap_or_default()
}

#[cfg(test)]
mod tests;
