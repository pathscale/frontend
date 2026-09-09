// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_middle::traits::query::{DropckOutlivesResult, NoSolution};
use crate::rustc_middle::ty::{ParamEnvAnd, TyCtxt};
use crate::rustc_span::Span;

use crate::rustc_trait_selection::infer::canonical::{CanonicalQueryInput, CanonicalQueryResponse};
use crate::rustc_trait_selection::traits::ObligationCtxt;
use crate::rustc_trait_selection::traits::query::dropck_outlives::{
    compute_dropck_outlives_inner, trivial_dropck_outlives,
};
use crate::rustc_trait_selection::traits::query::type_op::DropckOutlives;

impl<'tcx> super::QueryTypeOp<'tcx> for DropckOutlives<'tcx> {
    type QueryResponse = DropckOutlivesResult<'tcx>;

    fn try_fast_path(
        tcx: TyCtxt<'tcx>,
        key: &ParamEnvAnd<'tcx, Self>,
    ) -> Option<Self::QueryResponse> {
        trivial_dropck_outlives(tcx, key.value.dropped_ty).then(DropckOutlivesResult::default)
    }

    fn perform_query(
        tcx: TyCtxt<'tcx>,
        canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Self>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self::QueryResponse>, NoSolution> {
        tcx.dropck_outlives(canonicalized)
    }

    fn perform_locally_with_next_solver(
        ocx: &ObligationCtxt<'_, 'tcx>,
        key: ParamEnvAnd<'tcx, Self>,
        span: Span,
    ) -> Result<Self::QueryResponse, NoSolution> {
        compute_dropck_outlives_inner(ocx, key.param_env.and(key.value), span)
    }
}
