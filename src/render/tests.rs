use super::*;
use crate::cx::Limits;
use crate::ir::{Block, Document, Image, ImageRef, Inline, Metadata, Span, Table};

fn md(doc: &Document) -> String {
    render(doc, &RenderOptions::default(), &Limits::default()).markdown
}

fn md_with(doc: &Document, opts: RenderOptions) -> String {
    render(doc, &opts, &Limits::default()).markdown
}

fn doc_of(blocks: Vec<Block>) -> Document {
    Document {
        blocks,
        ..Document::default()
    }
}

// --- escaping -------------------------------------------------------------

#[test]
fn pipe_in_a_cell_cannot_break_the_table() {
    let doc = doc_of(vec![Block::Table(Table {
        caption: None,
        header: Some(vec![Inline::text("a"), Inline::text("b")]),
        rows: vec![vec![Inline::text("x | y"), Inline::text("z")]],
    })]);
    let out = md(&doc);
    let data_row = out
        .lines()
        .find(|l| l.contains("x "))
        .expect("data row present");
    // Three delimiters = two columns. A raw pipe would make it four.
    assert_eq!(data_row.matches(" | ").count(), 1, "row was {data_row:?}");
    assert!(data_row.contains("x \\| y"));
}

#[test]
fn newline_in_a_cell_becomes_a_break_not_a_row() {
    let doc = doc_of(vec![Block::Table(Table {
        caption: None,
        header: None,
        rows: vec![vec![Inline::text("line1\nline2")]],
    })]);
    let out = md(&doc);
    assert!(out.contains("line1<br>line2"));
    assert_eq!(out.lines().filter(|l| l.starts_with('|')).count(), 3);
}

#[test]
fn text_cannot_forge_markdown_structure() {
    let doc = doc_of(vec![Block::Paragraph(Inline::text(
        "# not a heading [not](a-link) *not em*",
    ))]);
    let out = md(&doc);
    assert!(out.contains("\\# not a heading"));
    assert!(out.contains("\\[not\\]"));
    assert!(out.contains("\\*not em\\*"));
}

#[test]
fn underscores_inside_words_are_left_alone() {
    let doc = doc_of(vec![Block::Paragraph(Inline::text(
        "snake_case_name and _em_",
    ))]);
    let out = md(&doc);
    assert!(out.contains("snake_case_name"), "got {out:?}");
    assert!(out.contains("\\_em\\_"), "got {out:?}");
}

#[test]
fn ordered_list_lookalikes_are_escaped() {
    let doc = doc_of(vec![Block::Paragraph(Inline::text("1. not a list"))]);
    assert!(md(&doc).contains("1\\. not a list"));
}

#[test]
fn html_tags_in_text_are_escaped() {
    let doc = doc_of(vec![Block::Paragraph(Inline::text(
        "<script>alert(1)</script>",
    ))]);
    let out = md(&doc);
    assert!(!out.contains("<script>"), "got {out:?}");
    assert!(out.contains("\\<script\\>"));
}

// --- code fences ----------------------------------------------------------

#[test]
fn fence_outgrows_backticks_in_the_body() {
    let doc = doc_of(vec![Block::Code {
        language: Some("rust".into()),
        text: "let x = ```y```;".into(),
    }]);
    let out = md(&doc);
    assert!(out.contains("````rust"), "got {out:?}");
    assert!(out.trim_end().ends_with("````"));
}

#[test]
fn inline_code_pads_when_it_starts_with_a_backtick() {
    let inline_md = inline(&Inline(vec![Span::Code("`x`".into())]));
    assert_eq!(inline_md, "`` `x` ``");
}

// --- structure ------------------------------------------------------------

#[test]
fn heading_offset_shifts_and_clamps() {
    let doc = doc_of(vec![
        Block::Heading {
            level: 1,
            text: Inline::text("one"),
        },
        Block::Heading {
            level: 6,
            text: Inline::text("six"),
        },
    ]);
    let out = md_with(
        &doc,
        RenderOptions {
            heading_offset: 2,
            ..RenderOptions::default()
        },
    );
    assert!(out.contains("### one"));
    // Clamped at h6 rather than emitting an invalid level.
    assert!(out.contains("###### six"));
}

