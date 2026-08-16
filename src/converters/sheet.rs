//! Spreadsheets — XLSX, XLSM, XLSB, legacy XLS, and ODS — via `calamine`.
//!
//! One dependency covers what markitdown needs two optional extras for
//! (`[xlsx]` on openpyxl and `[xls]` on xlrd), and it is pure Rust, so the
//! plugin still cross-compiles to musl and windows-gnu.
//!
//! Each sheet becomes its own heading plus table. markitdown concatenates
//! every sheet into one blob; for a 50-sheet workbook that is unreadable and
//! blows the context window, so sheets stay separated and the per-table row
//! cap applies to each.

use std::io::Cursor;

use calamine::{Data, Reader};

use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document, Inline, Table};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct SpreadsheetConverter;

impl Converter for SpreadsheetConverter {
    fn name(&self) -> &'static str {
        "spreadsheet"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        let named = info.is_ext("xlsx")
            || info.is_ext("xlsm")
            || info.is_ext("xlsb")
            || info.is_ext("xls")
            || info.is_ext("ods")
            || info.is_mime("application/vnd.ms-excel")
            || info
                .mimetype
                .as_deref()
                .is_some_and(|m| m.contains("spreadsheet"));
        if !named {
            return false;
        }
        // XLSX/XLSM/ODS are zips; XLS and XLSB are not. Checking the
        // container keeps a mislabelled file from reaching a parser that will
        // certainly fail on it.
        probe.starts_with(b"PK\x03\x04")
            || probe.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1])
    }

    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let cursor = Cursor::new(bytes.to_vec());
        let mut workbook =
            calamine::open_workbook_auto_from_rs(cursor).map_err(|e| map_open_error(&e))?;

        let mut doc = Document::new();
        if let Some(f) = &info.filename {
            doc = doc.with_title(f.clone());
        }

        let names = workbook.sheet_names().to_vec();
        if names.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                "the workbook contains no sheets",
            ));
            return Ok(doc);
        }

        let max_rows = cx.limits().max_table_rows as usize;
        for name in names {
            cx.budget().check_deadline()?;
            let Ok(range) = workbook.worksheet_range(&name) else {
                doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!("sheet {name:?} could not be read"),
                ));
                continue;
            };

            doc.push(Block::Heading {
                level: 2,
                text: Inline::text(name.clone()),
            });

            if range.is_empty() {
                doc.push(Block::Paragraph(Inline::text("(empty sheet)")));
                continue;
            }

            // Charge the cell count, not the file size: a sparse sheet with a
            // cell at ZZ100000 expands to a huge range from a tiny file.
            let cells = (range.height() as u64).saturating_mul(range.width() as u64);
            cx.budget().charge_expanded(cells)?;

            let mut rows: Vec<Vec<Inline>> = Vec::new();
            for row in range.rows().take(max_rows + 1) {
                rows.push(row.iter().map(|c| Inline::text(cell_text(c))).collect());
            }
            // Trailing empty rows are an artefact of the used-range, not data.
            while rows.last().is_some_and(|r| r.iter().all(Inline::is_blank)) {
                rows.pop();
            }
            if rows.is_empty() {
                doc.push(Block::Paragraph(Inline::text("(empty sheet)")));
                continue;
            }

            let total = range.height();
            if total > max_rows {
                doc.warn(Warning::new(
                    WarningKind::Truncated,
                    format!("sheet {name:?}: {total} rows, capped at {max_rows}"),
                ));
            }

            let header = if rows.len() > 1 && rows[0].iter().any(|c| !c.is_blank()) {
                Some(rows.remove(0))
            } else {
                None
            };
            doc.push(Block::Table(Table {
                caption: None,
                header,
                rows,
            }));
        }

        Ok(doc)
    }
}

/// `calamine` reports a password-protected workbook as an ordinary read
/// failure; separating it matters because the operator remedy differs.
fn map_open_error(e: &calamine::Error) -> ConvertError {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("password") || lower.contains("encrypt") {
        return ConvertError::Encrypted {
            format: "spreadsheet",
        };
    }
    ConvertError::Malformed {
        format: "spreadsheet",
        message: text,
    }
}

/// A cell as text.
///
/// Formulas are rendered as their cached value, not their source: a model
/// reading a report wants `1234`, not `=SUM(B2:B9)`. Dates come through as
/// their ISO form where calamine can resolve one.
fn cell_text(c: &Data) -> String {
    match c {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => {
            // Excel stores every number as a float; an integral one should
            // not render as "36.0".
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        // Formatted from the serial's components rather than through chrono:
        // `as_datetime` needs calamine's date feature, which would pull a
        // whole date library in for one `to_string`.
        Data::DateTime(d) => {
            if d.is_duration() {
                let hours = d.as_f64() * 24.0;
                format!("{hours:.4} h")
            } else {
                let (y, mo, da, h, mi, s, _ms) = d.to_ymd_hms_milli();
                if h == 0 && mi == 0 && s == 0 {
                    format!("{y:04}-{mo:02}-{da:02}")
                } else {
                    format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{s:02}")
                }
            }
        }
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#{e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    #[test]
    fn integral_floats_do_not_grow_a_decimal_point() {
        assert_eq!(cell_text(&Data::Float(36.0)), "36");
        assert_eq!(cell_text(&Data::Float(1.5)), "1.5");
    }

    #[test]
    fn empty_cells_render_as_nothing() {
        assert_eq!(cell_text(&Data::Empty), "");
    }

    #[test]
    fn booleans_and_ints_round_trip() {
        assert_eq!(cell_text(&Data::Bool(true)), "true");
        assert_eq!(cell_text(&Data::Int(-7)), "-7");
    }

    #[test]
    fn only_spreadsheet_containers_are_accepted() {
        let xlsx = StreamInfo::new().with_extension("xlsx");
        assert!(SpreadsheetConverter.accepts(&Probe::new(b"PK\x03\x04..."), &xlsx));
        // Right extension, wrong container: decline so another guess can try.
        assert!(!SpreadsheetConverter.accepts(&Probe::new(b"hello"), &xlsx));
        // Right container, unrelated extension.
        assert!(!SpreadsheetConverter.accepts(
            &Probe::new(b"PK\x03\x04..."),
            &StreamInfo::new().with_extension("docx")
        ));
    }

    #[test]
    fn a_non_workbook_is_malformed_not_a_panic() {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        let e = SpreadsheetConverter
            .convert(
                b"PK\x03\x04not really",
                &StreamInfo::new().with_extension("xlsx"),
                &cx,
            )
            .unwrap_err();
        assert!(matches!(
            e,
            ConvertError::Malformed { .. } | ConvertError::Encrypted { .. }
        ));
    }

    #[test]
    fn a_password_protected_workbook_is_reported_as_encrypted() {
        let e = map_open_error(&calamine::Error::Msg("workbook is password protected"));
        assert_eq!(e.code(), "encrypted");
    }
}
