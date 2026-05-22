//! Karpathy "LLM Wiki" consolidation pipeline.
//!
//! M7a delivers the single-page variant: rewrite one
//! `sessions/<id>.md` page from raw observations via an LLM. The
//! store's sha256-equality short-circuit + supersession chain means
//! the rewrite is a *version*, not a destructive overwrite —
//! exactly the Karpathy pattern.
//!
//! M7b extends this to multi-page atomic fan-out.

pub mod consolidator;
pub mod lint;
pub mod sweep;
pub mod types;

pub use consolidator::{Consolidator, ConsolidatorError, ConsolidatorResult};
pub use lint::{LintError, LintFinding, LintReport, run_lint};
pub use sweep::{EvictedPage, SweepError, SweepReport, run_sweep};
pub use types::{
    ConsolidatedBatch, ConsolidatedPage, ConsolidatedPageUpdate, ConsolidationOutcome,
};
