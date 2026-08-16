//! IR → Markdown.
//!
//! CommonMark plus GFM tables. The renderer is the only place that writes
//! Markdown syntax, which is what makes escaping an invariant rather than a
//! habit: converters put text into [`Span::Text`] and never think about
//! whether it contains a pipe.

use crate::cx::Limits;
use crate::error::{Warning, WarningKind};
use crate::ir::{Block, Document, Image, ImageRef, Inline, Span, Table};

/// How tables are emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableStyle {
    /// GitHub-flavoured pipe tables. Compact and the format models read best.
    #[default]
    Gfm,
    /// HTML `<table>`. Lossless for ragged or multi-line cells.
    Html,
    /// CSV inside a fenced block. For sheets far too wide for a pipe table.
    Csv,
}

/// Front-matter dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontMatter {
    #[default]
    None,
    Yaml,
    Toml,
}

/// Operator-facing rendering options.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderOptions {
    #[serde(default)]
    pub front_matter: FrontMatter,
    #[serde(default)]
    pub tables: TableStyle,
    /// Added to every heading level, so an embedded document can be nested
    /// under its parent's heading without colliding with it.
    #[serde(default)]
    pub heading_offset: u8,
    /// Emit `Block::Raw` verbatim. Off means raw blocks are dropped with a
    /// warning — the safe default when the output feeds a strict renderer.
    #[serde(default)]
    pub preserve_unsupported_html: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            front_matter: FrontMatter::None,
            tables: TableStyle::Gfm,
            heading_offset: 0,
            preserve_unsupported_html: false,
        }
    }
}

/// Rendered output plus anything the render itself had to warn about.
#[derive(Debug, Clone, Default)]
pub struct Rendered {
    pub markdown: String,
    pub warnings: Vec<Warning>,
}

/// Render a document to Markdown, stopping cleanly at
/// [`Limits::max_output_bytes`].
#[must_use]
pub fn render(doc: &Document, opts: &RenderOptions, limits: &Limits) -> Rendered {
    let mut r = Renderer {
        out: String::new(),
        opts,
        max: limits.max_output_bytes as usize,
        max_rows: limits.max_table_rows as usize,
        truncated: false,
        warnings: Vec::new(),
    };

    r.front_matter(doc);
    r.blocks(&doc.blocks, 0);

    let mut markdown = std::mem::take(&mut r.out);
    // Collapse the runs of blank lines block separation naturally produces.
    while markdown.contains("\n\n\n") {
        markdown = markdown.replace("\n\n\n", "\n\n");
    }
    let markdown = markdown.trim_end().to_owned() + "\n";

    let mut warnings = r.warnings;
    if r.truncated {
        warnings.push(Warning::new(
            WarningKind::Truncated,
            format!(
                "output reached max_output_bytes ({}) and was cut at a block boundary",
                limits.max_output_bytes
            ),
        ));
    }
    Rendered { markdown, warnings }
}

struct Renderer<'a> {
    out: String,
    opts: &'a RenderOptions,
    max: usize,
    max_rows: usize,
    truncated: bool,
    warnings: Vec<Warning>,
}

