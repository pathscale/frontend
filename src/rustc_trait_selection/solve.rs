// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub use crate::rustc_next_trait_solver::solve::*;

mod delegate;
mod fulfill;
pub mod inspect;
mod normalize;
mod select;

pub(crate) use delegate::SolverDelegate;
pub use fulfill::{FulfillmentCtxt, NextSolverError};
pub(crate) use normalize::deeply_normalize_for_diagnostics;
pub use normalize::{
    deeply_normalize, deeply_normalize_with_skipped_universes,
    deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals, normalize,
};
use crate::rustc_middle::query::Providers;
use crate::rustc_middle::ty::{RequiredDepth, TyCtxt};
pub use select::InferCtxtSelectExt;

fn evaluate_root_goal_for_proof_tree_raw<'tcx>(
    tcx: TyCtxt<'tcx>,
    key: (CanonicalInput<TyCtxt<'tcx>>, usize),
) -> (QueryResult<TyCtxt<'tcx>>, &'tcx inspect::Probe<TyCtxt<'tcx>>, RequiredDepth) {
    evaluate_root_goal_for_proof_tree_raw_provider::<SolverDelegate<'tcx>, TyCtxt<'tcx>>(
        tcx, key.0, key.1,
    )
}

pub fn provide(providers: &mut Providers) {
    *providers = Providers { evaluate_root_goal_for_proof_tree_raw, ..*providers };
}
