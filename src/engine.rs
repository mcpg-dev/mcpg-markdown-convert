//! The engine: detection ladder → converter registry → document → Markdown.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use crate::config::ConvertOptions;
use crate::cx::{Budget, ConvertCx};
use crate::detect::{Detection, detect};
use crate::error::{ConvertError, Warning, WarningKind};
use crate::ir::Document;
use crate::registry::{Converter, ConverterRegistry};
use crate::render::{Rendered, render};
use crate::stream_info::{Probe, StreamInfo};
use crate::template;

/// A conversion that produced output, plus everything worth reporting about
/// how it got there.
#[derive(Debug, Clone)]
pub struct Conversion {
    pub markdown: String,
    pub document: Document,
    /// Name of the converter that ran. A metric label and a span attribute.
    pub converter: &'static str,
    /// Which detection signal chose the guess that won.
    pub detected_via: &'static str,
    /// Every degradation, from the converter and from the renderer.
    pub warnings: Vec<Warning>,
}

impl Conversion {
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.document.title.as_deref()
    }
}

/// A configured conversion engine. Built once per profile at
/// `register_profile()` — the registry, the compiled templates and the format
/// tables are all reusable, and rebuilding them per call would be the single
/// largest avoidable cost on the request path.
pub struct Engine {
    registry: ConverterRegistry,
    options: ConvertOptions,
    templates: Option<template::Templates>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("converters", &self.registry.names())
            .field("templated", &self.templates.is_some())
            .finish()
    }
}

impl Engine {
    /// Build an engine from operator options.
    ///
    /// Fails on a template that does not compile or a `formats.enable` entry
    /// that names no converter — both at boot, never on the first call.
    pub fn new(options: ConvertOptions) -> Result<Self, ConvertError> {
        let mut registry = crate::converters::builtin_registry();

        if let Some(enabled) = &options.formats.enable {
            let known: Vec<&str> = registry.names();
            for want in enabled {
                if !known.iter().any(|k| k == want) {
                    return Err(ConvertError::InvalidInput(format!(
                        "formats.enable names {want:?}, which is not a converter in this build \
                         (available: {})",
                        known.join(", ")
                    )));
                }
            }
            registry.retain_names(enabled);
            if registry.is_empty() {
                return Err(ConvertError::InvalidInput(
                    "formats.enable left no converters registered".to_owned(),
                ));
            }
        }

        let templates = match &options.templates {
            Some(spec) => Some(template::Templates::compile(spec)?),
            None => None,
        };

        Ok(Self {
            registry,
            options,
            templates,
        })
    }

    /// Add a converter ahead of the built-ins at the same priority. Used by
    /// tests and by any future profile-local override.
    pub fn register(&mut self, converter: Arc<dyn Converter>) {
        self.registry.register(converter);
    }

    #[must_use]
    pub fn options(&self) -> &ConvertOptions {
        &self.options
    }

