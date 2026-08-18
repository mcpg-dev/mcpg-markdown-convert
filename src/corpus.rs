//! The golden corpus — what this engine actually emits, recorded.
//!
//! Every other test here asserts a *property*: the table has two columns, the
//! pipe is escaped, truncation warns. None assert what the Markdown looks
//! like. So a change to blank-line handling, or heading spacing, or the order
//! of front-matter keys, passes the whole suite while altering every document
//! the plugin produces, with no diff for a reviewer to see.
//!
//! This module records the output. Each case renders a fixture and compares
//! it byte-for-byte with a file under `src/golden/`.
//!
//! The corpus lives under `src/` rather than `tests/` because this module is
//! compiled into the library and embeds those files at compile time. The
//! open-source mirror ships `src/` and excludes `tests/`, so goldens kept
//! beside the integration tests would leave the published crate referencing
//! files it does not carry — it fails to compile for anyone but us.
//!
//! **It is not a correctness oracle.** It captures today's behaviour,
//! including anything currently wrong. Its value is that changing that
//! behaviour becomes visible and deliberate rather than incidental.
//!
//! ## Regenerating
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p mcpg-markdown-convert --features all
//! ```
//!
//! Then **read the diff**. A golden file updated without reading the diff is
//! worse than no golden file, because it converts a signal into a ritual.
//!
//! ## Why the goldens are `include_str!`, not read from disk
//!
//! Sandboxed test runners execute the binary somewhere other than the source
//! tree, so `CARGO_MANIFEST_DIR` does not lead back to `src/golden` and a
//! runtime read finds nothing. Embedding the files at compile time sidesteps
//! that entirely. The update path does need a real path, so it goes through
//! `option_env!` and simply does not run where the variable is absent —
//! comparison still works there, which is the half that matters in CI.

#![cfg(test)]

use crate::config::{ConvertOptions, FormatSelection, TemplateSpec};
use crate::cx::Limits;
use crate::engine::Engine;
use crate::render::{FrontMatter, RenderOptions, TableStyle};
use crate::stream_info::StreamInfo;

