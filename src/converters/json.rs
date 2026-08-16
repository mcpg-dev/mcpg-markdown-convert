//! JSON, NDJSON and Jupyter notebooks.

use serde_json::Value;

use crate::converters::decode_text;
use crate::cx::ConvertCx;
use crate::error::ConvertError;
use crate::ir::{Block, Document, Inline, Table};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct JsonConverter;

impl Converter for JsonConverter {
    fn name(&self) -> &'static str {
        "json"
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        if !probe.looks_textual() {
            return false;
        }
        if info.is_ext("ipynb") {
            // The notebook converter's job, and it produces far better output.
            return false;
        }
        info.is_mime("application/json")
            || info.is_mime("application/x-ndjson")
            || info.is_ext("json")
            || info.is_ext("jsonl")
            || info.is_ext("ndjson")
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let text = decode_text(bytes, info.charset.as_deref());
        let ndjson =
            info.is_ext("jsonl") || info.is_ext("ndjson") || info.is_mime("application/x-ndjson");

        let value = if ndjson {
            let mut rows = Vec::new();
            for (n, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                rows.push(serde_json::from_str::<Value>(line).map_err(|e| {
                    ConvertError::Malformed {
                        format: "json",
                        message: format!("line {}: {e}", n + 1),
                    }
                })?);
            }
            Value::Array(rows)
        } else {
            serde_json::from_str::<Value>(&text).map_err(|e| ConvertError::Malformed {
                format: "json",
                message: e.to_string(),
            })?
        };

        let mut doc = Document::new();
        if let Some(t) = value_to_table(&value, cx.limits().max_table_rows as usize) {
            doc.push(Block::Table(t));
        } else {
            doc.push(Block::Code {
                language: Some("json".to_owned()),
                text: serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.clone()),
            });
        }
        Ok(doc)
    }
}

/// An array of flat objects is a table, and a table is what a model reads
/// best. Anything else stays JSON — inventing structure for a deeply nested
/// document loses more than it gains.
fn value_to_table(value: &Value, max_rows: usize) -> Option<Table> {
    let arr = value.as_array()?;
    if arr.is_empty() || arr.len() > max_rows.saturating_mul(2) {
        return None;
    }
    let mut columns: Vec<String> = Vec::new();
    for item in arr {
        let obj = item.as_object()?;
        for (k, v) in obj {
            if v.is_object() || v.is_array() {
                return None;
            }
            if !columns.iter().any(|c| c == k) {
                columns.push(k.clone());
            }
        }
    }
    if columns.is_empty() {
        return None;
    }

    let rows = arr
        .iter()
        .take(max_rows + 1)
        .map(|item| {
            let obj = item.as_object().expect("checked above");
            columns
                .iter()
                .map(|c| Inline::text(scalar_to_string(obj.get(c))))
                .collect()
        })
        .collect();

    Some(Table {
        caption: None,
        header: Some(columns.into_iter().map(Inline::text).collect()),
        rows,
    })
}

fn scalar_to_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Jupyter notebooks. Markdown cells pass through, code cells become fenced
/// blocks, and outputs are included as text so the notebook reads the way it
/// looked when it ran.
pub struct IpynbConverter;