    #[must_use]
    pub fn converter_names(&self) -> Vec<&'static str> {
        self.registry.names()
    }

    /// Convert bytes to Markdown.
    pub fn convert(&self, bytes: &[u8], info: &StreamInfo) -> Result<Conversion, ConvertError> {
        let budget = Budget::new(self.options.limits.clone());
        budget.check_input_size(bytes.len() as u64)?;
        let cx = ConvertCx::new(&budget);
        self.convert_with(bytes, info, &cx)
    }

    /// Convert within an existing budget. The embedded-document path uses
    /// this so a nested archive spends the same allowance as its parent.
    pub fn convert_with(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<Conversion, ConvertError> {
        let detection = detect(bytes, info);
        let (mut document, converter) = self.run_converters(bytes, &detection, cx)?;

        if let Some(m) = &detection.mismatch {
            document.warn(Warning::new(WarningKind::TypeMismatch, m.clone()));
        }

        let rendered = self.render_document_with(
            &document,
            template::RenderExtras {
                source: Some(info),
                now: None,
            },
        )?;
        let mut warnings = document.warnings.clone();
        warnings.extend(rendered.warnings);

        Ok(Conversion {
            markdown: rendered.markdown,
            document,
            converter,
            detected_via: detection.leading_signal.as_str(),
            warnings,
        })
    }

    /// Convert bytes to the IR only, skipping the render. The enrichment pass
    /// runs between the two, so the plugin needs the halves separately.
    pub fn convert_to_ir(
        &self,
        bytes: &[u8],
        info: &StreamInfo,
        cx: &ConvertCx<'_>,
    ) -> Result<(Document, &'static str, &'static str), ConvertError> {
        let detection = detect(bytes, info);
        let (mut document, converter) = self.run_converters(bytes, &detection, cx)?;
        if let Some(m) = &detection.mismatch {
            document.warn(Warning::new(WarningKind::TypeMismatch, m.clone()));
        }
        Ok((document, converter, detection.leading_signal.as_str()))
    }

    /// Render an IR the caller may have modified (see `convert_to_ir`).
    pub fn render_document(&self, doc: &Document) -> Result<Rendered, ConvertError> {
        self.render_document_with(doc, template::RenderExtras::default())
    }

    /// Render with extra template context. `now` arrives from the caller
    /// rather than a clock read here, which is what keeps this crate pure and
    /// the golden-corpus output reproducible.
    pub fn render_document_with(
        &self,
        doc: &Document,
        extras: template::RenderExtras<'_>,
    ) -> Result<Rendered, ConvertError> {
        match &self.templates {
            Some(t) => t.render(doc, &self.options.output, &self.options.limits, extras),
            None => Ok(render(doc, &self.options.output, &self.options.limits)),
        }
    }

    /// The guess-then-try loop.
    ///
    /// Guesses outer, converters inner — the order markitdown uses, and the
    /// reason a mislabelled file still converts: a converter that would have
    /// rejected the caller's declared type gets another look under the
    /// content-derived guess.
    fn run_converters(
        &self,
        bytes: &[u8],
        detection: &Detection,
        cx: &ConvertCx<'_>,
    ) -> Result<(Document, &'static str), ConvertError> {
        let probe = Probe::new(bytes);
        let mut last_error: Option<ConvertError> = None;

        for guess in &detection.guesses {
            for conv in self.registry.iter() {
                if !conv.accepts(&probe, guess) {
                    continue;
                }
                cx.budget().check_deadline()?;
                match guarded_convert(conv.as_ref(), bytes, guess, cx) {
                    Ok(doc) => return Ok((doc, conv.name())),
                    // A budget trip is terminal: trying another converter
                    // would spend more of an allowance that is already gone.
                    Err(e @ ConvertError::LimitExceeded { .. }) => return Err(e),
                    Err(e) => last_error = Some(e),
                }
            }
        }

        Err(match last_error {
            // A converter accepted and failed: that error is far more useful
            // than "unsupported", which would be actively misleading.
            Some(e) => e,
            None => ConvertError::Unsupported {
                tried: detection
                    .guesses
                    .iter()
                    .map(StreamInfo::label)
                    .collect::<Vec<_>>()
                    .join(", "),
            },
        })
    }
}