/// Where a case's bytes come from.
enum Input {
    /// A literal document, for text formats — readable in the diff as-is.
    Text(&'static str),
    /// A container built by [`crate::fixtures`]. Binary formats are built
    /// rather than committed: a checked-in `.docx` is an opaque blob nobody
    /// reviews, and it would ride into the public OSS mirror.
    Built(fn() -> Vec<u8>),
}

struct Case {
    /// Also the golden filename, so a failure names the file to look at.
    name: &'static str,
    /// Drives extension-based detection.
    filename: &'static str,
    input: Input,
    options: fn() -> ConvertOptions,
    golden: &'static str,
}

fn default_options() -> ConvertOptions {
    ConvertOptions::default()
}

/// Render one case exactly as the engine would.
fn render(case: &Case) -> String {
    let engine = Engine::new((case.options)()).expect("profile builds");
    let bytes = match &case.input {
        Input::Text(s) => s.as_bytes().to_vec(),
        Input::Built(f) => f(),
    };
    let info = StreamInfo::new().with_filename(case.filename);
    match engine.convert(&bytes, &info) {
        Ok(out) => {
            // The converter and detection signal are part of the recorded
            // behaviour: a change that silently routes a document to a
            // different converter is exactly the regression this catches.
            let warnings: String = out
                .warnings
                .iter()
                .map(|w| format!("<!-- warning: {} — {} -->\n", w.kind.as_str(), w.message))
                .collect();
            format!(
                "<!-- converter: {} via {} -->\n{warnings}\n{}",
                out.converter, out.detected_via, out.markdown
            )
        }
        Err(e) => format!("<!-- error: {} — {e} -->\n", e.code()),
    }
}

/// Compare, or rewrite under `UPDATE_GOLDEN=1`.
fn check(case: &Case) {
    let actual = render(case);
    if actual == case.golden {
        return;
    }

    if std::env::var("UPDATE_GOLDEN").is_ok()
        && let Some(dir) = option_env!("CARGO_MANIFEST_DIR")
    {
        let path = std::path::Path::new(dir)
            .join("src/golden")
            .join(format!("{}.md", case.name));
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("create golden dir");
        std::fs::write(&path, &actual).expect("write golden");
        eprintln!("updated {}", path.display());
        return;
    }

    panic!(
        "golden mismatch for {name}\n\
         \n{diff}\n\
         If this change is intended, regenerate and READ the diff:\n  \
         UPDATE_GOLDEN=1 cargo test -p mcpg-markdown-convert --features all\n",
        name = case.name,
        diff = diff(case.golden, &actual),
    );
}

/// A line-oriented diff. `similar` is not in the tree and this does not
/// warrant adding it — the goal is to show which line moved, not to compute a
/// minimal edit script.
fn diff(expected: &str, actual: &str) -> String {
    let e: Vec<&str> = expected.lines().collect();
    let a: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    for i in 0..e.len().max(a.len()) {
        match (e.get(i), a.get(i)) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => {
                out.push_str(&format!("{:>4} -expected: {x:?}\n", i + 1));
                out.push_str(&format!("{:>4} +actual:   {y:?}\n", i + 1));
            }
            (Some(x), None) => out.push_str(&format!("{:>4} -expected: {x:?}\n", i + 1)),
            (None, Some(y)) => out.push_str(&format!("{:>4} +actual:   {y:?}\n", i + 1)),
            (None, None) => break,
        }
    }
    if out.is_empty() {
        // Equal line-by-line but not byte-equal: trailing whitespace or a
        // final-newline difference, which a line diff cannot show.
        out.push_str(&format!(
            "no line differs; byte lengths {} vs {} (trailing whitespace?)\n",
            expected.len(),
            actual.len()
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// One case. `include_str!` needs a literal, so the golden path is built
/// here rather than passed in.
macro_rules! case {
    ($name:literal, $filename:literal, $input:expr, $options:expr) => {
        Case {
            name: $name,
            filename: $filename,
            input: $input,
            options: $options,
            golden: include_str!(concat!("golden/", $name, ".md")),
        }
    };
}

const CSV_SRC: &str = "region,revenue,note\nEMEA,1200,\"needs | escaping\"\nAPAC,900,\n";

const JSON_TABLE_SRC: &str = r#"[{"id":1,"name":"ada"},{"id":2,"name":"grace","extra":true}]"#;

const JSON_NESTED_SRC: &str = r#"{"a":{"deep":[1,2,3]},"b":"top"}"#;

const XML_SRC: &str = r#"<?xml version="1.0"?>
<order id="7"><customer>Ada</customer><lines><line sku="A1" qty="2"/></lines></order>"#;

const RSS_SRC: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <title>Release Notes</title>
  <item><title>0.2.0</title><link>https://example.invalid/2</link>
    <pubDate>Mon, 02 Feb 2026 00:00:00 GMT</pubDate>
    <description>Adds markdown conversion.</description></item>
  <item><title>0.1.0</title><link>https://example.invalid/1</link>
    <description>First cut.</description></item>
</channel></rss>"#;

const HTML_SRC: &str = r#"<html><head><title>Quarterly</title>
<meta name="author" content="Ada"><style>.x{color:red}</style></head>
<body><h1>Q3</h1><p>Revenue <strong>rose</strong> to 1200.</p>
<ul><li>EMEA</li><li>APAC</li></ul>
<table><tr><th>region</th><th>eur</th></tr><tr><td>EMEA</td><td>1200</td></tr></table>
<script>alert(1)</script></body></html>"#;

const IPYNB_SRC: &str = r##"{
  "metadata": {"language_info": {"name": "python"}},
  "cells": [
    {"cell_type": "markdown", "source": ["# Analysis\n", "\n", "With **emphasis**.\n"]},
    {"cell_type": "code", "source": "print(1 + 1)\n",
     "outputs": [{"output_type": "stream", "text": ["2\n"]}]}
  ]
}"##;

const TEXT_SRC: &str = "First paragraph, soft\nwrapped across lines.\n\nSecond paragraph.\n";

const MARKDOWN_SRC: &str = "# Already Markdown\n\n- a list item\n- another\n\n`code` stays.\n";

fn docx_bytes() -> Vec<u8> {
    let core = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="http://x" xmlns:dc="http://purl.org/dc/elements/1.1/"
                   xmlns:dcterms="http://purl.org/dc/terms/">
  <dc:title>Quarterly Report</dc:title>
  <dc:creator>Ada Lovelace</dc:creator>
  <dcterms:created>2026-02-01T09:00:00Z</dcterms:created>
</cp:coreProperties>"#;
    let rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId4" Type="hyperlink" Target="https://example.invalid/detail"/>
</Relationships>"#;
    let body = concat!(
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Summary</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>Revenue </w:t></w:r>"#,
        r#"<w:r><w:rPr><w:b/></w:rPr><w:t>rose</w:t></w:r>"#,
        r#"<w:r><w:t> — see </w:t></w:r>"#,
        r#"<w:hyperlink r:id="rId4"><w:r><w:t>the detail</w:t></w:r></w:hyperlink></w:p>"#,
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/></w:numPr></w:pPr><w:r><w:t>EMEA up</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/></w:numPr></w:pPr><w:r><w:t>APAC flat</w:t></w:r></w:p>"#,
        r#"<w:tbl>"#,
        r#"<w:tr><w:tc><w:p><w:r><w:t>region</w:t></w:r></w:p></w:tc>"#,
        r#"<w:tc><w:p><w:r><w:t>eur</w:t></w:r></w:p></w:tc></w:tr>"#,
        r#"<w:tr><w:tc><w:p><w:r><w:t>EMEA</w:t></w:r></w:p></w:tc>"#,
        r#"<w:tc><w:p><w:r><w:t>1200</w:t></w:r></w:p></w:tc></w:tr>"#,
        r#"</w:tbl>"#,
    );
    crate::fixtures::docx_full(body, Some(core), Some(rels))
}

fn pptx_bytes() -> Vec<u8> {
    let one = crate::fixtures::slide_text(&["Agenda", "Numbers, then questions"]);
    let two = crate::fixtures::slide_text(&["Numbers"]);
    crate::fixtures::pptx(
        &[&one, &two],
        &[(
            1,
            "<a:p><a:r><a:t>Keep this to five minutes.</a:t></a:r></a:p>",
        )],
    )
}

fn epub_bytes() -> Vec<u8> {
    crate::fixtures::epub(
        &[
            ("c1", "Chapter One", "It began badly."),
            ("c2", "Chapter Two", "It got worse."),
        ],
        // Spine deliberately reversed: a zip walk would emit c1 first.
        &["c2", "c1"],
    )
}

fn zip_bytes() -> Vec<u8> {
    crate::fixtures::zip_of_text(&[("notes.txt", "A loose note.\n"), ("data.csv", "k,v\n1,2\n")])
}

fn pdf_bytes() -> Vec<u8> {
    crate::fixtures::pdf(&[
        "EXECUTIVE SUMMARY",
        "Revenue rose to 1200 in the third",
        "quarter.",
    ])
}

#[cfg(feature = "media")]
fn wav_bytes() -> Vec<u8> {
    // 8000 samples at 8 kHz — exactly one second, so the reported duration is
    // a round number rather than a rounding artefact.
    crate::fixtures::wav(8000)
}

#[cfg(feature = "email")]
fn msg_bytes() -> Vec<u8> {
    use crate::fixtures::utf16;
    crate::fixtures::msg(&[
        ("__substg1.0_0037001F", utf16("Quarterly numbers")),
        ("__substg1.0_0C1A001F", utf16("Ada <ada@example.invalid>")),
        (
            "__substg1.0_0E04001F",
            utf16("Grace <grace@example.invalid>"),
        ),
        (
            "__substg1.0_1000001F",
            utf16("Numbers attached.\n\nRegards,\nAda"),
        ),
        ("__substg1.0_3707001F", utf16("q3.xlsx")),
    ])
}

fn yaml_front_matter() -> ConvertOptions {
    ConvertOptions {
        output: RenderOptions {
            front_matter: FrontMatter::Yaml,
            ..RenderOptions::default()
        },
        ..ConvertOptions::default()
    }
}

fn html_tables() -> ConvertOptions {
    ConvertOptions {
        output: RenderOptions {
            tables: TableStyle::Html,
            ..RenderOptions::default()
        },
        ..ConvertOptions::default()
    }
}

fn csv_tables() -> ConvertOptions {
    ConvertOptions {
        output: RenderOptions {
            tables: TableStyle::Csv,
            ..RenderOptions::default()
        },
        ..ConvertOptions::default()
    }
}

fn heading_offset() -> ConvertOptions {
    ConvertOptions {
        output: RenderOptions {
            heading_offset: 2,
            ..RenderOptions::default()
        },
        ..ConvertOptions::default()
    }
}

fn templated() -> ConvertOptions {
    let mut blocks = std::collections::BTreeMap::new();
    blocks.insert(
        "table".to_owned(),
        "**{{ block.rows | length }} row(s)**\n\n{{ gfm_table(block) }}".to_owned(),
    );
    ConvertOptions {
        templates: Some(TemplateSpec {
            document: Some("---\nsource: {{ source.filename }}\n---\n\n{{ body }}".to_owned()),
            blocks,
        }),
        ..ConvertOptions::default()
    }
}

fn truncating() -> ConvertOptions {
    ConvertOptions {
        limits: Limits {
            max_table_rows: 2,
            ..Limits::default()
        },
        ..ConvertOptions::default()
    }
}

fn text_only() -> ConvertOptions {
    ConvertOptions {
        formats: FormatSelection {
            enable: Some(vec!["text".to_owned()]),
        },
        ..ConvertOptions::default()
    }
}

const WIDE_CSV: &str = "n\n1\n2\n3\n4\n5\n";

/// Every case in this build.
///
/// A function rather than a `const` array so the format groups can be
/// `cfg`-gated: a case whose converter is not compiled in would otherwise
/// fail with "unsupported" and record that as its expected output.
fn cases() -> Vec<Case> {
    let mut v = vec![
        // --- text family, always compiled in --------------------------
        case!(
            "text-paragraphs",
            "notes.txt",
            Input::Text(TEXT_SRC),
            default_options
        ),
        case!(
            "markdown-passthrough",
            "notes.md",
            Input::Text(MARKDOWN_SRC),
            default_options
        ),
        case!(
            "csv-table",
            "revenue.csv",
            Input::Text(CSV_SRC),
            default_options
        ),
        case!(
            "json-tabular",
            "rows.json",
            Input::Text(JSON_TABLE_SRC),
            default_options
        ),
        case!(
            "json-nested",
            "tree.json",
            Input::Text(JSON_NESTED_SRC),
            default_options
        ),
        case!(
            "ipynb-notebook",
            "analysis.ipynb",
            Input::Text(IPYNB_SRC),
            default_options
        ),
        case!(
            "xml-tree",
            "order.xml",
            Input::Text(XML_SRC),
            default_options
        ),
        case!(
            "rss-feed",
            "notes.rss",
            Input::Text(RSS_SRC),
            default_options
        ),
        // --- render options, exercised on one stable input ------------
        case!(
            "csv-front-matter",
            "revenue.csv",
            Input::Text(CSV_SRC),
            yaml_front_matter
        ),
        case!(
            "csv-html-tables",
            "revenue.csv",
            Input::Text(CSV_SRC),
            html_tables
        ),
        case!(
            "csv-csv-tables",
            "revenue.csv",
            Input::Text(CSV_SRC),
            csv_tables
        ),
        case!(
            "csv-templated",
            "revenue.csv",
            Input::Text(CSV_SRC),
            templated
        ),
        case!(
            "csv-truncated",
            "wide.csv",
            Input::Text(WIDE_CSV),
            truncating
        ),
        case!(
            "csv-format-disabled",
            "revenue.csv",
            Input::Text(CSV_SRC),
            text_only
        ),
        case!(
            "markdown-heading-offset",
            "notes.md",
            Input::Text(MARKDOWN_SRC),
            heading_offset
        ),
    ];

    #[cfg(feature = "web")]
    v.push(case!(
        "html-page",
        "quarterly.html",
        Input::Text(HTML_SRC),
        default_options
    ));

    #[cfg(feature = "office")]
    {
        v.push(case!(
            "docx-report",
            "report.docx",
            Input::Built(docx_bytes),
            default_options
        ));
        v.push(case!(
            "pptx-deck",
            "deck.pptx",
            Input::Built(pptx_bytes),
            default_options
        ));
        v.push(case!(
            "epub-spine-order",
            "book.epub",
            Input::Built(epub_bytes),
            default_options
        ));
        v.push(case!(
            "zip-archive",
            "bundle.zip",
            Input::Built(zip_bytes),
            default_options
        ));
    }

    #[cfg(feature = "pdf")]
    v.push(case!(
        "pdf-text-layer",
        "summary.pdf",
        Input::Built(pdf_bytes),
        default_options
    ));

    #[cfg(feature = "media")]
    {
        // Both record the metadata-only output an operator gets with
        // enrichment off, warning included. That warning is the contract:
        // a thin document must not look like a complete one.
        v.push(case!(
            "image-metadata-only",
            "chart.png",
            Input::Built(crate::fixtures::png),
            default_options
        ));
        v.push(case!(
            "audio-metadata-only",
            "clip.wav",
            Input::Built(wav_bytes),
            default_options
        ));
    }

    #[cfg(feature = "email")]
    v.push(case!(
        "msg-outlook",
        "mail.msg",
        Input::Built(msg_bytes),
        default_options
    ));

    v
}

#[cfg(test)]
mod run {
    use super::*;

    #[test]
    fn corpus_matches_the_recorded_output() {
        let mut failures = Vec::new();
        for case in &cases() {
            // Collect rather than fail fast: one renderer change usually
            // moves every golden, and seeing all of them at once is the
            // difference between one review and twenty re-runs.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(case))).is_err() {
                failures.push(case.name);
            }
        }
        assert!(
            failures.is_empty(),
            "{} golden case(s) differ: {}",
            failures.len(),
            failures.join(", ")
        );
    }

    #[test]
    fn every_case_has_a_distinct_name() {
        let all = cases();
        let mut names: Vec<&str> = all.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate case name shadows a golden");
    }

    #[test]
    fn the_corpus_covers_every_compiled_converter() {
        // A converter with no golden case is one whose output can change
        // unnoticed, which would leave a hole exactly where this module is
        // supposed to be load-bearing.
        //
        // `spreadsheet` is the one documented exception: calamine reads but
        // does not write, so a fixture would mean either a committed binary
        // blob (unreviewable, and it would ride into the OSS mirror) or a
        // writer dependency. Its behaviour stays covered by unit tests.
        const EXEMPT: &[&str] = &["spreadsheet"];

        let rendered: Vec<String> = cases().iter().map(render).collect();
        let missing: Vec<&str> = crate::available_formats()
            .into_iter()
            .filter(|f| !EXEMPT.contains(f))
            .filter(|f| {
                let marker = format!("<!-- converter: {f} via");
                !rendered.iter().any(|r| r.contains(&marker))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "converters with no golden case: {missing:?}"
        );
    }
}
