//! Routines to check for relations between fully inferred types.
//!
//! FIXME: Move this to a more general place. The utility of this extends to
//! other areas of the compiler as well.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing in
// this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_infer::infer::TyCtxtInferExt;
use crate::rustc_middle::traits::ObligationCause;
use crate::rustc_middle::ty::{Ty, TyCtxt, TypingEnv, Unnormalized, Variance};
use crate::rustc_trait_selection::traits::ObligationCtxt;

/// Returns whether `src` is a subtype of `dest`, i.e. `src <: dest`.
pub fn sub_types<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    src: Ty<'tcx>,
    dest: Ty<'tcx>,
) -> bool {
    relate_types(tcx, typing_env, Variance::Covariant, src, dest)
}

/// Returns whether `src` is a subtype of `dest`, i.e. `src <: dest`.
///
/// When validating assignments, the variance should be `Covariant`. When checking
/// during `MirPhase` >= `MirPhase::Runtime(RuntimePhase::Initial)` variance should be `Invariant`
/// because we want to check for type equality.
pub fn relate_types<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    variance: Variance,
    src: Ty<'tcx>,
    dest: Ty<'tcx>,
) -> bool {
    if src == dest {
        return true;
    }

    let (infcx, param_env) = tcx.infer_ctxt().ignoring_regions().build_with_typing_env(typing_env);
    let ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    let src = ocx.normalize(&cause, param_env, Unnormalized::new_wip(src));
    let dest = ocx.normalize(&cause, param_env, Unnormalized::new_wip(dest));
    match ocx.relate(&cause, param_env, variance, src, dest) {
        Ok(()) => {}
        Err(_) => return false,
    };
    ocx.evaluate_obligations_error_on_ambiguity().no_errors()
}