#[test]
fn embedded_documents_are_demoted_below_their_own_heading() {
    let mut inner = Document::new();
    inner.push(Block::Heading {
        level: 1,
        text: Inline::text("inner title"),
    });
    let doc = doc_of(vec![Block::Embedded {
        name: "attachment.txt".into(),
        doc: Box::new(inner),
    }]);
    let out = md(&doc);
    assert!(out.contains("## attachment.txt"));
    // h1 + 2 = h3, so the child can never outrank its own introduction.
    assert!(out.contains("### inner title"), "got {out:?}");
}

#[test]
fn nested_lists_indent_continuation_lines() {
    let doc = doc_of(vec![Block::List {
        ordered: true,
        items: vec![vec![
            Block::Paragraph(Inline::text("first")),
            Block::Paragraph(Inline::text("still first")),
        ]],
    }]);
    let out = md(&doc);
    assert!(out.contains("1. first"), "got {out:?}");
    assert!(out.contains("\n   still first"), "got {out:?}");
}

#[test]
fn quotes_prefix_every_line_including_blanks() {
    let doc = doc_of(vec![Block::Quote(vec![
        Block::Paragraph(Inline::text("a")),
        Block::Paragraph(Inline::text("b")),
    ])]);
    let out = md(&doc);
    assert!(out.contains("> a"));
    assert!(out.contains("> b"));
    assert!(!out.lines().any(|l| !l.is_empty() && !l.starts_with('>')));
}

#[test]
fn ragged_rows_still_produce_a_valid_table() {
    let doc = doc_of(vec![Block::Table(Table {
        caption: None,
        header: Some(vec![Inline::text("a")]),
        rows: vec![vec![
            Inline::text("1"),
            Inline::text("2"),
            Inline::text("3"),
        ]],
    })]);
    let out = md(&doc);
    let rows: Vec<&str> = out.lines().filter(|l| l.starts_with('|')).collect();
    let cols = rows[0].matches('|').count();
    assert!(
        rows.iter().all(|r| r.matches('|').count() == cols),
        "{rows:?}"
    );
}

#[test]
fn headerless_table_keeps_gfm_valid() {
    let doc = doc_of(vec![Block::Table(Table {
        caption: None,
        header: None,
        rows: vec![vec![Inline::text("only")]],
    })]);
    let out = md(&doc);
    let rows: Vec<&str> = out.lines().filter(|l| l.starts_with('|')).collect();
    assert_eq!(rows.len(), 3, "header + separator + one row: {rows:?}");
    assert!(rows[1].contains("---"));
}

// --- table styles ---------------------------------------------------------

#[test]
fn html_table_style_escapes_entities() {
    let doc = doc_of(vec![Block::Table(Table {
        caption: None,
        header: None,
        rows: vec![vec![Inline::text("<b>&</b>")]],
    })]);
    let out = md_with(
        &doc,
        RenderOptions {
            tables: TableStyle::Html,
            ..RenderOptions::default()
        },
    );
    assert!(out.contains("&lt;b&gt;&amp;&lt;/b&gt;"), "got {out:?}");
}

#[test]
fn csv_table_style_quotes_embedded_commas() {
    let doc = doc_of(vec![Block::Table(Table {
        caption: None,
        header: None,
        rows: vec![vec![Inline::text("a,b"), Inline::text("plain")]],
    })]);
    let out = md_with(
        &doc,
        RenderOptions {
            tables: TableStyle::Csv,
            ..RenderOptions::default()
        },
    );
    assert!(out.contains("\"a,b\",plain"), "got {out:?}");
}

// --- front matter ---------------------------------------------------------

#[test]
fn yaml_front_matter_quotes_every_scalar() {
    let doc = Document {
        title: Some("yes".into()),
        metadata: Metadata {
            author: Some("A: B".into()),
            ..Metadata::default()
        },
        ..Document::default()
    };
    let out = md_with(
        &doc,
        RenderOptions {
            front_matter: FrontMatter::Yaml,
            ..RenderOptions::default()
        },
    );
    // Unquoted, `yes` parses as a boolean and `A: B` breaks the mapping.
    assert!(out.contains("title: \"yes\""), "got {out:?}");
    assert!(out.contains("author: \"A: B\""), "got {out:?}");
}

