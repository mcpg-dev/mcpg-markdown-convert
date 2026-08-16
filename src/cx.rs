//! Conversion limits and the per-call budget.
//!
//! A `cdylib` plugin shares the gateway's address space, so an out-of-memory
//! or an abort here is a gateway outage rather than a failed request. Every
//! unbounded loop in a converter — archive members, spreadsheet rows, PDF
//! pages — is metered against one [`Budget`], and the budget is checked
//! between units of work rather than after them.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::ConvertError;

/// Operator-tunable ceilings. Defaults are sized for a document a person
/// would plausibly hand to a model, not for the largest file that exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Largest accepted input. Checked before allocation wherever the length
    /// is known up front.
    #[serde(default = "d_max_input")]
    pub max_input_bytes: u64,
    /// Largest rendered Markdown. Overflow truncates at a block boundary.
    #[serde(default = "d_max_output")]
    pub max_output_bytes: u64,
    /// Total bytes decompressed across every container. The zip-bomb ceiling:
    /// a 42-byte archive that expands to 4 GB trips this, not `max_input`.
    #[serde(default = "d_max_expanded")]
    pub max_expanded_bytes: u64,
    /// Nesting depth for archives, attachments and embedded documents.
    #[serde(default = "d_max_depth")]
    pub max_depth: u32,
    /// Total embedded documents across the whole conversion.
    #[serde(default = "d_max_embedded")]
    pub max_embedded_documents: u32,
    /// Wall-clock ceiling for one conversion.
    #[serde(default = "d_timeout_ms")]
    pub timeout_ms: u64,
    /// Rows rendered per table. A 500k-row sheet is not a Markdown table.
    #[serde(default = "d_max_table_rows")]
    pub max_table_rows: u32,
}

fn d_max_input() -> u64 {
    20 * 1024 * 1024
}
fn d_max_output() -> u64 {
    4 * 1024 * 1024
}
fn d_max_expanded() -> u64 {
    200 * 1024 * 1024
}
fn d_max_depth() -> u32 {
    3
}
fn d_max_embedded() -> u32 {
    64
}
fn d_timeout_ms() -> u64 {
    30_000
}
fn d_max_table_rows() -> u32 {
    5_000
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: d_max_input(),
            max_output_bytes: d_max_output(),
            max_expanded_bytes: d_max_expanded(),
            max_depth: d_max_depth(),
            max_embedded_documents: d_max_embedded(),
            timeout_ms: d_timeout_ms(),
            max_table_rows: d_max_table_rows(),
        }
    }
}

/// Live counters for one conversion. Shared by reference down the whole
/// converter tree, so a nested archive spends the same allowance as its
/// parent — this is what stops depth-bounded nesting from multiplying into an
/// unbounded total.
#[derive(Debug)]
pub struct Budget {
    limits: Limits,
    expanded: AtomicU64,
    embedded: AtomicUsize,
    started: Instant,
}

