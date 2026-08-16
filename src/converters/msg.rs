//! Outlook `.msg` — an OLE compound file carrying MAPI property streams.
//!
//! `cfb` reads the container (the job `olefile` does for markitdown); the MAPI
//! decoding on top is ours. A property stream is named
//! `__substg1.0_<TAG><TYPE>`, where `TAG` identifies the field and `TYPE` says
//! how the bytes are encoded — `001F` for UTF-16LE, `001E` for 8-bit.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use crate::converters::squeeze;
use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document, Inline, Span};
use crate::registry::{Converter, PRIORITY_SPECIFIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct OutlookMsgConverter;

impl Converter for OutlookMsgConverter {
    fn name(&self) -> &'static str {
        "msg"
    }

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        // Every OLE compound file shares this signature — legacy .doc/.xls
        // too — so the extension or MIME must agree before we claim it.
        probe.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1])
            && (info.is_ext("msg") || info.is_mime("application/vnd.ms-outlook"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        _info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        let mut comp = cfb::CompoundFile::open(Cursor::new(bytes.to_vec())).map_err(|e| {
            ConvertError::Malformed {
                format: "msg",
                message: e.to_string(),
            }
        })?;

        let paths: Vec<String> = comp
            .walk()
            .filter(|e| e.is_stream())
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();

        let mut props: BTreeMap<String, String> = BTreeMap::new();
        let mut attachments: Vec<String> = Vec::new();

        for path in paths {
            cx.budget().check_deadline()?;
            let Some(name) = path.rsplit('/').next() else {
                continue;
            };
            let Some((tag, kind)) = parse_stream_name(name) else {
                continue;
            };
            let Some(field) = TAGS.iter().find(|(t, _)| *t == tag).map(|(_, f)| *f) else {
                continue;
            };

            let mut buf = Vec::new();
            {
                let Ok(stream) = comp.open_stream(&path) else {
                    continue;
                };
                let remaining = cx
                    .limits()
                    .max_expanded_bytes
                    .saturating_sub(cx.budget().expanded_bytes());
                if remaining == 0 {
                    break;
                }
                if stream.take(remaining + 1).read_to_end(&mut buf).is_err() {
                    continue;
                }
            }
            cx.budget().charge_expanded(buf.len() as u64)?;

            let value = decode_property(&buf, kind);
            if value.trim().is_empty() {
                continue;
            }
            // An attachment filename appears once per attachment sub-storage,
            // so it collects into a list rather than overwriting.
            if field == "attachment" {
                attachments.push(squeeze(&value));
            } else {
                props.entry(field.to_owned()).or_insert(value);
            }
        }

        let mut doc = Document::new();
        if let Some(subject) = props.get("subject") {
            doc = doc.with_title(squeeze(subject));
        }
        if let Some(from) = props.get("from") {
            doc.metadata.author = Some(squeeze(from));
        }
        if let Some(date) = props.get("date") {
            doc.metadata.created = Some(squeeze(date));
        }

        // The envelope reads better as a small table than as prose.
        let mut header_rows: Vec<Vec<Inline>> = Vec::new();
        for (label, key) in [
            ("From", "from"),
            ("To", "to"),
            ("Cc", "cc"),
            ("Date", "date"),
            ("Subject", "subject"),
        ] {
            if let Some(v) = props.get(key) {
                header_rows.push(vec![Inline::text(label), Inline::text(squeeze(v))]);
            }
        }
        if !header_rows.is_empty() {
            doc.push(Block::Table(crate::ir::Table {
                caption: None,
                header: None,
                rows: header_rows,
            }));
        }

        if !attachments.is_empty() {
            doc.push(Block::Paragraph(Inline(vec![
                Span::Strong(Inline::text("Attachments")),
                Span::Text(format!(": {}", attachments.join(", "))),
            ])));
            doc.warn(Warning::new(
                WarningKind::SkippedMember,
                format!(
                    "{} attachment(s) named but not converted",
                    attachments.len()
                ),
            ));
        }

        match props.get("body") {
            Some(body) => {
                doc.push(Block::Rule);
                for para in body.split("\n\n") {
                    let para = para.trim();
                    if !para.is_empty() {
                        doc.push(Block::Paragraph(Inline::text(para.replace('\n', " "))));
                    }
                }
            }
            None => {
                if props.contains_key("body_html") || props.contains_key("body_rtf") {
                    doc.warn(Warning::new(
                        WarningKind::Degraded,
                        "the message has no plain-text body; the HTML/RTF alternative is \
                         not converted",
                    ));
                } else {
                    doc.warn(Warning::new(
                        WarningKind::NoTextLayer,
                        "the message carried no body",
                    ));
                }
            }
        }

        if doc.blocks.is_empty() {
            return Err(ConvertError::Malformed {
                format: "msg",
                message: "no MAPI property streams — not an Outlook message".to_owned(),
            });
        }
        Ok(doc)
    }
}

/// `__substg1.0_0037001F` → (`"0037"`, `"001F"`).
fn parse_stream_name(name: &str) -> Option<(String, &'static str)> {
    let hex = name.strip_prefix("__substg1.0_")?;
    if hex.len() < 8 {
        return None;
    }
    let (tag, kind) = hex.split_at(4);
    let kind = match &kind[..4].to_ascii_uppercase()[..] {
        "001F" => "utf16",
        "001E" => "ascii",
        "0102" => "binary",
        _ => return None,
    };
    Some((tag.to_ascii_uppercase(), kind))
}