#[test]
fn front_matter_is_omitted_when_there_is_nothing_to_say() {
    let doc = doc_of(vec![Block::Paragraph(Inline::text("body"))]);
    let out = md_with(
        &doc,
        RenderOptions {
            front_matter: FrontMatter::Yaml,
            ..RenderOptions::default()
        },
    );
    assert!(!out.starts_with("---"), "got {out:?}");
}

// --- images ---------------------------------------------------------------

#[test]
fn embedded_image_names_do_not_become_broken_links() {
    let doc = doc_of(vec![Block::Image(Image {
        alt: Some("chart".into()),
        caption: None,
        source: ImageRef::Embedded("word/media/image1.png".into()),
    })]);
    let out = md(&doc);
    assert!(!out.contains("]("), "got {out:?}");
    assert!(out.contains("word/media/image1.png"));
}

#[test]
fn a_generated_caption_gets_its_own_line() {
    let doc = doc_of(vec![Block::Image(Image {
        alt: Some("logo".into()),
        caption: Some("A blue circle".into()),
        source: ImageRef::Resource("mcpg-resource://hash:abc".into()),
    })]);
    let out = md(&doc);
    assert!(out.contains("![logo](mcpg-resource://hash:abc)"));
    assert!(out.contains("*A blue circle*"));
}

// --- limits ---------------------------------------------------------------

#[test]
fn output_cap_truncates_and_warns() {
    let blocks: Vec<Block> = (0..500)
        .map(|i| Block::Paragraph(Inline::text(format!("paragraph number {i}"))))
        .collect();
    let doc = doc_of(blocks);
    let r = render(
        &doc,
        &RenderOptions::default(),
        &Limits {
            max_output_bytes: 200,
            ..Limits::default()
        },
    );
    assert!(r.markdown.len() <= 220, "len {}", r.markdown.len());
    assert!(
        r.warnings.iter().any(|w| w.kind == WarningKind::Truncated),
        "{:?}",
        r.warnings
    );
}

#[test]
fn table_row_cap_truncates_and_says_so() {
    let rows: Vec<Vec<Inline>> = (0..50).map(|i| vec![Inline::text(i.to_string())]).collect();
    let doc = doc_of(vec![Block::Table(Table {
        caption: None,
        header: Some(vec![Inline::text("n")]),
        rows,
    })]);
    let r = render(
        &doc,
        &RenderOptions::default(),
        &Limits {
            max_table_rows: 10,
            ..Limits::default()
        },
    );
    assert!(
        r.markdown.contains("40 further rows omitted"),
        "{}",
        r.markdown
    );
    assert!(r.warnings.iter().any(|w| w.kind == WarningKind::Truncated));
}

#[test]
fn html_fragments_are_dropped_unless_opted_in() {
    let doc = doc_of(vec![Block::RawHtml {
        html: "<details>hidden</details>".into(),
    }]);
    let r = render(&doc, &RenderOptions::default(), &Limits::default());
    assert!(!r.markdown.contains("details"), "{}", r.markdown);
    assert!(r.warnings.iter().any(|w| w.kind == WarningKind::Degraded));

    let kept = md_with(
        &doc,
        RenderOptions {
            preserve_unsupported_html: true,
            ..RenderOptions::default()
        },
    );
    assert!(kept.contains("<details>hidden</details>"));
}

#[test]
fn markdown_passthrough_is_never_dropped_or_re_escaped() {
    // A `.md` source is already Markdown. Escaping it again would turn every
    // heading into literal text; dropping it would lose the document.
    let doc = doc_of(vec![Block::Raw {
        markdown: "# real heading\n\n- a list".into(),
    }]);
    let out = md(&doc);
    assert!(out.contains("# real heading"), "got {out:?}");
    assert!(out.contains("- a list"));
}

#[test]
fn output_always_ends_with_exactly_one_newline() {
    let doc = doc_of(vec![
        Block::Paragraph(Inline::text("a")),
        Block::Rule,
        Block::Paragraph(Inline::text("b")),
    ]);
    let out = md(&doc);
    assert!(out.ends_with('\n'));
    assert!(!out.ends_with("\n\n"));
    assert!(!out.contains("\n\n\n"));
}
