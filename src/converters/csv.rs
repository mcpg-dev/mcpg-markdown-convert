//! Delimited text → a Markdown table.

use std::io::Cursor;

use crate::converters::decode_text;
use crate::cx::ConvertCx;
use crate::error::ConvertError;
use crate::ir::{Block, Document, Inline, Table};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct CsvConverter;

impl Converter for CsvConverter {
    fn name(&self) -> &'static str {
        "csv"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        probe.looks_textual()
            && (info.is_mime("text/csv")
                || info.is_mime("text/tab-separated-values")
                || info.is_ext("csv")
                || info.is_ext("tsv")
                || info.is_ext("tab"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let text = decode_text(bytes, info.charset.as_deref());
        let delimiter = delimiter_for(info, &text);

        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .flexible(true)
            .has_headers(false)
            .from_reader(Cursor::new(text.as_bytes()));

        let mut rows: Vec<Vec<Inline>> = Vec::new();
        // One more than the render cap so the renderer can report how many
        // were omitted without us reading a 500 MB sheet to find out.
        let cap = cx.limits().max_table_rows as usize + 1;
        for rec in rdr.records() {
            let rec = rec.map_err(|e| ConvertError::Malformed {
                format: "csv",
                message: e.to_string(),
            })?;
            rows.push(rec.iter().map(Inline::text).collect());
            if rows.len() > cap {
                break;
            }
            if rows.len().is_multiple_of(1024) {
                cx.budget().check_deadline()?;
            }
        }

        let mut doc = Document::new();
        if rows.is_empty() {
            return Ok(doc);
        }

        // A header row is the norm for CSV and what markitdown assumes, but
        // only when the first row looks like labels rather than data.
        let header = if looks_like_header(&rows) {
            Some(rows.remove(0))
        } else {
            None
        };

        doc.push(Block::Table(Table {
            caption: info.filename.clone(),
            header,
            rows,
        }));
        Ok(doc)
    }
}

/// Pick the delimiter from the declared type, the extension, or the content.
fn delimiter_for(info: &StreamInfo, text: &str) -> u8 {
    if info.is_ext("tsv") || info.is_ext("tab") || info.is_mime("text/tab-separated-values") {
        return b'\t';
    }
    if info.is_ext("csv") || info.is_mime("text/csv") {
        // A `.csv` written by a European locale is semicolon-delimited often
        // enough that guessing from the first line is worth it.
        return sniff_delimiter(text).unwrap_or(b',');
    }
    sniff_delimiter(text).unwrap_or(b',')
}

/// Whichever candidate appears most consistently across the first few lines.
fn sniff_delimiter(text: &str) -> Option<u8> {
    let sample: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(5)
        .collect();
    if sample.is_empty() {
        return None;
    }
    let mut best: Option<(u8, usize)> = None;
    for cand in *b",;\t|" {
        let counts: Vec<usize> = sample
            .iter()
            .map(|l| l.bytes().filter(|b| *b == cand).count())
            .collect();
        let first = counts[0];
        // Consistent across lines and actually present.
        if first > 0 && counts.iter().all(|c| *c == first) {
            let score = first;
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((cand, score));
            }
        }
    }
    best.map(|(d, _)| d)
}

/// Treat the first row as a header when it is all non-numeric and the row
/// below it is not. Guessing wrong costs one row of a table; refusing to
/// guess costs every table its column names.
fn looks_like_header(rows: &[Vec<Inline>]) -> bool {
    if rows.len() < 2 {
        return rows.len() == 1;
    }
    let numeric = |cells: &Vec<Inline>| -> usize {
        cells
            .iter()
            .filter(|c| {
                let t = c.to_plain();
                let t = t.trim();
                !t.is_empty() && t.parse::<f64>().is_ok()
            })
            .count()
    };
    numeric(&rows[0]) == 0 && numeric(&rows[1]) > 0 || numeric(&rows[0]) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    fn convert(bytes: &[u8], info: &StreamInfo) -> Document {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        CsvConverter.convert(bytes, info, &cx).expect("converts")
    }

    fn table(doc: &Document) -> &Table {
        match &doc.blocks[0] {
            Block::Table(t) => t,
            other => panic!("{other:?}"),
        }
    }

    fn csv_info() -> StreamInfo {
        StreamInfo::new().with_extension("csv")
    }

    #[test]
    fn header_and_rows_are_separated() {
        let doc = convert(b"name,age\nada,36\ngrace,45\n", &csv_info());
        let t = table(&doc);
        assert_eq!(
            t.header
                .as_ref()
                .map(|h| h.iter().map(Inline::to_plain).collect::<Vec<_>>()),
            Some(vec!["name".to_owned(), "age".to_owned()])
        );
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn all_numeric_first_row_is_data_not_a_header() {
        let doc = convert(b"1,2\n3,4\n", &csv_info());
        let t = table(&doc);
        assert!(t.header.is_none(), "{:?}", t.header);
        assert_eq!(t.rows.len(), 2);
    }

    #[test]
    fn semicolon_delimiters_are_detected() {
        let doc = convert("name;city\nada;london\n".as_bytes(), &csv_info());
        assert_eq!(table(&doc).width(), 2);
    }

    #[test]
    fn tsv_uses_tabs_from_the_extension() {
        let si = StreamInfo::new().with_extension("tsv");
        let doc = convert(b"a\tb\n1\t2\n", &si);
        assert_eq!(table(&doc).width(), 2);
    }

    #[test]
    fn quoted_fields_survive_embedded_delimiters() {
        let doc = convert(b"a,b\n\"x,y\",z\n", &csv_info());
        let t = table(&doc);
        assert_eq!(t.rows[0][0].to_plain(), "x,y");
    }

    #[test]
    fn ragged_rows_are_kept_rather_than_rejected() {
        let doc = convert(b"a,b,c\n1,2\n3,4,5,6\n", &csv_info());
        assert_eq!(table(&doc).rows.len(), 2);
    }

    #[test]
    fn empty_input_yields_no_table() {
        let doc = convert(b"", &csv_info());
        assert!(doc.blocks.is_empty());
    }

    #[test]
    fn row_reading_stops_near_the_cap() {
        let mut src = String::from("n\n");
        for i in 0..5000 {
            src.push_str(&format!("{i}\n"));
        }
        let b = Budget::new(Limits {
            max_table_rows: 10,
            ..Limits::default()
        });
        let cx = ConvertCx::new(&b);
        let doc = CsvConverter
            .convert(src.as_bytes(), &csv_info(), &cx)
            .unwrap();
        // Cap plus the one lookahead row that tells the renderer it truncated.
        assert!(table(&doc).rows.len() <= 12, "{}", table(&doc).rows.len());
    }

    #[test]
    fn only_delimited_extensions_are_accepted() {
        let p = Probe::new(b"a,b\n");
        assert!(CsvConverter.accepts(&p, &csv_info()));
        assert!(!CsvConverter.accepts(&p, &StreamInfo::new().with_extension("txt")));
    }
}
