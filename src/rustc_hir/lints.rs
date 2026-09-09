#[allow(unused_imports)]
use {alloc::boxed::Box};
use crate::rustc_data_structures::sync::{DynSend, DynSync};
use crate::rustc_error_messages::MultiSpan;
use crate::rustc_errors::{Diag, DiagCtxtHandle, Level};
use crate::rustc_hir_id::HirId;
use crate::rustc_lint_defs::LintId;

pub type DelayedLints = Box<[DelayedLint]>;

/// During ast lowering, no lints can be emitted.
/// That is because lints attach to nodes either in the AST, or on the built HIR.
/// When attached to AST nodes, they're emitted just before building HIR,
/// and then there's a gap where no lints can be emitted until HIR is done.
/// The variants in this enum represent lints that are temporarily stashed during
/// AST lowering to be emitted once HIR is built.
pub struct DelayedLint {
    pub lint_id: LintId,
    pub id: HirId,
    pub span: MultiSpan,
    pub callback: Box<
        dyn for<'a> FnOnce(DiagCtxtHandle<'a>, Level, &dyn core::any::Any) -> Diag<'a, ()>
            + DynSend
            + DynSync
            + 'static,
    >,
}

impl core::fmt::Debug for DelayedLint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DelayedLint")
            .field("lint_id", &self.lint_id)
            .field("id", &self.id)
            .field("span", &self.span)
            .finish()
    }
}
