//! Conversion errors and the non-fatal warning channel.

use serde::{Deserialize, Serialize};

/// Why a conversion failed. Every variant carries enough detail for an
/// operator to act without re-running with debug logging.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// No registered converter accepted the input. Carries the guess list so
    /// the operator sees what was tried rather than a bare "unsupported".
    #[error("unsupported format (tried: {tried})")]
    Unsupported { tried: String },

    /// A converter accepted the input and then failed to parse it.
    #[error("{format}: malformed input — {message}")]
    Malformed {
        format: &'static str,
        message: String,
    },

    /// Encrypted or password-protected. Distinct from `Malformed` because the
    /// operator remedy is completely different.
    #[error("{format}: input is encrypted or password-protected")]
    Encrypted { format: &'static str },

    /// A budget was exhausted. Conversion stops rather than truncating when
    /// the trip happens before any output exists.
    #[error("limit exceeded: {limit} ({actual} > {allowed})")]
    LimitExceeded {
        limit: &'static str,
        actual: u64,
        allowed: u64,
    },

    /// A third-party parser panicked and the guard caught it. Never expected;
    /// always worth an alert.
    #[error("{format}: converter panicked")]
    ConverterPanic { format: &'static str },

    /// A template failed to compile or render.
    #[error("template {stage}: {message}")]
    Template {
        stage: &'static str,
        message: String,
    },

    /// Input was not valid for the declared source (bad base64, absent
    /// pointer, empty body).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl ConvertError {
    /// Short stable code for metric labels and structured errors. Kept
    /// separate from `Display` so log text can change without breaking a
    /// dashboard.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ConvertError::Unsupported { .. } => "unsupported",
            ConvertError::Malformed { .. } => "malformed",
            ConvertError::Encrypted { .. } => "encrypted",
            ConvertError::LimitExceeded { .. } => "limit_exceeded",
            ConvertError::ConverterPanic { .. } => "panic",
            ConvertError::Template { .. } => "template",
            ConvertError::InvalidInput(_) => "invalid_input",
        }
    }
}

/// A degradation that did not stop the conversion.
///
/// markitdown degrades silently — a missing optional dependency means a
/// converter is simply never registered, and the caller gets a worse result
/// from some other converter with no signal. Every degradation here is
/// recorded on the document, surfaced in the tool result, and counted in
/// `mcpg_markdown_warnings_total`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    /// Stable machine-readable kind. Doubles as a metric label.
    pub kind: WarningKind,
    /// Human-readable detail, safe to show an operator.
    pub message: String,
}

impl Warning {
    pub fn new(kind: WarningKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// The closed set of degradation kinds. Closed on purpose: an open string
/// would produce unbounded metric cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// Output or expansion hit a byte ceiling and was cut at a safe boundary.
    Truncated,
    /// A container member was skipped (encrypted, unreadable, over depth).
    SkippedMember,
    /// The format was handled, but by a path that loses information.
    Degraded,
    /// A structural guess fired with low confidence (PDF headings, columns).
    HeuristicApplied,
    /// A page or part carried no extractable text (scanned PDF, empty slide).
    NoTextLayer,
    /// LLM enrichment was requested and did not complete.
    EnrichmentFailed,
    /// Detection disagreed with the declared type, and we went with content.
    TypeMismatch,
}

impl WarningKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            WarningKind::Truncated => "truncated",
            WarningKind::SkippedMember => "skipped_member",
            WarningKind::Degraded => "degraded",
            WarningKind::HeuristicApplied => "heuristic_applied",
            WarningKind::NoTextLayer => "no_text_layer",
            WarningKind::EnrichmentFailed => "enrichment_failed",
            WarningKind::TypeMismatch => "type_mismatch",
        }
    }
}