impl Renderer<'_> {
    /// True once the output ceiling is reached. Every block-level entry point
    /// checks this first, so truncation always lands between blocks.
    fn full(&self) -> bool {
        self.truncated || self.out.len() >= self.max
    }

    fn push(&mut self, s: &str) {
        if self.truncated {
            return;
        }
        if self.out.len() + s.len() > self.max {
            self.truncated = true;
            return;
        }
        self.out.push_str(s);
    }

    fn line(&mut self, s: &str) {
        self.push(s);
        self.push("\n");
    }

    fn blank(&mut self) {
        if !self.out.ends_with("\n\n") && !self.out.is_empty() {
            self.push("\n");
        }
    }

    fn front_matter(&mut self, doc: &Document) {
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(t) = &doc.title {
            pairs.push(("title".into(), t.clone()));
        }
        let m = &doc.metadata;
        for (k, v) in [
            ("author", &m.author),
            ("created", &m.created),
            ("modified", &m.modified),
            ("language", &m.language),
        ] {
            if let Some(v) = v {
                pairs.push((k.into(), v.clone()));
            }
        }
        pairs.extend(m.extra.iter().cloned());

        match self.opts.front_matter {
            FrontMatter::None => {}
            FrontMatter::Yaml => {
                if pairs.is_empty() {
                    return;
                }
                self.line("---");
                for (k, v) in pairs {
                    let line = format!("{}: {}", yaml_key(&k), yaml_scalar(&v));
                    self.line(&line);
                }
                self.line("---");
                self.blank();
            }
            FrontMatter::Toml => {
                if pairs.is_empty() {
                    return;
                }
                self.line("+++");
                for (k, v) in pairs {
                    let line = format!("{} = {}", yaml_key(&k), toml_string(&v));
                    self.line(&line);
                }
                self.line("+++");
                self.blank();
            }
        }
    }

    fn blocks(&mut self, blocks: &[Block], extra_heading: u8) {
        for (i, b) in blocks.iter().enumerate() {
            if self.full() {
                // Stopping here IS truncation. `push` only notices an
                // overflow it was asked to perform, so a loop that bails on
                // `full()` first would drop blocks silently.
                if i < blocks.len() {
                    self.truncated = true;
                }
                return;
            }
            self.block(b, extra_heading);
        }
    }

    fn block(&mut self, block: &Block, extra_heading: u8) {
        match block {
            Block::Heading { level, text } => {
                let lvl = (u16::from(*level)
                    + u16::from(self.opts.heading_offset)
                    + u16::from(extra_heading))
                .clamp(1, 6) as usize;
                self.blank();
                let line = format!("{} {}", "#".repeat(lvl), inline(text));
                self.line(&line);
                self.blank();
            }
            Block::Paragraph(text) => {
                if text.is_blank() {
                    return;
                }
                self.blank();
                let line = inline(text);
                self.line(&line);
                self.blank();
            }
            Block::List { ordered, items } => {
                self.blank();
                self.list(*ordered, items, 0, extra_heading);
                self.blank();
            }
            Block::Table(t) => {
                self.blank();
                self.table(t);
                self.blank();
            }
            Block::Code { language, text } => {
                self.blank();
                let fence = fence_for(text);
                let open = format!("{fence}{}", language.as_deref().unwrap_or(""));
                self.line(&open);
                self.line(text.trim_end_matches('\n'));
                self.line(&fence);
                self.blank();
            }
            Block::Quote(inner) => {
                self.blank();
                // Render the inner blocks standalone, then prefix. Simpler and
                // more correct than threading a prefix through every writer.
                let nested = render_fragment(inner, self.opts, self.max, self.max_rows);
                for l in nested.lines() {
                    let line = if l.is_empty() {
                        ">".to_owned()
                    } else {
                        format!("> {l}")
                    };
                    self.line(&line);
                }
                self.blank();
            }
            Block::Image(img) => {
                self.blank();
                self.line(&image(img));
                self.blank();
            }
            Block::Rule => {
                self.blank();
                self.line("---");
                self.blank();
            }
            Block::Raw { markdown } => {
                self.blank();
                self.line(markdown.trim_end());
                self.blank();
            }
            Block::RawHtml { html } => {
                if self.opts.preserve_unsupported_html {
                    self.blank();
                    self.line(html.trim_end());
                    self.blank();
                } else {
                    self.warnings.push(Warning::new(
                        WarningKind::Degraded,
                        "dropped an HTML fragment Markdown cannot express \
                         (preserve_unsupported_html is off)",
                    ));
                }
            }
            Block::Embedded { name, doc } => {
                self.blank();
                let heading = format!("## {}", escape_text(name));
                self.line(&heading);
                self.blank();
                // Nested headings shift down so an embedded document cannot
                // outrank the heading that introduces it.
                self.blocks(&doc.blocks, extra_heading + 2);
                self.blank();
            }
        }
    }

    fn list(&mut self, ordered: bool, items: &[Vec<Block>], indent: usize, extra_heading: u8) {
        let pad = "  ".repeat(indent);
        for (i, item) in items.iter().enumerate() {
            if self.full() {
                return;
            }
            let marker = if ordered {
                format!("{}. ", i + 1)
            } else {
                "- ".to_owned()
            };
            // A list item is a block sequence. Render it, then indent every
            // line after the first by the marker width.
            let body = render_fragment(item, self.opts, self.max, self.max_rows);
            let body = body.trim_end();
            let mut lines = body.lines();
            match lines.next() {
                Some(first) => {
                    let line = format!("{pad}{marker}{first}");
                    self.line(&line);
                }
                None => {
                    let line = format!("{pad}{marker}");
                    self.line(line.trim_end());
                    continue;
                }
            }
            let cont = " ".repeat(marker.len());
            for l in lines {
                if l.is_empty() {
                    self.push("\n");
                } else {
                    let line = format!("{pad}{cont}{l}");
                    self.line(&line);
                }
            }
        }
        let _ = extra_heading;
    }

    fn table(&mut self, t: &Table) {
        if let Some(c) = &t.caption
            && !c.trim().is_empty()
        {
            let line = format!("**{}**", escape_text(c));
            self.line(&line);
            self.blank();
        }
        let width = t.width();
        if width == 0 {
            return;
        }
        let truncated_rows = t.rows.len() > self.max_rows;
        let rows: &[Vec<Inline>] = if truncated_rows {
            &t.rows[..self.max_rows]
        } else {
            &t.rows
        };

        match self.opts.tables {
            TableStyle::Gfm => {
                let header: Vec<String> = match &t.header {
                    Some(h) => pad_cells(h, width),
                    // GFM requires a header row. An empty one keeps the table
                    // valid without inventing column names.
                    None => vec![String::new(); width],
                };
                let line = format!("| {} |", header.join(" | "));
                self.line(&line);
                let sep = format!("| {} |", vec!["---"; width].join(" | "));
                self.line(&sep);
                for row in rows {
                    if self.full() {
                        break;
                    }
                    let cells = pad_cells(row, width);
                    let line = format!("| {} |", cells.join(" | "));
                    self.line(&line);
                }
            }
            TableStyle::Html => {
                self.line("<table>");
                if let Some(h) = &t.header {
                    self.line("<thead><tr>");
                    for c in pad_cells_raw(h, width) {
                        let line = format!("<th>{}</th>", html_escape(&c));
                        self.line(&line);
                    }
                    self.line("</tr></thead>");
                }
                self.line("<tbody>");
                for row in rows {
                    if self.full() {
                        break;
                    }
                    self.line("<tr>");
                    for c in pad_cells_raw(row, width) {
                        let line = format!("<td>{}</td>", html_escape(&c));
                        self.line(&line);
                    }
                    self.line("</tr>");
                }
                self.line("</tbody>");
                self.line("</table>");
            }
            TableStyle::Csv => {
                self.line("```csv");
                if let Some(h) = &t.header {
                    let line = csv_row(&pad_cells_raw(h, width));
                    self.line(&line);
                }
                for row in rows {
                    if self.full() {
                        break;
                    }
                    let line = csv_row(&pad_cells_raw(row, width));
                    self.line(&line);
                }
                self.line("```");
            }
        }

        if truncated_rows {
            self.warnings.push(Warning::new(
                WarningKind::Truncated,
                format!(
                    "table truncated to {} of {} rows (max_table_rows)",
                    self.max_rows,
                    t.rows.len()
                ),
            ));
            self.blank();
            let note = format!("_… {} further rows omitted_", t.rows.len() - self.max_rows);
            self.line(&note);
        }
    }
}

