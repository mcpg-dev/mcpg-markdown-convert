//! Format detection — the prioritised guess ladder.
//!
//! markitdown sniffs content with Magika, a small ML classifier. We do not
//! ship a model in a `cdylib`: it is large, its verdicts are not reproducible
//! across versions, and the load cost lands on every gateway boot. The
//! substitute is three signals combined in the same guess-then-try shape:
//!
//! 1. **Magic bytes** — decisive for containers, and the only signal that
//!    survives a lying filename.
//! 2. **Declared MIME** — caller-supplied, so trusted least, but it is the
//!    only signal that exists for a stream with no name.
//! 3. **Extension** — the best signal for text formats, where bytes say
//!    nothing.
//!
//! Conflicting signals produce several guesses rather than one verdict. That
//! is why markitdown copes well with mislabelled files, and it costs nothing
//! to keep.

use std::io::Cursor;

use crate::stream_info::{Probe, StreamInfo, normalize_ext};

/// The guess list plus anything worth telling the operator about how it was
/// reached.
#[derive(Debug, Clone, Default)]
pub struct Detection {
    /// Guesses in confidence order. Never empty — the last entry is always
    /// the caller's own view, so a converter keyed on a declared MIME we
    /// could not corroborate still gets its turn.
    pub guesses: Vec<StreamInfo>,
    /// Set when content and the declared type disagreed. Surfaced as a
    /// `TypeMismatch` warning; content wins, but the operator hears about it.
    pub mismatch: Option<String>,
    /// Which signal produced the leading guess. A span attribute.
    pub leading_signal: Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Signal {
    Content,
    Declared,
    Extension,
    #[default]
    None,
}

impl Signal {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Signal::Content => "content",
            Signal::Declared => "declared",
            Signal::Extension => "extension",
            Signal::None => "none",
        }
    }
}

/// Build the guess ladder for `bytes` given whatever the caller declared.
#[must_use]
pub fn detect(bytes: &[u8], base: &StreamInfo) -> Detection {
    let probe = Probe::new(bytes);
    let mut guesses: Vec<StreamInfo> = Vec::new();
    let mut mismatch = None;
    let mut leading_signal = Signal::None;

    // --- 1. content -------------------------------------------------------
    if let Some((mime, ext)) = sniff(bytes, &probe) {
        let mut si = base.clone();
        si.mimetype = Some(mime.to_owned());
        si.extension = Some(ext.to_owned());
        // Compare against whatever the caller implied — an explicit MIME, or
        // the one their filename implies. A caller who sends a PDF named
        // `invoice.docx` has declared nothing, but has still said something
        // wrong, and that is exactly the case worth reporting.
        let claimed = base.mimetype.clone().or_else(|| {
            base.extension
                .as_deref()
                .and_then(mime_for_extension)
                .map(str::to_owned)
        });
        if let Some(declared) = claimed
            && declared != mime
            && !compatible(&declared, mime)
        {
            mismatch = Some(format!("declared {declared}, content looks like {mime}"));
        }
        leading_signal = Signal::Content;
        guesses.push(si);
    }

    // --- 2. extension -----------------------------------------------------
    // Ahead of the bare declared type: for text formats the extension is the
    // only signal that distinguishes `.csv` from `.md`, and a caller that
    // declared `text/plain` for a CSV is the common case, not the exception.
    if let Some(ext) = &base.extension {
        let ext = normalize_ext(ext);
        if let Some(mime) = mime_for_extension(&ext) {
            let mut si = base.clone();
            si.mimetype = Some(mime.to_owned());
            si.extension = Some(ext);
            if leading_signal == Signal::None {
                leading_signal = Signal::Extension;
            }
            guesses.push(si);
        }
    }

    // --- 3. whatever the caller said -------------------------------------
    if base.mimetype.is_some() || base.extension.is_some() {
        if leading_signal == Signal::None {
            leading_signal = Signal::Declared;
        }
        guesses.push(base.clone());
    }

    // --- 4. textual fallback ---------------------------------------------
    // Something that decodes as text is worth one attempt as text even when
    // nothing named it. This is the analogue of markitdown's plain-text
    // converter sitting at generic priority.
    if probe.looks_textual() {
        let mut si = base.clone();
        if si.mimetype.is_none() {
            si.mimetype = Some("text/plain".to_owned());
        }
        if si.charset.is_none() {
            si.charset = Some(sniff_charset(bytes));
        }
        guesses.push(si);
    }

    // A conversion always gets at least one attempt.
    if guesses.is_empty() {
        guesses.push(base.clone());
    }

    dedupe(&mut guesses);
    Detection {
        guesses,
        mismatch,
        leading_signal,
    }
}

