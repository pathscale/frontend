// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::fmt;

use crate::rustc_middle::traits::ObligationCause;
use crate::rustc_middle::traits::query::NoSolution;
pub use crate::rustc_middle::traits::query::type_op::Normalize;
use crate::rustc_middle::ty::{self, Lift, ParamEnvAnd, Ty, TyCtxt, TypeFoldable, TypeVisitableExt};
use crate::rustc_span::Span;

use crate::rustc_trait_selection::infer::canonical::{CanonicalQueryInput, CanonicalQueryResponse};
use crate::rustc_trait_selection::traits::ObligationCtxt;

impl<'tcx, T> super::QueryTypeOp<'tcx> for Normalize<'tcx, T>
where
    T: Normalizable<'tcx> + 'tcx,
{
    type QueryResponse = T;

    fn try_fast_path(_tcx: TyCtxt<'tcx>, key: &ParamEnvAnd<'tcx, Self>) -> Option<T> {
        if !key.value.value.skip_normalization().has_aliases() {
            Some(key.value.value.skip_normalization())
        } else {
            None
        }
    }

    fn perform_query(
        tcx: TyCtxt<'tcx>,
        canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Self>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self::QueryResponse>, NoSolution> {
        T::type_op_method(tcx, canonicalized)
    }

    fn perform_locally_with_next_solver(
        ocx: &ObligationCtxt<'_, 'tcx>,
        key: ParamEnvAnd<'tcx, Self>,
        span: Span,
    ) -> Result<Self::QueryResponse, NoSolution> {
        ocx.deeply_normalize(
            &ObligationCause::dummy_with_span(span),
            key.param_env,
            key.value.value,
        )
        .map_err(|_| NoSolution)
    }
}

pub trait Normalizable<'tcx>:
    fmt::Debug + TypeFoldable<TyCtxt<'tcx>> + Lift<TyCtxt<'tcx>> + Copy
{
    fn type_op_method(
        tcx: TyCtxt<'tcx>,
        canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Normalize<'tcx, Self>>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self>, NoSolution>;
}

impl<'tcx> Normalizable<'tcx> for Ty<'tcx> {
    fn type_op_method(
        tcx: TyCtxt<'tcx>,
        canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Normalize<'tcx, Self>>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self>, NoSolution> {
        tcx.type_op_normalize_ty(canonicalized)
    }
}

impl<'tcx> Normalizable<'tcx> for ty::Clause<'tcx> {
    fn type_op_method(
        tcx: TyCtxt<'tcx>,
        canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Normalize<'tcx, Self>>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self>, NoSolution> {
        tcx.type_op_normalize_clause(canonicalized)
    }
}

impl<'tcx> Normalizable<'tcx> for ty::PolyFnSig<'tcx> {
    fn type_op_method(
        tcx: TyCtxt<'tcx>,
        canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Normalize<'tcx, Self>>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self>, NoSolution> {
        tcx.type_op_normalize_poly_fn_sig(canonicalized)
    }
}

impl<'tcx> Normalizable<'tcx> for ty::FnSig<'tcx> {
    fn type_op_method(
        tcx: TyCtxt<'tcx>,
        canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Normalize<'tcx, Self>>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self>, NoSolution> {
        tcx.type_op_normalize_fn_sig(canonicalized)
    }
}

/// This impl is not needed, since we never normalize type outlives clauses
/// in the old solver, but is required by trait bounds to be happy.
impl<'tcx> Normalizable<'tcx> for ty::PolyTypeOutlivesClause<'tcx> {
    fn type_op_method(
        _tcx: TyCtxt<'tcx>,
        _canonicalized: CanonicalQueryInput<'tcx, ParamEnvAnd<'tcx, Normalize<'tcx, Self>>>,
    ) -> Result<CanonicalQueryResponse<'tcx, Self>, NoSolution> {
        unreachable!("we never normalize PolyTypeOutlivesClause")
    }
}