/// Render one block with the built-in renderer.
///
/// The templating layer calls this for every block type an operator did *not*
/// override, which is what makes per-block templates additive rather than
/// all-or-nothing.
#[must_use]
pub fn render_block(block: &Block, opts: &RenderOptions, limits: &Limits) -> String {
    render_fragment(
        std::slice::from_ref(block),
        opts,
        limits.max_output_bytes as usize,
        limits.max_table_rows as usize,
    )
}

/// Render a block sequence standalone. Used for constructs that need their
/// output post-processed (quote prefixes, list-item indentation).
fn render_fragment(blocks: &[Block], opts: &RenderOptions, max: usize, max_rows: usize) -> String {
    let mut r = Renderer {
        out: String::new(),
        opts,
        max,
        max_rows,
        truncated: false,
        warnings: Vec::new(),
    };
    r.blocks(blocks, 0);
    r.out.trim().to_owned()
}

/// Cells escaped for a GFM pipe table.
///
/// Goes through [`inline_cell`], not `inline` + [`escape_table_cell`] — the
/// latter would escape the pipe twice and render `x \\| y`.
fn pad_cells(cells: &[Inline], width: usize) -> Vec<String> {
    let mut out: Vec<String> = cells.iter().map(inline_cell).collect();
    out.resize(width, String::new());
    out
}