/// Magic-byte identification. `None` when the bytes carry no signature we
/// recognise — which is the normal case for every text format.
fn sniff(bytes: &[u8], probe: &Probe<'_>) -> Option<(&'static str, &'static str)> {
    if probe.starts_with(b"%PDF-") {
        return Some(("application/pdf", "pdf"));
    }
    // OLE compound file: .msg, and the legacy .doc/.xls/.ppt trio.
    if probe.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return Some(("application/vnd.ms-outlook", "msg"));
    }
    if probe.starts_with(b"PK\x03\x04") {
        return Some(sniff_zip(bytes));
    }
    // Everything with a stable signature that is not a container.
    if let Some(kind) = infer::get(probe.head()) {
        let mime = kind.mime_type();
        let ext = kind.extension();
        // Only report families we have a converter for; otherwise fall
        // through so the text and extension paths still get their turn.
        if mime.starts_with("image/") || mime.starts_with("audio/") || mime.starts_with("video/") {
            return Some((leak_mime(mime), leak_ext(ext)));
        }
    }
    None
}

/// Which OOXML family a zip holds, from the central directory rather than a
/// guess at the first local header — an OOXML writer is free to order entries
/// however it likes, and several do.
fn sniff_zip(bytes: &[u8]) -> (&'static str, &'static str) {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return ("application/zip", "zip");
    };
    let mut has_ooxml_marker = false;
    let mut epub = false;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index_raw(i) else {
            continue;
        };
        let name = entry.name().to_owned();
        if name == "[Content_Types].xml" {
            has_ooxml_marker = true;
            continue;
        }
        if name == "mimetype" || name.starts_with("META-INF/container.xml") {
            epub = true;
            continue;
        }
        if name.starts_with("word/") {
            return (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "docx",
            );
        }
        if name.starts_with("ppt/") {
            return (
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "pptx",
            );
        }
        if name.starts_with("xl/") {
            return (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "xlsx",
            );
        }
        if name.starts_with("OEBPS/") {
            epub = true;
        }
    }
    if epub {
        return ("application/epub+zip", "epub");
    }
    let _ = has_ooxml_marker;
    ("application/zip", "zip")
}

/// `infer` returns `&'static str` already; this indirection exists only to
/// keep the signature honest if that ever changes.
fn leak_mime(m: &'static str) -> &'static str {
    m
}
fn leak_ext(e: &'static str) -> &'static str {
    e
}

/// Extension → MIME for the formats we convert.
///
/// `mime_guess` covers most of these, but not `.ipynb` or `.msg`, and it
/// disagrees with us on a few (it maps `.md` to `text/markdown` only with the
/// right feature set). An explicit table is one place to look when a format
/// is not being picked up.
#[must_use]
pub fn mime_for_extension(ext: &str) -> Option<&'static str> {
    let m = match ext {
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" | "tab" => "text/tab-separated-values",
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "ipynb" => "application/x-ipynb+json",
        "xml" => "application/xml",
        "rss" => "application/rss+xml",
        "atom" => "application/atom+xml",
        "html" | "htm" | "xhtml" => "text/html",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsm" => "application/vnd.ms-excel.sheet.macroenabled.12",
        "xlsb" => "application/vnd.ms-excel.sheet.binary.macroenabled.12",
        "xls" => "application/vnd.ms-excel",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "epub" => "application/epub+zip",
        "zip" => "application/zip",
        "msg" => "application/vnd.ms-outlook",
        "eml" => "message/rfc822",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "heic" | "heif" => "image/heif",
        "avif" => "image/avif",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        _ => return mime_guess::from_ext(ext).first_raw(),
    };
    Some(m)
}

/// True when two MIME strings describe the same thing closely enough that
/// disagreement is not worth a warning.
fn compatible(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // A caller declaring the generic container type for an OOXML file, or
    // `application/octet-stream` for anything, is being unhelpful rather than
    // wrong.
    if a == "application/octet-stream" || b == "application/octet-stream" {
        return true;
    }
    let container = ["application/zip", "application/x-zip-compressed"];
    if container.contains(&a) && b.contains("officedocument") {
        return true;
    }
    if container.contains(&a) && b == "application/epub+zip" {
        return true;
    }
    // text/* callers labelling a more specific text format.
    a.starts_with("text/") && b.starts_with("text/")
}

/// Detect the character set of a text input. `chardetng` replaces
/// markitdown's `charset_normalizer`; a BOM outranks it.
#[must_use]
pub fn sniff_charset(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return "utf-8".to_owned();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return "utf-16le".to_owned();
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return "utf-16be".to_owned();
    }
    if std::str::from_utf8(bytes).is_ok() {
        return "utf-8".to_owned();
    }
    let mut det = chardetng::EncodingDetector::new();
    det.feed(bytes, true);
    det.guess(None, true).name().to_ascii_lowercase()
}

