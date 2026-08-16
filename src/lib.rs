//! # `mcpg-markdown-convert`
//!
//! Any document → Markdown, for LLM consumption.
//!
//! The engine behind the `dev.mcpg.backend.markdown` plugin. Given bytes and
//! whatever the caller believes about them, it picks a converter, parses the
//! document into a small IR, and renders CommonMark + GFM — optionally
//! through operator-supplied MiniJinja templates.
//!
//! ## What this crate does not do
//!
//! No filesystem access. No network. No host handle. No clock. Everything
//! that touches the outside world — fetching the bytes, calling a model to
//! caption an image, stamping a timestamp into front matter — belongs to the
//! plugin that wraps this crate. That boundary is what lets every converter
//! be unit-tested without a gateway, and it is why an image reference inside
//! a document is rendered as a link and never followed.
//!
//! ## Shape
//!
//! ```text
//! bytes + StreamInfo
//!   → detect()          prioritised guesses: magic bytes, extension, declared
//!   → ConverterRegistry priority-ordered, accepts() then convert()
//!   → Document          the IR: blocks, metadata, warnings
//!   → render()          CommonMark + GFM, or a template
//! ```
//!
//! ## Example
//!
//! ```
//! use mcpg_markdown_convert::{ConvertOptions, Engine, StreamInfo};
//!
//! let engine = Engine::new(ConvertOptions::default()).unwrap();
//! let info = StreamInfo::new().with_filename("data.csv");
//! let out = engine.convert(b"name,age\nada,36\n", &info).unwrap();
//!
//! assert_eq!(out.converter, "csv");
//! assert!(out.markdown.contains("| name | age |"));
//! ```
//!
//! ## Degradation is never silent
//!
//! A conversion that loses something says so. Truncation, a skipped archive
//! member, a PDF page with no text layer, a low-confidence structural guess —
//! each lands in [`Document::warnings`] and reaches the caller. markitdown
//! degrades quietly (an absent optional dependency simply unregisters a
//! converter), and finding out by diffing output is not a reasonable ask.

#![forbid(unsafe_code)]

pub mod config;
pub mod converters;
pub mod cx;
pub mod detect;
pub mod engine;
pub mod error;
pub mod ir;
pub mod registry;
pub mod render;
pub mod stream_info;
pub mod template;

pub use config::{ConvertOptions, FormatSelection, TemplateSpec};
pub use cx::{Budget, ConvertCx, Limits};
pub use detect::{Detection, Signal, detect};
pub use engine::{Conversion, Engine};
pub use error::{ConvertError, Warning, WarningKind};
pub use ir::{Block, Document, Image, ImageRef, Inline, Metadata, Span, Table};
pub use registry::{Converter, ConverterRegistry, PRIORITY_GENERIC, PRIORITY_SPECIFIC};
pub use render::{FrontMatter, RenderOptions, Rendered, TableStyle};
pub use stream_info::{Probe, StreamInfo};
pub use template::{RenderExtras, Templates};

/// Names of every converter compiled into this build, for the plugin's
/// `formats.enable` validation and for the tool description.
#[must_use]
pub fn available_formats() -> Vec<&'static str> {
    converters::builtin_registry().names()
}

#[cfg(test)]
mod corpus;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;
