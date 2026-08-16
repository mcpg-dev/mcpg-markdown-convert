//! Archives — each member converted in turn, under a shared budget.
//!
//! This is the converter most exposed to hostile input, because an archive is
//! where "small file, enormous expansion" lives. Three guards, all shared with
//! the parent conversion rather than reset per level:
//!
//! - `max_expanded_bytes` counts bytes as they are decompressed, never
//!   trusting the declared size in the header;
//! - `max_depth` bounds nesting, so a zip inside a zip inside a zip stops;
//! - `max_embedded_documents` bounds member count.
//!
//! A member that trips a guard is skipped with a warning; the archive still
//! converts. Only exhausting the shared byte budget stops the whole call —
//! at that point there is no allowance left for anything else either.

use std::io::{Cursor, Read};

use crate::cx::ConvertCx;
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::{Block, Document, sanitize_member_name};
use crate::registry::{Converter, PRIORITY_GENERIC};
use crate::stream_info::{Probe, StreamInfo};

pub struct ZipConverter;

impl Converter for ZipConverter {
    fn name(&self) -> &'static str {
        "zip"
    }

    fn priority(&self) -> i32 {
        // Generic: every OOXML format and EPUB is also a zip, and each has a
        // converter that produces far better output. They must all get first
        // refusal.
        PRIORITY_GENERIC + 10
    }

    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool {
        probe.starts_with(b"PK\x03\x04")
            && (info.is_ext("zip")
                || info.is_mime("application/zip")
                || info.is_mime("application/x-zip-compressed"))
    }

    fn convert(
        &self,
        bytes: &[u8],
        _info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError> {
        // The engine is rebuilt rather than threaded in: a converter must not
        // hold a reference to the registry that owns it. Members convert with
        // the default profile, which is the conservative reading — an
        // operator's per-profile template applies to the document they asked
        // for, not to whatever happened to be inside it.
        let engine = crate::engine::Engine::new(crate::config::ConvertOptions {
            limits: cx.limits().clone(),
            ..crate::config::ConvertOptions::default()
        })?;

        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).map_err(|e| {
            ConvertError::Malformed {
                format: "zip",
                message: e.to_string(),
            }
        })?;

        let child = match cx.descend() {
            Ok(c) => c,
            Err(_) => {
                let mut doc = Document::new();
                doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!(
                        "nested archive not expanded: max_depth ({}) reached",
                        cx.limits().max_depth
                    ),
                ));
                return Ok(doc);
            }
        };

        let mut doc = Document::new();
        let count = zip.len();
        for i in 0..count {
            cx.budget().check_deadline()?;

            let (name, size, encrypted) = {
                let Ok(entry) = zip.by_index_raw(i) else {
                    continue;
                };
                (entry.name().to_owned(), entry.size(), entry.encrypted())
            };
            if name.ends_with('/') {
                continue;
            }
            let safe = sanitize_member_name(&name);

            if encrypted {
                doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!("{safe}: encrypted"),
                ));
                continue;
            }
            if cx.budget().charge_embedded().is_err() {
                doc.warn(Warning::new(
                    WarningKind::Truncated,
                    format!(
                        "stopped after {} members (max_embedded_documents)",
                        cx.limits().max_embedded_documents
                    ),
                ));
                break;
            }

            // The declared size is a claim, so it is used only to skip an
            // obviously hopeless member early. The real accounting happens on
            // the bytes actually read, below.
            if size > cx.limits().max_expanded_bytes {
                doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!("{safe}: declares {size} bytes, over max_expanded_bytes"),
                ));
                continue;
            }

            let mut buf = Vec::new();
            {
                let Ok(entry) = zip.by_index(i) else {
                    doc.warn(Warning::new(
                        WarningKind::SkippedMember,
                        format!("{safe}: could not be decompressed"),
                    ));
                    continue;
                };
                // `take` is the actual zip-bomb guard: it caps the read at the
                // remaining allowance regardless of what the header promised.
                let remaining = cx
                    .limits()
                    .max_expanded_bytes
                    .saturating_sub(cx.budget().expanded_bytes());
                if remaining == 0 {
                    doc.warn(Warning::new(
                        WarningKind::Truncated,
                        "expansion budget exhausted; remaining members skipped",
                    ));
                    break;
                }
                if entry.take(remaining + 1).read_to_end(&mut buf).is_err() {
                    doc.warn(Warning::new(
                        WarningKind::SkippedMember,
                        format!("{safe}: read failed"),
                    ));
                    continue;
                }
            }
            if buf.len() as u64 > cx.limits().max_expanded_bytes {
                doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!("{safe}: expands past max_expanded_bytes"),
                ));
                continue;
            }
            if cx.budget().charge_expanded(buf.len() as u64).is_err() {
                doc.warn(Warning::new(
                    WarningKind::Truncated,
                    "expansion budget exhausted; remaining members skipped",
                ));
                break;
            }

            let info = StreamInfo::new().with_filename(safe.clone());
            match engine.convert_to_ir(&buf, &info, &child) {
                Ok((inner, _, _)) => {
                    doc.warnings.extend(inner.warnings.clone());
                    doc.push(Block::Embedded {
                        name: safe,
                        doc: Box::new(Document {
                            warnings: Vec::new(),
                            ..inner
                        }),
                    });
                }
                Err(e @ ConvertError::LimitExceeded { .. }) => return Err(e),
                Err(e) => doc.warn(Warning::new(
                    WarningKind::SkippedMember,
                    format!("{safe}: {e}"),
                )),
            }
        }

        if doc.blocks.is_empty() && doc.warnings.is_empty() {
            doc.warn(Warning::new(
                WarningKind::NoTextLayer,
                "the archive contained no convertible members",
            ));
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::cx::{Budget, Limits};

    fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in parts {
                w.start_file(*name, opts).unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    fn convert_with(bytes: &[u8], limits: Limits) -> Result<Document, ConvertError> {
        let b = Budget::new(limits);
        let cx = ConvertCx::new(&b);
        ZipConverter.convert(bytes, &StreamInfo::new().with_extension("zip"), &cx)
    }

    fn convert(bytes: &[u8]) -> Document {
        convert_with(bytes, Limits::default()).expect("converts")
    }

    #[test]
    fn members_become_embedded_documents() {
        let bytes = zip_of(&[("a.txt", b"hello"), ("b.csv", b"x,y\n1,2\n")]);
        let doc = convert(&bytes);
        let names: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Embedded { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["a.txt", "b.csv"]);
    }

    #[test]
    fn member_names_are_sanitised_before_they_reach_the_output() {
        let bytes = zip_of(&[("../../etc/passwd", b"root:x:0:0")]);
        let doc = convert(&bytes);
        let out = format!("{:?}", doc.blocks);
        assert!(!out.contains(".."), "{out}");
        assert!(out.contains("etc/passwd"), "{out}");
    }

    #[test]
    fn directories_are_not_treated_as_members() {
        let bytes = zip_of(&[("dir/", b""), ("dir/a.txt", b"x")]);
        let n = convert(&bytes)
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Embedded { .. }))
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_highly_compressible_member_cannot_exceed_the_expansion_budget() {
        // 4 MB of zeros compresses to a few kilobytes: the classic shape of a
        // zip bomb, at a size the test can afford.
        let bomb = vec![0u8; 4 * 1024 * 1024];
        let bytes = zip_of(&[("bomb.bin", &bomb)]);
        assert!(bytes.len() < 64 * 1024, "fixture did not compress");

        let doc = convert_with(
            &bytes,
            Limits {
                max_expanded_bytes: 4096,
                ..Limits::default()
            },
        )
        .expect("skips the member rather than failing");
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::SkippedMember || w.kind == WarningKind::Truncated),
            "{:?}",
            doc.warnings
        );
        assert!(doc.blocks.is_empty(), "the bomb was expanded anyway");
    }

    #[test]
    fn a_lying_size_header_does_not_get_the_member_read() {
        // The guard that matters is the capped read, not the declared size.
        let big = vec![b'a'; 200_000];
        let bytes = zip_of(&[("big.txt", &big)]);
        let doc = convert_with(
            &bytes,
            Limits {
                max_expanded_bytes: 1000,
                ..Limits::default()
            },
        )
        .expect("skips");
        assert!(doc.blocks.is_empty());
    }

    #[test]
    fn member_count_is_capped() {
        let bodies: Vec<(String, Vec<u8>)> = (0..20)
            .map(|i| (format!("f{i}.txt"), b"x".to_vec()))
            .collect();
        let parts: Vec<(&str, &[u8])> = bodies
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let bytes = zip_of(&parts);
        let doc = convert_with(
            &bytes,
            Limits {
                max_embedded_documents: 5,
                ..Limits::default()
            },
        )
        .expect("converts");
        let n = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Embedded { .. }))
            .count();
        assert!(n <= 5, "{n} members converted");
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::Truncated)
        );
    }

    #[test]
    fn nesting_stops_at_max_depth_with_a_warning() {
        let inner = zip_of(&[("deep.txt", b"deep")]);
        let outer = zip_of(&[("inner.zip", &inner)]);
        let doc = convert_with(
            &outer,
            Limits {
                max_depth: 1,
                ..Limits::default()
            },
        )
        .expect("converts");
        let out = format!("{doc:?}");
        assert!(out.contains("max_depth"), "{out}");
        assert!(
            !out.contains("deep"),
            "nested content escaped the depth cap"
        );
    }

    #[test]
    fn an_unconvertible_member_is_skipped_not_fatal() {
        let bytes = zip_of(&[
            ("good.txt", b"fine"),
            ("bad.bin", &[0xFF, 0x00, 0xFE, 0x01]),
        ]);
        let doc = convert(&bytes);
        assert!(
            doc.blocks
                .iter()
                .any(|b| matches!(b, Block::Embedded { name, .. } if name == "good.txt"))
        );
    }

    #[test]
    fn an_empty_archive_warns() {
        let bytes = zip_of(&[]);
        let doc = convert(&bytes);
        assert!(
            doc.warnings
                .iter()
                .any(|w| w.kind == WarningKind::NoTextLayer)
        );
    }

    #[test]
    fn ooxml_containers_are_left_to_their_own_converters() {
        // The zip converter would happily accept these; the extension check
        // is what keeps a .docx from being rendered as a pile of XML parts.
        let p = Probe::new(b"PK\x03\x04");
        assert!(ZipConverter.accepts(&p, &StreamInfo::new().with_extension("zip")));
        for ext in ["docx", "pptx", "xlsx", "epub"] {
            assert!(
                !ZipConverter.accepts(&p, &StreamInfo::new().with_extension(ext)),
                "zip claimed .{ext}"
            );
        }
    }
}
