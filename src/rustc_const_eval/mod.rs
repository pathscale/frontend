// tidy-alphabetical-start
// tidy-alphabetical-end

#![warn(unqualified_local_imports)]
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing in
// this file at all, which is why they are not trimmed by inspection.

// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, so modules import them from there when they are used.
// ---------------------------------------------------------------------------------------------
pub mod check_consts;
pub mod const_eval;
mod diagnostics;
pub mod util;

use crate::rustc_middle::util::Providers;
use crate::rustc_middle::ty;

pub fn provide(providers: &mut Providers) {
    const_eval::provide(&mut providers.queries);
    providers.queries.tag_for_variant = const_eval::tag_for_variant_provider;
    providers.queries.eval_to_const_value_raw = const_eval::eval_to_const_value_raw_provider;
    providers.queries.eval_to_allocation_raw = const_eval::eval_to_allocation_raw_provider;
    providers.queries.eval_static_initializer = const_eval::eval_static_initializer_provider;
    providers.queries.eval_to_valtree = |tcx, ty::PseudoCanonicalInput { typing_env, value }| {
        const_eval::eval_to_valtree(tcx, typing_env, value)
    };
    providers.hooks.try_destructure_mir_constant_for_user_output =
        const_eval::try_destructure_mir_constant_for_user_output;
    providers.queries.valtree_to_const_val =
        |tcx, cv| const_eval::valtree_to_const_value(tcx, ty::TypingEnv::fully_monomorphized(), cv);
    providers.queries.check_validity_requirement = |tcx, (init_kind, param_env_and_ty)| {
        util::check_validity_requirement(tcx, init_kind, param_env_and_ty)
    };
    providers.hooks.validate_scalar_in_layout =
        |tcx, scalar, layout| util::validate_scalar_in_layout(tcx, scalar, layout);
}

/// Kept for callers that assert no interpreter contexts are created.
pub fn interp_cx_new_count() -> u64 {
    0
}
