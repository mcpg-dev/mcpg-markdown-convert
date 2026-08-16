//! `StreamInfo` — the hint bundle threaded through detection and conversion,
//! and `Probe`, the read-only window a converter's `accepts()` sees.

/// What we believe about the input, before and after detection.
///
/// The same five fields markitdown threads through its converters. Every one
/// is a hint: any of them may be absent, and any of them may be wrong (they
/// are caller-supplied). Converters treat them as evidence, never as truth.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamInfo {
    /// Declared or detected MIME type, lower-cased, parameters stripped.
    pub mimetype: Option<String>,
    /// Lower-cased, no leading dot.
    pub extension: Option<String>,
    /// Character set for text formats.
    pub charset: Option<String>,
    pub filename: Option<String>,
    /// Where the bytes came from, when that is meaningful (a URL, a
    /// `mcpg-resource://` URI, a zip member path).
    pub url: Option<String>,
}

impl StreamInfo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_mimetype(mut self, m: impl Into<String>) -> Self {
        self.mimetype = Some(normalize_mime(&m.into()));
        self
    }

    #[must_use]
    pub fn with_extension(mut self, e: impl Into<String>) -> Self {
        self.extension = Some(normalize_ext(&e.into()));
        self
    }

    #[must_use]
    pub fn with_charset(mut self, c: impl Into<String>) -> Self {
        self.charset = Some(c.into().to_ascii_lowercase());
        self
    }

    /// Sets the filename and derives the extension from it when one is not
    /// already known.
    #[must_use]
    pub fn with_filename(mut self, f: impl Into<String>) -> Self {
        let f = f.into();
        if self.extension.is_none()
            && let Some((_, ext)) = f.rsplit_once('.')
            && !ext.is_empty()
        {
            self.extension = Some(normalize_ext(ext));
        }
        self.filename = Some(f);
        self
    }

    #[must_use]
    pub fn with_url(mut self, u: impl Into<String>) -> Self {
        self.url = Some(u.into());
        self
    }

    /// True when this guess names the given extension.
    #[must_use]
    pub fn is_ext(&self, ext: &str) -> bool {
        self.extension.as_deref() == Some(ext)
    }

    /// True when the mimetype equals `mime` or is one of `alts`. Callers pass
    /// the alternates because MIME registration for document formats is a
    /// mess: `.xls` alone answers to four different strings in the wild.
    #[must_use]
    pub fn is_mime(&self, mime: &str) -> bool {
        self.mimetype.as_deref() == Some(mime)
    }

    /// A short label for logs and the `Unsupported` error's guess list.
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.mimetype, &self.extension) {
            (Some(m), Some(e)) => format!("{m}/.{e}"),
            (Some(m), None) => m.clone(),
            (None, Some(e)) => format!(".{e}"),
            (None, None) => "unknown".to_owned(),
        }
    }
}

/// Lower-case, drop parameters (`; charset=utf-8`) and surrounding space.
#[must_use]
pub fn normalize_mime(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_ascii_lowercase()
}

/// Lower-case, drop a leading dot.
#[must_use]
pub fn normalize_ext(raw: &str) -> String {
    raw.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// The read-only view a converter's `accepts()` gets.
///
/// markitdown's base class *documents* that `accepts()` must restore the
/// stream position if it reads. A comment cannot enforce that, and forgetting
/// it corrupts the next converter's attempt. Here `accepts()` is simply not
/// given a cursor: it sees a fixed prefix of the bytes and cannot move
/// anything.
#[derive(Debug, Clone, Copy)]
pub struct Probe<'a> {
    head: &'a [u8],
    total_len: usize,
}

/// How many leading bytes a converter may inspect. Enough for every magic
/// number we check plus the OOXML central-directory hint, and small enough
/// that holding it costs nothing.
pub const PROBE_BYTES: usize = 8192;

impl<'a> Probe<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            head: &bytes[..bytes.len().min(PROBE_BYTES)],
            total_len: bytes.len(),
        }
    }

    /// The leading bytes. Never longer than [`PROBE_BYTES`].
    #[must_use]
    pub fn head(&self) -> &'a [u8] {
        self.head
    }

    /// Full length of the input, which may exceed the probe window.
    #[must_use]
    pub fn len(&self) -> usize {
        self.total_len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    #[must_use]
    pub fn starts_with(&self, magic: &[u8]) -> bool {
        self.head.starts_with(magic)
    }

    /// True when the probe window contains `needle`. Bounded by the window,
    /// so a match is evidence and a miss is not proof of absence.
    #[must_use]
    pub fn contains(&self, needle: &[u8]) -> bool {
        if needle.is_empty() || needle.len() > self.head.len() {
            return false;
        }
        self.head.windows(needle.len()).any(|w| w == needle)
    }

    /// True when the window decodes as UTF-8 and looks like text — no NUL
    /// bytes and few controls. The cheap discriminator between "a text format
    /// we should try to parse" and "binary we should not".
    #[must_use]
    pub fn looks_textual(&self) -> bool {
        if self.head.is_empty() {
            return true;
        }
        if self.head.contains(&0) {
            return false;
        }
        let controls = self
            .head
            .iter()
            .filter(|b| **b < 0x09 || (**b > 0x0d && **b < 0x20))
            .count();
        controls * 100 < self.head.len()
    }

    /// The leading non-space characters, lossily decoded. For sniffing `<?xml`,
    /// `{`, `<!doctype html`, and similar.
    #[must_use]
    pub fn leading_text(&self, max: usize) -> String {
        String::from_utf8_lossy(&self.head[..self.head.len().min(max)])
            .trim_start()
            .to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_derives_extension() {
        let si = StreamInfo::new().with_filename("Report.FINAL.DocX");
        assert_eq!(si.extension.as_deref(), Some("docx"));
        assert_eq!(si.filename.as_deref(), Some("Report.FINAL.DocX"));
    }

    #[test]
    fn explicit_extension_survives_filename() {
        let si = StreamInfo::new()
            .with_extension("csv")
            .with_filename("data.bin");
        assert_eq!(si.extension.as_deref(), Some("csv"));
    }

    #[test]
    fn mime_normalisation_drops_parameters() {
        assert_eq!(normalize_mime("Text/HTML; charset=UTF-8"), "text/html");
    }

    #[test]
    fn probe_window_is_bounded() {
        let big = vec![b'a'; PROBE_BYTES * 3];
        let p = Probe::new(&big);
        assert_eq!(p.head().len(), PROBE_BYTES);
        assert_eq!(p.len(), PROBE_BYTES * 3);
    }

    #[test]
    fn probe_detects_binary() {
        assert!(!Probe::new(b"PK\x03\x04\x00\x00").looks_textual());
        assert!(Probe::new(b"hello, world\n").looks_textual());
        assert!(Probe::new(b"").looks_textual());
    }

    #[test]
    fn probe_contains_is_window_bounded() {
        let p = Probe::new(b"abcdef");
        assert!(p.contains(b"cde"));
        assert!(!p.contains(b"xyz"));
        assert!(!p.contains(b""));
    }
}