impl Converter for IpynbConverter {
    fn name(&self) -> &'static str {
        "ipynb"
    }

    fn priority(&self) -> i32 {
        // Ahead of the JSON converter, which would also parse it.
        PRIORITY_SPECIFIC - 1
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        probe.looks_textual() && (info.is_ext("ipynb") || info.is_mime("application/x-ipynb+json"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let text = decode_text(bytes, info.charset.as_deref());
        let nb: Value = serde_json::from_str(&text).map_err(|e| ConvertError::Malformed {
            format: "ipynb",
            message: e.to_string(),
        })?;

        let mut doc = Document::new();
        let language = nb
            .pointer("/metadata/kernelspec/language")
            .or_else(|| nb.pointer("/metadata/language_info/name"))
            .and_then(Value::as_str)
            .unwrap_or("python")
            .to_owned();
        doc.metadata.language = Some(language.clone());
        if let Some(t) = nb
            .pointer("/metadata/title")
            .and_then(Value::as_str)
            .or_else(|| {
                nb.pointer("/metadata/kernelspec/display_name")
                    .and_then(Value::as_str)
            })
        {
            doc = doc.with_title(t);
        }

        let cells =
            nb.get("cells")
                .and_then(Value::as_array)
                .ok_or_else(|| ConvertError::Malformed {
                    format: "ipynb",
                    message: "no `cells` array".to_owned(),
                })?;

        for cell in cells {
            cx.budget().check_deadline()?;
            let source = join_source(cell.get("source"));
            match cell.get("cell_type").and_then(Value::as_str) {
                Some("markdown") => {
                    if !source.trim().is_empty() {
                        // Notebook markdown is markdown; escaping it would
                        // destroy every heading and list in the document.
                        doc.push(Block::Raw {
                            markdown: source.trim_end().to_owned(),
                        });
                    }
                }
                Some("code") => {
                    if !source.trim().is_empty() {
                        doc.push(Block::Code {
                            language: Some(language.clone()),
                            text: source,
                        });
                    }
                    for out in outputs(cell) {
                        doc.push(out);
                    }
                }
                Some("raw") if !source.trim().is_empty() => {
                    doc.push(Block::Code {
                        language: None,
                        text: source,
                    });
                }
                _ => {}
            }
        }

        if doc.title.is_none() {
            doc.title = first_heading(&doc);
        }
        Ok(doc)
    }
}

/// Notebook `source` is either a string or an array of lines.
fn join_source(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(lines)) => lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

/// Cell outputs as text. Images are named rather than embedded: a notebook
/// carries them as base64 PNGs, and inlining those would multiply the
/// document size for no gain to a reader.
fn outputs(cell: &Value) -> Vec<Block> {
    let Some(outs) = cell.get("outputs").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for o in outs {
        let text = match o.get("output_type").and_then(Value::as_str) {
            Some("stream") => join_source(o.get("text")),
            Some("execute_result" | "display_data") => {
                let plain = join_source(o.pointer("/data/text~1plain"));
                if plain.trim().is_empty() && o.pointer("/data/image~1png").is_some() {
                    "[image output]".to_owned()
                } else {
                    plain
                }
            }
            Some("error") => o
                .get("traceback")
                .and_then(Value::as_array)
                .map(|t| {
                    t.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| {
                    format!(
                        "{}: {}",
                        o.get("ename").and_then(Value::as_str).unwrap_or("error"),
                        o.get("evalue").and_then(Value::as_str).unwrap_or_default()
                    )
                }),
            _ => String::new(),
        };
        if text.trim().is_empty() {
            continue;
        }
        blocks.push(Block::Code {
            language: Some("text".to_owned()),
            text: text.trim_end().to_owned(),
        });
    }
    blocks
}

fn first_heading(doc: &Document) -> Option<String> {
    doc.blocks.iter().find_map(|b| match b {
        Block::Raw { markdown } => markdown
            .lines()
            .find(|l| l.starts_with("# "))
            .map(|l| l.trim_start_matches('#').trim().to_owned()),
        _ => None,
    })
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

    fn json_info() -> StreamInfo {
        StreamInfo::new().with_extension("json")
    }

    #[test]
    fn an_array_of_flat_objects_becomes_a_table() {
        let doc = run(
            &JsonConverter,
            br#"[{"a":1,"b":"x"},{"a":2,"b":"y"}]"#,
            &json_info(),
        );
        match &doc.blocks[0] {
            Block::Table(t) => {
                assert_eq!(t.rows.len(), 2);
                assert_eq!(t.width(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ragged_objects_union_their_keys() {
        let doc = run(&JsonConverter, br#"[{"a":1},{"b":2}]"#, &json_info());
        match &doc.blocks[0] {
            Block::Table(t) => assert_eq!(t.width(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nested_json_stays_json() {
        let doc = run(&JsonConverter, br#"[{"a":{"deep":1}}]"#, &json_info());
        match &doc.blocks[0] {
            Block::Code { language, .. } => assert_eq!(language.as_deref(), Some("json")),
            other => panic!("nested data must not be flattened into a table: {other:?}"),
        }
    }

    #[test]
    fn ndjson_is_read_line_by_line() {
        let si = StreamInfo::new().with_extension("ndjson");
        let doc = run(&JsonConverter, b"{\"a\":1}\n\n{\"a\":2}\n", &si);
        match &doc.blocks[0] {
            Block::Table(t) => assert_eq!(t.rows.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_json_reports_the_position() {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        let err = JsonConverter
            .convert(b"{not json", &json_info(), &cx)
            .unwrap_err();
        assert_eq!(err.code(), "malformed");
    }

    #[test]
    fn ndjson_error_names_the_line() {
        let si = StreamInfo::new().with_extension("ndjson");
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        let err = JsonConverter
            .convert(b"{\"a\":1}\nnope\n", &si, &cx)
            .unwrap_err();
        assert!(format!("{err}").contains("line 2"), "{err}");
    }

    // --- notebooks --------------------------------------------------------

    fn notebook() -> &'static [u8] {
        // `br##` rather than `br#`: the notebook body contains `"#`, which
        // would close a single-hash raw string.
        br##"{
          "metadata": {"language_info": {"name": "python"}},
          "cells": [
            {"cell_type": "markdown", "source": ["# Analysis\n", "\n", "Some **prose**.\n"]},
            {"cell_type": "code", "source": "print(1)\n", "outputs": [
              {"output_type": "stream", "text": ["1\n"]}
            ]},
            {"cell_type": "code", "source": "boom()", "outputs": [
              {"output_type": "error", "ename": "ValueError", "evalue": "bad"}
            ]}
          ]
        }"##
    }

    fn ipynb_info() -> StreamInfo {
        StreamInfo::new().with_extension("ipynb")
    }

    #[test]
    fn notebook_markdown_cells_are_not_re_escaped() {
        let doc = run(&IpynbConverter, notebook(), &ipynb_info());
        match &doc.blocks[0] {
            Block::Raw { markdown } => assert!(markdown.contains("**prose**")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn notebook_code_cells_carry_the_kernel_language() {
        let doc = run(&IpynbConverter, notebook(), &ipynb_info());
        let langs: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Code { language, .. } => language.clone(),
                _ => None,
            })
            .collect();
        assert!(langs.contains(&"python".to_owned()), "{langs:?}");
    }

    #[test]
    fn notebook_outputs_are_included() {
        let doc = run(&IpynbConverter, notebook(), &ipynb_info());
        let all = format!("{:?}", doc.blocks);
        assert!(all.contains("ValueError"), "error output missing");
    }

    #[test]
    fn notebook_title_comes_from_the_first_heading() {
        let doc = run(&IpynbConverter, notebook(), &ipynb_info());
        assert_eq!(doc.title.as_deref(), Some("Analysis"));
    }

    #[test]
    fn the_json_converter_stands_aside_for_notebooks() {
        let p = Probe::new(b"{}");
        assert!(!JsonConverter.accepts(&p, &ipynb_info()));
        assert!(IpynbConverter.accepts(&p, &ipynb_info()));
    }

    #[test]
    fn a_notebook_without_cells_is_malformed() {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        let err = IpynbConverter
            .convert(b"{\"metadata\":{}}", &ipynb_info(), &cx)
            .unwrap_err();
        assert!(format!("{err}").contains("cells"), "{err}");
    }
}
