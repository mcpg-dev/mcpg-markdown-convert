use std::collections::BTreeMap;

use super::*;
use crate::config::TemplateSpec;
use crate::cx::Limits;
use crate::ir::{Block, Document, Inline, Metadata, Table};
use crate::render::FrontMatter;

fn spec(document: Option<&str>, blocks: &[(&str, &str)]) -> TemplateSpec {
    TemplateSpec {
        document: document.map(str::to_owned),
        blocks: blocks
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn doc() -> Document {
    Document {
        title: Some("Q3 Report".into()),
        metadata: Metadata {
            author: Some("Ada".into()),
            ..Metadata::default()
        },
        blocks: vec![
            Block::Heading {
                level: 1,
                text: Inline::text("Summary"),
            },
            Block::Paragraph(Inline::text("Revenue rose.")),
            Block::Table(Table {
                caption: Some("Totals".into()),
                header: Some(vec![Inline::text("q"), Inline::text("eur")]),
                rows: vec![vec![Inline::text("Q3"), Inline::text("1|2")]],
            }),
        ],
        warnings: vec![],
    }
}

fn render_with(spec: &TemplateSpec) -> Rendered {
    Templates::compile(spec)
        .expect("compiles")
        .render(
            &doc(),
            &RenderOptions::default(),
            &Limits::default(),
            RenderExtras::default(),
        )
        .expect("renders")
}

#[test]
fn a_document_template_owns_the_whole_output() {
    let out = render_with(&spec(
        Some("TITLE={{ doc.title }}\nAUTHOR={{ doc.metadata.author }}\n---\n{{ body }}"),
        &[],
    ));
    assert!(out.markdown.starts_with("TITLE=Q3 Report"));
    assert!(out.markdown.contains("AUTHOR=Ada"));
    // `body` carries the default rendering, so a template need not
    // reimplement the renderer to add a header.
    assert!(out.markdown.contains("# Summary"));
}

#[test]
fn a_block_template_overrides_only_its_own_type() {
    let out = render_with(&spec(None, &[("heading", ">>> {{ block.text }} <<<")]));
    assert!(out.markdown.contains(">>> Summary <<<"), "{}", out.markdown);
    // The paragraph still comes from the built-in renderer.
    assert!(out.markdown.contains("Revenue rose."), "{}", out.markdown);
}

#[test]
fn block_overrides_are_visible_to_the_document_template() {
    // If `body` were rendered before block overrides applied, a profile with
    // both would silently lose the block override.
    let out = render_with(&spec(
        Some("{{ body }}"),
        &[("heading", "== {{ block.text }} ==")],
    ));
    assert!(out.markdown.contains("== Summary =="), "{}", out.markdown);
}

#[test]
fn templates_see_plain_strings_not_inline_spans() {
    let out = render_with(&spec(None, &[("paragraph", "[{{ block.text }}]")]));
    assert!(out.markdown.contains("[Revenue rose.]"), "{}", out.markdown);
}

#[test]
fn table_rows_reach_templates_as_lists_of_strings() {
    let out = render_with(&spec(
        None,
        &[(
            "table",
            "{% for r in block.rows %}{{ r[0] }}/{{ r[1] }}{% endfor %}",
        )],
    ));
    assert!(out.markdown.contains("Q3/1|2"), "{}", out.markdown);
}

#[test]
fn gfm_table_helper_escapes_cells() {
    let out = render_with(&spec(None, &[("table", "{{ gfm_table(block) }}")]));
    // The raw pipe in the cell must not split the row.
    assert!(out.markdown.contains("1\\|2"), "{}", out.markdown);
    assert!(out.markdown.contains("| --- | --- |"), "{}", out.markdown);
}

#[test]
fn source_and_now_come_from_the_caller_not_a_clock() {
    let si = crate::stream_info::StreamInfo::new().with_filename("in.docx");
    let t = Templates::compile(&spec(Some("{{ source.filename }} at {{ now }}"), &[])).unwrap();
    let out = t
        .render(
            &doc(),
            &RenderOptions::default(),
            &Limits::default(),
            RenderExtras {
                source: Some(&si),
                now: Some("2026-01-01T00:00:00Z"),
            },
        )
        .unwrap();
    assert!(out.markdown.contains("in.docx at 2026-01-01T00:00:00Z"));
}

#[test]
fn a_document_template_suppresses_the_built_in_front_matter() {
    let out = Templates::compile(&spec(Some("---\nmine: 1\n---\n{{ body }}"), &[]))
        .unwrap()
        .render(
            &doc(),
            &RenderOptions {
                front_matter: FrontMatter::Yaml,
                ..RenderOptions::default()
            },
            &Limits::default(),
            RenderExtras::default(),
        )
        .unwrap();
    // Exactly one front-matter block, the template's own.
    assert_eq!(out.markdown.matches("---\n").count(), 2, "{}", out.markdown);
    assert!(!out.markdown.contains("title: \"Q3 Report\""));
}

#[test]
fn a_broken_template_fails_at_compile_time() {
    let err = Templates::compile(&spec(Some("{% for x in %}"), &[])).unwrap_err();
    assert_eq!(err.code(), "template");
}

#[test]
fn an_unknown_block_name_is_rejected_rather_than_ignored() {
    let err = Templates::compile(&spec(None, &[("paragrahp", "x")])).unwrap_err();
    assert!(format!("{err}").contains("unknown block template"), "{err}");
}

#[test]
fn templates_cannot_load_from_the_filesystem() {
    let err = Templates::compile(&spec(Some("{% include '/etc/passwd' %}"), &[]))
        .map(|t| {
            t.render(
                &doc(),
                &RenderOptions::default(),
                &Limits::default(),
                RenderExtras::default(),
            )
        })
        .expect("parses")
        .unwrap_err();
    assert_eq!(err.code(), "template");
}

#[test]
fn templated_output_respects_the_byte_ceiling() {
    let out = Templates::compile(&spec(
        Some("{% for i in range(5000) %}line {{ i }}\n{% endfor %}"),
        &[],
    ))
    .unwrap()
    .render(
        &doc(),
        &RenderOptions::default(),
        &Limits {
            max_output_bytes: 200,
            ..Limits::default()
        },
        RenderExtras::default(),
    )
    .unwrap();
    assert!(out.markdown.len() <= 220, "len {}", out.markdown.len());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.kind == crate::error::WarningKind::Truncated)
    );
}

#[test]
fn validate_block_names_matches_the_compiler() {
    let mut m = BTreeMap::new();
    m.insert("table".to_owned(), "x".to_owned());
    assert!(validate_block_names(&m).is_ok());
    m.insert("nope".to_owned(), "x".to_owned());
    assert!(validate_block_names(&m).is_err());
}