impl Budget {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            expanded: AtomicU64::new(0),
            embedded: AtomicUsize::new(0),
            started: Instant::now(),
        }
    }

    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Charge decompressed bytes. `Err` when the ceiling is crossed; callers
    /// that can degrade gracefully (skip this member, keep the document)
    /// should catch it rather than propagate.
    pub fn charge_expanded(&self, bytes: u64) -> Result<(), ConvertError> {
        let total = self.expanded.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if total > self.limits.max_expanded_bytes {
            return Err(ConvertError::LimitExceeded {
                limit: "max_expanded_bytes",
                actual: total,
                allowed: self.limits.max_expanded_bytes,
            });
        }
        Ok(())
    }

    /// Reserve one embedded-document slot.
    pub fn charge_embedded(&self) -> Result<(), ConvertError> {
        let n = self.embedded.fetch_add(1, Ordering::Relaxed) + 1;
        if n as u64 > u64::from(self.limits.max_embedded_documents) {
            return Err(ConvertError::LimitExceeded {
                limit: "max_embedded_documents",
                actual: n as u64,
                allowed: u64::from(self.limits.max_embedded_documents),
            });
        }
        Ok(())
    }

    /// Check the wall clock. Called between units of work — per archive
    /// member, per PDF page, per sheet — never inside a tight inner loop.
    pub fn check_deadline(&self) -> Result<(), ConvertError> {
        let elapsed = self.started.elapsed();
        if elapsed > Duration::from_millis(self.limits.timeout_ms) {
            return Err(ConvertError::LimitExceeded {
                limit: "timeout_ms",
                actual: elapsed.as_millis() as u64,
                allowed: self.limits.timeout_ms,
            });
        }
        Ok(())
    }

    pub fn check_input_size(&self, bytes: u64) -> Result<(), ConvertError> {
        if bytes > self.limits.max_input_bytes {
            return Err(ConvertError::LimitExceeded {
                limit: "max_input_bytes",
                actual: bytes,
                allowed: self.limits.max_input_bytes,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn expanded_bytes(&self) -> u64 {
        self.expanded.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Per-call context handed to every converter.
///
/// Holds the shared [`Budget`] and the depth of this particular descent.
/// Cloning is cheap and only changes the depth, which is what
/// [`descend`](ConvertCx::descend) does.
pub struct ConvertCx<'a> {
    budget: &'a Budget,
    depth: u32,
}

impl<'a> ConvertCx<'a> {
    #[must_use]
    pub fn new(budget: &'a Budget) -> Self {
        Self { budget, depth: 0 }
    }

    #[must_use]
    pub fn budget(&self) -> &'a Budget {
        self.budget
    }

    #[must_use]
    pub fn limits(&self) -> &Limits {
        self.budget.limits()
    }

    #[must_use]
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// One level deeper, for converting an embedded document. `Err` at the
    /// ceiling — the caller turns that into a `SkippedMember` warning rather
    /// than failing the whole document.
    pub fn descend(&self) -> Result<ConvertCx<'a>, ConvertError> {
        if self.depth + 1 > self.budget.limits.max_depth {
            return Err(ConvertError::LimitExceeded {
                limit: "max_depth",
                actual: u64::from(self.depth + 1),
                allowed: u64::from(self.budget.limits.max_depth),
            });
        }
        Ok(ConvertCx {
            budget: self.budget,
            depth: self.depth + 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx_limits() -> Limits {
        Limits {
            max_expanded_bytes: 100,
            max_embedded_documents: 2,
            max_depth: 2,
            ..Limits::default()
        }
    }

    #[test]
    fn expanded_bytes_accumulate_across_the_tree() {
        let b = Budget::new(cx_limits());
        assert!(b.charge_expanded(60).is_ok());
        // The second charge alone fits; the running total does not.
        let err = b.charge_expanded(60).unwrap_err();
        assert_eq!(err.code(), "limit_exceeded");
    }

    #[test]
    fn embedded_slots_are_capped() {
        let b = Budget::new(cx_limits());
        assert!(b.charge_embedded().is_ok());
        assert!(b.charge_embedded().is_ok());
        assert!(b.charge_embedded().is_err());
    }

    #[test]
    fn descend_stops_at_max_depth() {
        let b = Budget::new(cx_limits());
        let cx = ConvertCx::new(&b);
        let d1 = cx.descend().expect("depth 1");
        let d2 = d1.descend().expect("depth 2");
        assert_eq!(d2.depth(), 2);
        assert!(d2.descend().is_err());
    }

    #[test]
    fn budget_is_shared_not_per_level() {
        let b = Budget::new(cx_limits());
        let cx = ConvertCx::new(&b);
        let child = cx.descend().unwrap();
        child.budget().charge_expanded(100).unwrap();
        // The parent sees the child's spend — the point of sharing.
        assert!(cx.budget().charge_expanded(1).is_err());
    }

    #[test]
    fn input_size_is_checked_against_the_ceiling() {
        let b = Budget::new(Limits {
            max_input_bytes: 10,
            ..Limits::default()
        });
        assert!(b.check_input_size(10).is_ok());
        assert!(b.check_input_size(11).is_err());
    }
}
