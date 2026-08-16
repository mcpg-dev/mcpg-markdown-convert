//! The converter trait and the priority-ordered registry.

use std::sync::Arc;

use crate::cx::ConvertCx;
use crate::error::ConvertError;
use crate::ir::Document;
use crate::stream_info::{Probe, StreamInfo};

/// Format-specific converters: `.docx`, `.pdf`, `.xlsx`. Tried first.
pub const PRIORITY_SPECIFIC: i32 = 0;
/// Near-catch-alls: plain text, HTML, zip. Tried last, so a `.docx` is never
/// eaten by the zip converter that would also accept it.
pub const PRIORITY_GENERIC: i32 = 100;

/// One format.
///
/// The `accepts` / `convert` pair mirrors markitdown's `DocumentConverter`,
/// with one contract tightened: `accepts` receives a [`Probe`] rather than a
/// seekable stream, so it cannot disturb the cursor for the converter tried
/// next. markitdown can only document that rule; here it is unrepresentable
/// to break it.
pub trait Converter: Send + Sync {
    /// Stable short name. Used as a metric label and in error text, so it
    /// must not vary at runtime.
    fn name(&self) -> &'static str;

    fn priority(&self) -> i32 {
        PRIORITY_SPECIFIC
    }

    /// Can this converter handle the input? Cheap checks only — magic bytes,
    /// extension, MIME. Anything expensive belongs in `convert`.
    ///
    /// Returning `true` is a promise that `convert` will at least try; a
    /// converter that accepts everything and then fails starves the ones
    /// behind it.
    fn accepts(&self, probe: &Probe<'_>, info: &StreamInfo) -> bool;

    /// Parse the whole input into the IR.
    ///
    /// `bytes` is the complete input: like markitdown, we materialise the
    /// stream up front, because guess-then-try needs to re-read it and half
    /// the formats here are zip containers that need random access anyway.
    fn convert(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Document, ConvertError>;
}

/// The ordered converter set.
///
/// Ordering rule, taken from markitdown: ascending priority, and within one
/// priority the most recently registered converter wins. That second half is
/// what lets a profile-local override beat a built-in without renumbering
/// anything.
#[derive(Clone, Default)]
pub struct ConverterRegistry {
    entries: Vec<Entry>,
}

#[derive(Clone)]
struct Entry {
    converter: Arc<dyn Converter>,
    priority: i32,
    /// Registration sequence, used to break priority ties in reverse.
    seq: usize,
}

impl std::fmt::Debug for ConverterRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConverterRegistry")
            .field("converters", &self.names())
            .finish()
    }
}

impl ConverterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a converter at its own declared priority.
    pub fn register(&mut self, converter: Arc<dyn Converter>) {
        let priority = converter.priority();
        self.register_at(converter, priority);
    }

    /// Add a converter at an explicit priority, overriding its default.
    pub fn register_at(&mut self, converter: Arc<dyn Converter>, priority: i32) {
        let seq = self.entries.len();
        self.entries.push(Entry {
            converter,
            priority,
            seq,
        });
        // Ascending priority; later registrations first within a tier.
        self.entries
            .sort_by(|a, b| a.priority.cmp(&b.priority).then(b.seq.cmp(&a.seq)));
    }

    /// Drop every converter whose name is not in `keep`. This is how the
    /// operator's `formats.enable` allowlist is applied: an explicit list,
    /// never a wildcard, so a new format arriving in a new plugin version is
    /// an operator decision rather than a surprise.
    pub fn retain_names(&mut self, keep: &[String]) {
        self.entries
            .retain(|e| keep.iter().any(|k| k == e.converter.name()));
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.entries.iter().map(|e| e.converter.name()).collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Converters in try order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Converter>> {
        self.entries.iter().map(|e| &e.converter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str, i32);

    impl Converter for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn priority(&self) -> i32 {
            self.1
        }
        fn accepts(&self, _p: &Probe<'_>, _i: &StreamInfo) -> bool {
            true
        }
        fn convert(
            &self,
            _b: &[u8],
            _i: &StreamInfo,
            _c: &ConvertCx<'_>,
        ) -> Result<Document, ConvertError> {
            Ok(Document::new())
        }
    }

    fn reg(specs: &[(&'static str, i32)]) -> ConverterRegistry {
        let mut r = ConverterRegistry::new();
        for (n, p) in specs {
            r.register(Arc::new(Stub(n, *p)));
        }
        r
    }

    #[test]
    fn specific_beats_generic() {
        let r = reg(&[("zip", PRIORITY_GENERIC), ("docx", PRIORITY_SPECIFIC)]);
        assert_eq!(r.names(), vec!["docx", "zip"]);
    }

    #[test]
    fn later_registration_wins_within_a_tier() {
        let r = reg(&[
            ("builtin", PRIORITY_SPECIFIC),
            ("override", PRIORITY_SPECIFIC),
        ]);
        assert_eq!(r.names(), vec!["override", "builtin"]);
    }

    #[test]
    fn retain_applies_an_allowlist() {
        let mut r = reg(&[("a", 0), ("b", 0), ("c", 0)]);
        r.retain_names(&["a".to_owned(), "c".to_owned()]);
        let mut names = r.names();
        names.sort_unstable();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn explicit_priority_overrides_the_declared_one() {
        let mut r = ConverterRegistry::new();
        r.register(Arc::new(Stub("generic", PRIORITY_GENERIC)));
        r.register_at(Arc::new(Stub("promoted", PRIORITY_GENERIC)), -1);
        assert_eq!(r.names(), vec!["promoted", "generic"]);
    }
}