/// Run one converter with a panic guard.
///
/// The SDK already catches panics at the FFI boundary, which keeps a bad file
/// from aborting the process. This inner guard is finer-grained: it keeps a
/// bad file from failing the *request*, because the loop can fall through to
/// the next converter. Document parsers are the classic fuzz target and this
/// is the cheapest possible insurance.
fn guarded_convert(
    conv: &dyn Converter,
    bytes: &[u8],
    info: &StreamInfo,
    cx: &ConvertCx<'_>,
) -> Result<Document, ConvertError> {
    let name = conv.name();
    match std::panic::catch_unwind(AssertUnwindSafe(|| conv.convert(bytes, info, cx))) {
        Ok(r) => r,
        Err(_) => Err(ConvertError::ConverterPanic { format: name }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConvertOptions, FormatSelection};
    use crate::cx::Limits;
    use crate::ir::{Block, Inline};
    use crate::registry::{PRIORITY_GENERIC, PRIORITY_SPECIFIC};

    fn engine() -> Engine {
        Engine::new(ConvertOptions::default()).expect("default options build")
    }

    struct Panicky;
    impl Converter for Panicky {
        fn name(&self) -> &'static str {
            "panicky"
        }
        fn priority(&self) -> i32 {
            PRIORITY_SPECIFIC - 10
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
            panic!("simulated parser explosion");
        }
    }

    struct Always(&'static str);
    impl Converter for Always {
        fn name(&self) -> &'static str {
            self.0
        }
        fn priority(&self) -> i32 {
            PRIORITY_GENERIC + 10
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
            let mut d = Document::new();
            d.push(Block::Paragraph(Inline::text("rescued")));
            Ok(d)
        }
    }

    #[test]
    fn a_panicking_converter_does_not_take_the_request_down() {
        let mut e = engine();
        e.register(Arc::new(Panicky));
        e.register(Arc::new(Always("rescue")));
        let out = e
            .convert(b"whatever", &StreamInfo::new())
            .expect("falls through to the next converter");
        assert!(out.markdown.contains("rescued"));
    }

    #[test]
    fn a_panic_with_nothing_behind_it_surfaces_as_an_error() {
        let mut e = Engine::new(ConvertOptions {
            formats: FormatSelection {
                enable: Some(vec!["text".to_owned()]),
            },
            ..ConvertOptions::default()
        })
        .unwrap();
        e.register(Arc::new(Panicky));
        // Binary input, so the text converter declines and only Panicky runs.
        let err = e.convert(&[0u8, 1, 2, 3], &StreamInfo::new()).unwrap_err();
        assert_eq!(err.code(), "panic");
    }

    #[test]
    fn unknown_format_names_what_was_tried() {
        // `csv` is in every build, so this assertion does not change meaning
        // with the feature set.
        let e = Engine::new(ConvertOptions {
            formats: FormatSelection {
                enable: Some(vec!["csv".to_owned()]),
            },
            ..ConvertOptions::default()
        })
        .unwrap();
        let err = e
            .convert(&[0xFFu8, 0xD8, 0x00, 0x01], &StreamInfo::new())
            .unwrap_err();
        assert_eq!(err.code(), "unsupported");
        assert!(format!("{err}").contains("tried:"), "{err}");
    }

    #[test]
    fn an_unknown_format_name_fails_at_build_time() {
        let err = Engine::new(ConvertOptions {
            formats: FormatSelection {
                enable: Some(vec!["nonesuch".to_owned()]),
            },
            ..ConvertOptions::default()
        })
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert!(format!("{err}").contains("nonesuch"));
    }

    #[test]
    fn oversized_input_is_refused_before_conversion() {
        let e = Engine::new(ConvertOptions {
            limits: Limits {
                max_input_bytes: 8,
                ..Limits::default()
            },
            ..ConvertOptions::default()
        })
        .unwrap();
        let err = e.convert(&[b'a'; 64], &StreamInfo::new()).unwrap_err();
        assert_eq!(err.code(), "limit_exceeded");
    }

    #[test]
    fn a_type_mismatch_reaches_the_caller_as_a_warning() {
        let e = engine();
        let si = StreamInfo::new().with_filename("notes.txt");
        let out = e.convert(b"%PDF-1.4 broken", &si);
        // Either the PDF converter refuses the truncated file or it succeeds
        // with a warning; both are acceptable. What must not happen is a
        // silent conversion under the wrong type.
        if let Ok(c) = out {
            assert!(
                c.warnings
                    .iter()
                    .any(|w| w.kind == WarningKind::TypeMismatch),
                "{:?}",
                c.warnings
            );
        }
    }
}