/// Cells as plain text, for the HTML and CSV styles which do their own
/// escaping.
fn pad_cells_raw(cells: &[Inline], width: usize) -> Vec<String> {
    let mut out: Vec<String> = cells.iter().map(Inline::to_plain).collect();
    out.resize(width, String::new());
    out
}

fn csv_row(cells: &[String]) -> String {
    cells
        .iter()
        .map(|c| {
            if c.contains([',', '"', '\n']) {
                format!("\"{}\"", c.replace('"', "\"\""))
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Inline spans → Markdown, for flowing text.
#[must_use]
pub fn inline(i: &Inline) -> String {
    inline_with(i, escape_text)
}

/// Inline spans → Markdown, for a GFM table cell.
///
/// Identical except that a hard newline becomes `<br>` rather than a space: a
/// cell cannot contain a line break, and collapsing it loses the row's shape.
#[must_use]
pub fn inline_cell(i: &Inline) -> String {
    inline_with(i, escape_cell_text)
}

fn inline_with(i: &Inline, escape: fn(&str) -> String) -> String {
    let mut out = String::new();
    for span in &i.0 {
        match span {
            Span::Text(t) => out.push_str(&escape(t)),
            Span::Emphasis(inner) => {
                let s = inline(inner);
                if !s.trim().is_empty() {
                    out.push('*');
                    out.push_str(s.trim());
                    out.push('*');
                }
            }
            Span::Strong(inner) => {
                let s = inline(inner);
                if !s.trim().is_empty() {
                    out.push_str("**");
                    out.push_str(s.trim());
                    out.push_str("**");
                }
            }
            Span::Code(t) => {
                // Inline code needs a backtick run longer than any inside it,
                // and padding spaces when it starts or ends with a backtick.
                let longest = longest_backtick_run(t);
                let ticks = "`".repeat(longest + 1);
                out.push_str(&ticks);
                if t.starts_with('`') || t.ends_with('`') {
                    out.push(' ');
                    out.push_str(t);
                    out.push(' ');
                } else {
                    out.push_str(t);
                }
                out.push_str(&ticks);
            }
            Span::Link { text, href } => {
                let label = inline(text);
                let label = if label.trim().is_empty() {
                    escape_text(href)
                } else {
                    label
                };
                out.push('[');
                out.push_str(&label);
                out.push_str("](");
                out.push_str(&escape_url(href));
                out.push(')');
            }
            Span::LineBreak => out.push_str("  \n"),
        }
    }
    out
}

fn image(img: &Image) -> String {
    let alt = img
        .alt
        .as_deref()
        .or(img.caption.as_deref())
        .unwrap_or("")
        .to_owned();
    let body = match &img.source {
        ImageRef::Url(u) => format!("![{}]({})", escape_text(&alt), escape_url(u)),
        ImageRef::Resource(u) => format!("![{}]({})", escape_text(&alt), escape_url(u)),
        // An embedded name is not a resolvable link. Naming it beats emitting
        // a broken relative path a reader might try to follow.
        ImageRef::Embedded(name) => {
            format!("*[image: {}]*", escape_text(name))
        }
        ImageRef::None => format!("*[image: {}]*", escape_text(&alt)),
    };
    match (&img.caption, &img.source) {
        // A caption that is not already carrying the alt text is worth its own
        // line — it is usually a model-generated description.
        (Some(c), ImageRef::Url(_) | ImageRef::Resource(_)) if img.alt.is_some() => {
            format!("{body}\n\n*{}*", escape_text(c))
        }
        _ => body,
    }
}

/// Escape the characters that would otherwise start Markdown syntax.
///
/// Conservative on purpose: over-escaping produces a stray backslash, while
/// under-escaping lets document content forge structure. `_` is escaped only
/// at a word boundary so `snake_case_names` survive intact.
#[must_use]
pub fn escape_text(s: &str) -> String {
    escape_inner(s, " ")
}

/// As [`escape_text`], but a newline becomes `<br>` instead of a space.
#[must_use]
pub fn escape_cell_text(s: &str) -> String {
    escape_inner(s, "<br>")
}

fn escape_inner(s: &str, newline: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let bytes: Vec<char> = s.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        let at_word_boundary = |idx: usize| -> bool {
            let before = idx
                .checked_sub(1)
                .and_then(|j| bytes.get(j))
                .is_none_or(|c| !c.is_alphanumeric());
            let after = bytes.get(idx + 1).is_none_or(|c| !c.is_alphanumeric());
            before || after
        };
        match c {
            '\\' | '`' | '*' | '[' | ']' | '<' | '>' | '|' => {
                out.push('\\');
                out.push(*c);
            }
            '_' if at_word_boundary(i) => {
                out.push('\\');
                out.push('_');
            }
            // Only leading-position characters can start a block construct.
            '#' | '+' | '-' if i == 0 => {
                out.push('\\');
                out.push(*c);
            }
            '.' if i > 0 && bytes[..i].iter().all(char::is_ascii_digit) => {
                // "1." at line start is an ordered list.
                out.push('\\');
                out.push('.');
            }
            '\r' => {}
            '\n' => out.push_str(newline),
            _ => out.push(*c),
        }
    }
    out
}

/// Escape a cell for a GFM pipe table.
///
/// Cells cannot contain a newline, and an unescaped `|` splits the row — the
/// single most common bug in hand-written Markdown emitters, which is why
/// this is enforced here rather than left to converters.
#[must_use]
pub fn escape_table_cell(s: &str) -> String {
    s.replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

/// Escape a URL for use inside `](...)`.
fn escape_url(u: &str) -> String {
    let trimmed = u.trim();
    if trimmed.contains([' ', '(', ')']) {
        format!("<{}>", trimmed.replace(['<', '>'], ""))
    } else {
        trimmed.to_owned()
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn longest_backtick_run(s: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// A fence long enough to survive any backtick run in the body.
fn fence_for(body: &str) -> String {
    "`".repeat(longest_backtick_run(body).max(2) + 1)
}

fn yaml_key(k: &str) -> String {
    if k.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !k.is_empty()
    {
        k.to_owned()
    } else {
        format!("\"{}\"", k.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Always quote. Front matter carries document-controlled text, and an
/// unquoted `value: yes` silently becomes a boolean in the reader.
fn yaml_scalar(v: &str) -> String {
    format!(
        "\"{}\"",
        v.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], " ")
    )
}

fn toml_string(v: &str) -> String {
    format!(
        "\"{}\"",
        v.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], " ")
    )
}

#[cfg(test)]
mod tests;
