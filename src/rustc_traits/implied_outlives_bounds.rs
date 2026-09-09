//! Provider for the `implied_outlives_bounds` query.
//! Do not call this query directly. See
//! [`crate::rustc_trait_selection::traits::implied_outlives_bounds`].

use alloc::vec::Vec;
use crate::rustc_infer::infer::TyCtxtInferExt;
use crate::rustc_infer::infer::canonical::{self, Canonical};
use crate::rustc_infer::traits::query::OutlivesBound;
use crate::rustc_infer::traits::query::type_op::ImpliedOutlivesBounds;
use crate::rustc_middle::query::Providers;
use crate::rustc_middle::ty::{ParamEnvAnd, TyCtxt};
use crate::rustc_trait_selection::infer::InferCtxtBuilderExt;
use crate::rustc_trait_selection::traits::implied_outlives_bounds::query_compute_implied_outlives_bounds;
use crate::rustc_trait_selection::traits::query::{CanonicalImpliedOutlivesBoundsGoal, NoSolution};

pub(crate) fn provide(p: &mut Providers) {
    *p = Providers { implied_outlives_bounds, ..*p };
}

fn implied_outlives_bounds<'tcx>(
    tcx: TyCtxt<'tcx>,
    (goal, disable_implied_bounds_hack): (CanonicalImpliedOutlivesBoundsGoal<'tcx>, bool),
) -> Result<
    &'tcx Canonical<'tcx, canonical::QueryResponse<'tcx, Vec<OutlivesBound<'tcx>>>>,
    NoSolution,
> {
    tcx.infer_ctxt().enter_canonical_trait_query(&goal, |ocx, key| {
        let ParamEnvAnd { param_env, value: ImpliedOutlivesBounds { ty } } = key;
        query_compute_implied_outlives_bounds(ocx, param_env, ty, disable_implied_bounds_hack)
    })
}
