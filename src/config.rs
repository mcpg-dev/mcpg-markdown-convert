//! Operator-facing engine options.
//!
//! These are the parts of a profile that the engine itself understands. The
//! plugin adds the parts that need a host — source modes, LLM enrichment —
//! and keeps them out of here so this crate stays pure compute.

use serde::{Deserialize, Serialize};

use crate::cx::Limits;
use crate::render::RenderOptions;

/// Everything the engine needs to be built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertOptions {
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub output: RenderOptions,
    #[serde(default)]
    pub formats: FormatSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates: Option<TemplateSpec>,
}

/// Which converters are live at runtime.
///
/// Separate from the crate's cargo features on purpose. Features decide what
/// is *compiled in*; this decides what an operator has *turned on*. It is an
/// explicit allowlist rather than a wildcard so that a format added in a new
/// plugin version arrives as an operator decision, not as a side effect of an
/// upgrade.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatSelection {
    /// Converter names to keep. `None` means every converter in the build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable: Option<Vec<String>>,
}

/// Operator templates. Both halves are optional: a profile can override just
/// one block type and inherit the default renderer for everything else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateSpec {
    /// Whole-document template. Receives `doc`, `source` and `now`, and can
    /// call `render(blocks)` to defer to the default renderer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// Per-block-type overrides, keyed by the block's snake_case type name
    /// (`heading`, `paragraph`, `table`, `code`, `image`, `list`, `quote`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub blocks: std::collections::BTreeMap<String, String>,
}

impl TemplateSpec {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.document.is_none() && self.blocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_round_trip_through_json() {
        let opts = ConvertOptions::default();
        let json = serde_json::to_value(&opts).unwrap();
        let back: ConvertOptions = serde_json::from_value(json).unwrap();
        assert_eq!(opts, back);
    }

    #[test]
    fn an_empty_object_yields_the_defaults() {
        let opts: ConvertOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(opts, ConvertOptions::default());
        assert_eq!(opts.limits.max_depth, 3);
    }

    #[test]
    fn a_typo_is_rejected_rather_than_silently_defaulted() {
        // deny_unknown_fields is the difference between "your limit is
        // ignored" and "your config is wrong". Skipping it here is a known
        // trap in this tree.
        let err = serde_json::from_str::<ConvertOptions>(r#"{"limit": {}}"#).unwrap_err();
        assert!(format!("{err}").contains("unknown field"), "{err}");

        let err = serde_json::from_str::<Limits>(r#"{"max_input_byte": 5}"#).unwrap_err();
        assert!(format!("{err}").contains("unknown field"), "{err}");
    }

    #[test]
    fn partial_limits_keep_the_other_defaults() {
        let l: Limits = serde_json::from_str(r#"{"max_depth": 9}"#).unwrap();
        assert_eq!(l.max_depth, 9);
        assert_eq!(l.max_input_bytes, Limits::default().max_input_bytes);
    }
}