/// Remove guesses that repeat a (mimetype, extension) pair already seen,
/// keeping the earlier — higher-confidence — one.
fn dedupe(guesses: &mut Vec<StreamInfo>) {
    let mut seen: Vec<(Option<String>, Option<String>)> = Vec::new();
    guesses.retain(|g| {
        let key = (g.mimetype.clone(), g.extension.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_of(names: &[&str]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for n in names {
                w.start_file(*n, opts).unwrap();
                w.write_all(b"x").unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn pdf_magic_wins_over_a_lying_extension() {
        let si = StreamInfo::new().with_filename("invoice.docx");
        let d = detect(b"%PDF-1.7\n%stuff", &si);
        assert_eq!(d.guesses[0].mimetype.as_deref(), Some("application/pdf"));
        assert_eq!(d.leading_signal, Signal::Content);
        assert!(d.mismatch.is_some(), "should report the disagreement");
    }

    #[test]
    fn ooxml_family_comes_from_the_central_directory() {
        for (entry, want) in [
            ("word/document.xml", "docx"),
            ("ppt/presentation.xml", "pptx"),
            ("xl/workbook.xml", "xlsx"),
        ] {
            let bytes = zip_of(&["[Content_Types].xml", entry]);
            let d = detect(&bytes, &StreamInfo::new());
            assert_eq!(
                d.guesses[0].extension.as_deref(),
                Some(want),
                "entry {entry}"
            );
        }
    }

    #[test]
    fn ooxml_detection_survives_entry_reordering() {
        // The marker file last — a writer is free to do this and some do.
        let bytes = zip_of(&["word/document.xml", "[Content_Types].xml"]);
        let d = detect(&bytes, &StreamInfo::new());
        assert_eq!(d.guesses[0].extension.as_deref(), Some("docx"));
    }

    #[test]
    fn epub_is_distinguished_from_a_plain_zip() {
        let epub = zip_of(&["mimetype", "META-INF/container.xml", "OEBPS/ch1.xhtml"]);
        assert_eq!(
            detect(&epub, &StreamInfo::new()).guesses[0]
                .extension
                .as_deref(),
            Some("epub")
        );
        let plain = zip_of(&["a.txt", "b.txt"]);
        assert_eq!(
            detect(&plain, &StreamInfo::new()).guesses[0]
                .extension
                .as_deref(),
            Some("zip")
        );
    }

    #[test]
    fn zip_container_mislabel_is_not_a_mismatch() {
        let bytes = zip_of(&["[Content_Types].xml", "word/document.xml"]);
        let si = StreamInfo::new().with_mimetype("application/zip");
        assert!(detect(&bytes, &si).mismatch.is_none());
    }

    #[test]
    fn extension_outranks_a_generic_declared_type_for_text() {
        let si = StreamInfo::new()
            .with_mimetype("text/plain")
            .with_filename("data.csv");
        let d = detect(b"a,b\n1,2\n", &si);
        assert_eq!(d.guesses[0].mimetype.as_deref(), Some("text/csv"));
        // The caller's own view survives further down the ladder.
        assert!(d.guesses.iter().any(|g| g.is_mime("text/plain")));
    }

    #[test]
    fn unnamed_text_still_gets_one_attempt() {
        let d = detect(b"just some words\n", &StreamInfo::new());
        assert!(!d.guesses.is_empty());
        assert!(d.guesses.iter().any(|g| g.is_mime("text/plain")));
    }

    #[test]
    fn guesses_are_deduped() {
        let si = StreamInfo::new()
            .with_mimetype("text/csv")
            .with_filename("x.csv");
        let d = detect(b"a,b\n", &si);
        let csv_guesses = d.guesses.iter().filter(|g| g.is_mime("text/csv")).count();
        assert_eq!(csv_guesses, 1, "{:?}", d.guesses);
    }

    #[test]
    fn charset_prefers_a_bom_then_utf8() {
        assert_eq!(sniff_charset(b"\xEF\xBB\xBFhi"), "utf-8");
        assert_eq!(sniff_charset("héllo".as_bytes()), "utf-8");
        assert_eq!(sniff_charset(b"\xFF\xFEh\x00"), "utf-16le");
    }

    #[test]
    fn ole_container_is_recognised() {
        let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        bytes.extend_from_slice(&[0u8; 64]);
        let d = detect(&bytes, &StreamInfo::new());
        assert_eq!(d.guesses[0].extension.as_deref(), Some("msg"));
    }
}