/// MAPI property tags we care about. Everything else is routing metadata a
/// reader does not want.
const TAGS: &[(&str, &str)] = &[
    ("0037", "subject"),
    ("1000", "body"),
    ("1013", "body_html"),
    ("1009", "body_rtf"),
    ("0C1A", "from"),
    ("0065", "from"),
    ("0E04", "to"),
    ("0E03", "cc"),
    ("0E06", "date"),
    ("3007", "date"),
    ("3707", "attachment"),
    ("3704", "attachment"),
];

fn decode_property(bytes: &[u8], kind: &str) -> String {
    match kind {
        "utf16" => {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        "ascii" => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
        // A binary property is not text; naming it is all we can honestly do.
        _ => String::new(),
    }
    .trim_end_matches('\0')
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Budget, Limits};

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// Build an OLE compound file holding the given MAPI property streams.
    /// `create_with`, not `create`: the latter wants a filesystem path, and
    /// nothing in this crate is allowed to touch the disk.
    fn msg_of(props: &[(&str, Vec<u8>)]) -> Vec<u8> {
        use std::io::Write;
        let mut comp = cfb::OpenOptions::new()
            .create_with(Cursor::new(Vec::new()))
            .expect("create cfb");
        for (name, body) in props {
            let path = format!("/{name}");
            let mut s = comp.create_stream(&path).expect("create stream");
            s.write_all(body).expect("write");
            s.flush().expect("flush stream");
        }
        comp.into_inner().into_inner()
    }

    fn convert(bytes: &[u8]) -> Result<Document, ConvertError> {
        let b = Budget::new(Limits::default());
        let cx = ConvertCx::new(&b);
        OutlookMsgConverter.convert(bytes, &StreamInfo::new().with_extension("msg"), &cx)
    }

    fn sample() -> Vec<u8> {
        msg_of(&[
            ("__substg1.0_0037001F", utf16("Quarterly numbers")),
            ("__substg1.0_0C1A001F", utf16("Ada <ada@example.invalid>")),
            (
                "__substg1.0_0E04001F",
                utf16("Grace <grace@example.invalid>"),
            ),
            ("__substg1.0_1000001F", utf16("First line.\n\nSecond line.")),
        ])
    }

    #[test]
    fn the_subject_becomes_the_title() {
        let doc = convert(&sample()).expect("converts");
        assert_eq!(doc.title.as_deref(), Some("Quarterly numbers"));
    }

    #[test]
    fn the_envelope_renders_as_a_table() {
        let doc = convert(&sample()).expect("converts");
        let out = format!("{:?}", doc.blocks);
        assert!(out.contains("ada@example.invalid"), "{out}");
        assert!(out.contains("grace@example.invalid"), "{out}");
        assert!(doc.blocks.iter().any(|b| matches!(b, Block::Table(_))));
    }

    #[test]
    fn the_body_splits_into_paragraphs() {
        let doc = convert(&sample()).expect("converts");
        let paras: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(i) => Some(i.to_plain()),
                _ => None,
            })
            .collect();
        assert!(paras.contains(&"First line.".to_owned()), "{paras:?}");
        assert!(paras.contains(&"Second line.".to_owned()), "{paras:?}");
    }

    #[test]
    fn ascii_properties_decode_too() {
        let bytes = msg_of(&[
            ("__substg1.0_0037001E", b"Plain subject".to_vec()),
            ("__substg1.0_1000001E", b"Body text".to_vec()),
        ]);
        let doc = convert(&bytes).expect("converts");
        assert_eq!(doc.title.as_deref(), Some("Plain subject"));
    }

    #[test]
    fn attachments_are_named_and_flagged_as_unconverted() {
        let mut props = vec![
            ("__substg1.0_0037001F", utf16("With attachment")),
            ("__substg1.0_1000001F", utf16("see attached")),
        ];
        props.push(("__substg1.0_3707001F", utf16("report.pdf")));
        let doc = convert(&msg_of(&props)).expect("converts");
        let out = format!("{:?}", doc.blocks);
        assert!(out.contains("report.pdf"), "{out}");
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::SkippedMember),
            "an unconverted attachment must not be silent"
        );
    }

    #[test]
    fn an_html_only_body_is_reported_rather_than_dropped_silently() {
        let bytes = msg_of(&[
            ("__substg1.0_0037001F", utf16("HTML only")),
            ("__substg1.0_1013001F", utf16("<p>hi</p>")),
        ]);
        let doc = convert(&bytes).expect("converts");
        assert!(
            doc.warnings.iter().any(|w| w.kind == WarningKind::Degraded),
            "{:?}",
            doc.warnings
        );
    }

    #[test]
    fn a_compound_file_with_no_mapi_streams_is_rejected() {
        let bytes = msg_of(&[("Workbook", b"not a message".to_vec())]);
        let e = convert(&bytes).unwrap_err();
        assert_eq!(e.code(), "malformed");
    }

    #[test]
    fn a_non_ole_file_is_malformed_not_a_panic() {
        assert!(convert(b"not compound at all").is_err());
    }

    #[test]
    fn stream_names_parse_into_tag_and_encoding() {
        assert_eq!(
            parse_stream_name("__substg1.0_0037001F"),
            Some(("0037".to_owned(), "utf16"))
        );
        assert_eq!(
            parse_stream_name("__substg1.0_1000001E"),
            Some(("1000".to_owned(), "ascii"))
        );
        assert_eq!(parse_stream_name("Workbook"), None);
        assert_eq!(parse_stream_name("__substg1.0_00"), None);
    }

    #[test]
    fn only_ole_files_named_msg_are_accepted() {
        let ole = Probe::new(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0]);
        assert!(OutlookMsgConverter.accepts(&ole, &StreamInfo::new().with_extension("msg")));
        // Legacy .xls shares the container; it must not be read as a message.
        assert!(!OutlookMsgConverter.accepts(&ole, &StreamInfo::new().with_extension("xls")));
    }
}
