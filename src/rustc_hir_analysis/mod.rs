/*!

# typeck

The type checker is responsible for:

1. Determining the type of each expression.
2. Resolving methods and traits.
3. Guaranteeing that most type rules are met. ("Most?", you say, "why most?"
   Well, dear reader, read on.)

The main entry point is [`check_crate()`]. Type checking operates in
several major phases:

1. The collect phase first passes over all items and determines their
   type, without examining their "innards".

2. Variance inference then runs to compute the variance of each parameter.

3. Coherence checks for overlapping or orphaned impls.

4. Finally, the check phase then checks function bodies and so forth.
   Within the check phase, we check each function body one at a time
   (bodies of function expressions are checked as part of the
   containing function). Inference is used to supply types wherever
   they are unknown. The actual checking of a function itself has
   several phases (check, regionck, writeback), as discussed in the
   documentation for the [`check`] module.

The type checker is defined into various submodules which are documented
independently:

- hir_ty_lowering: lowers type-system entities from the [HIR][hir] to the
  [`crate::rustc_middle::ty`] representation.

- collect: computes the types of each top-level item and enters them into
  the `tcx.types` table for later use.

- coherence: enforces coherence rules, builds some tables.

- variance: variance inference

- outlives: outlives inference

- check: walks over function bodies and type checks them, inferring types for
  local variables, type parameters, etc as necessary.

- infer: finds the types to use for each type variable such that
  all subtyping and assignment constraints are met. In essence, the check
  module specifies the constraints, and the infer module solves them.

## Note

This API is completely unstable and subject to change.

*/


// tidy-alphabetical-start

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
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
// tidy-alphabetical-end

// The alloc prelude. These arrive with the standard prelude under `std`, name no path, and a
// `#[derive]` can use them without the name appearing in this file - so a `std::` search cannot
// see them and they are not trimmed by inspection. They sit below every `#![...]` because an
// inner attribute must precede all items, and a `use` is an item.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

// These are used by Clippy.
pub mod check;

pub mod autoderef;
mod check_unused;
mod coherence;
mod collect;
mod constrained_generic_params;
pub mod delegation;
pub mod diagnostics;
pub mod hir_ty_lowering;
pub mod hir_wf_check;
mod impl_wf_check;
mod outlives;
mod variance;

use crate::rustc_abi::{CVariadicStatus, ExternAbi};
use crate::rustc_data_structures::sync::{cost, run_stage_weighted};
use crate::rustc_hir as hir;
use crate::rustc_hir::def::DefKind;
use crate::rustc_middle::mir::interpret::GlobalId;
use crate::rustc_middle::query::Providers;
use crate::rustc_middle::ty::{Const, Ty, TyCtxt};
use crate::rustc_middle::{middle, ty};
use crate::rustc_session::diagnostics::feature_err;
use crate::rustc_span::{ErrorGuaranteed, Span};
use crate::rustc_trait_selection::traits;

pub use crate::rustc_hir_analysis::collect::suggest_impl_trait;
use crate::rustc_hir_analysis::hir_ty_lowering::HirTyLowerer;

fn check_c_variadic_abi(tcx: TyCtxt<'_>, decl: &hir::FnDecl<'_>, abi: ExternAbi, span: Span) {
    if !decl.c_variadic() {
        // Not even a variadic function.
        return;
    }

    match abi.supports_c_variadic() {
        CVariadicStatus::Stable => {}
        CVariadicStatus::NotSupported => {
            tcx.dcx()
                .create_err(diagnostics::VariadicFunctionCompatibleConvention {
                    span,
                    convention: &format!("{abi}"),
                })
                .emit();
        }
        CVariadicStatus::Unstable { feature } => {
            if !tcx.features().enabled(feature) {
                feature_err(
                    &tcx.sess,
                    feature,
                    span,
                    format!("C-variadic functions with the {abi} calling convention are unstable"),
                )
                .emit();
            }
        }
    }
}

/// Adds query implementations to the [Providers] vtable, see [`crate::rustc_middle::query`]
pub fn provide(providers: &mut Providers) {
    collect::provide(providers);
    coherence::provide(providers);
    check::provide(providers);
    *providers = Providers {
        check_unused_traits: check_unused::check_unused_traits,
        diagnostic_hir_wf_check: hir_wf_check::diagnostic_hir_wf_check,
        inferred_outlives_crate: outlives::inferred_outlives_crate,
        inferred_outlives_of: outlives::inferred_outlives_of,
        inherit_sig_for_delegation_item: delegation::inherit_sig_for_delegation_item,
        delegation_user_specified_args: delegation::delegation_user_specified_args,
        enforce_impl_non_lifetime_params_are_constrained:
            impl_wf_check::enforce_impl_non_lifetime_params_are_constrained,
        crate_variances: variance::crate_variances,
        variances_of: variance::variances_of,
        ..*providers
    };
}

pub fn check_crate(tcx: TyCtxt<'_>) {
    let _prof_timer = tcx.sess.timer("type_check_crate");

    tcx.sess.time("coherence_checking", || {
        // When discarding query call results, use an explicit type to indicate
        // what we are intending to discard, to help future type-based refactoring.
        type R = Result<(), ErrorGuaranteed>;

        let _: R = tcx.ensure_result().check_type_wf(());

        // One stage over the traits with local impls, read in place from the frozen map. Each
        // trait's coherence is its own query and reads nothing another trait's check writes.
        // A trait weighs its local impls' source: what its coherence check reads.
        let trait_impls = tcx.all_local_trait_impls(());
        run_stage_weighted(
            trait_impls,
            trait_impls.len(),
            |trait_impls, index| {
                let (_, impls) = trait_impls.get_index(index).expect("index below len");
                impls.iter().fold(0u32, |weight, &impl_def_id| {
                    weight.saturating_add(tcx.stage_weight(impl_def_id, cost::TYPECK))
                })
            },
            |trait_impls, index| {
                let (&trait_def_id, _) = trait_impls.get_index(index).expect("index below len");
                let _: R = tcx.ensure_result().coherent_trait(trait_def_id);
            },
        );
        // these queries are executed for side-effects (error reporting):
        let _: R = tcx.ensure_result().crate_inherent_impls_validity_check(());
        let _: R = tcx.ensure_result().crate_inherent_impls_overlap_check(());
    });

    // One stage over the body owners, each item read in place from the crate's frozen list, and
    // weighing its source at type checking's rate.
    let owners = tcx.hir_body_owner_ids();
    run_stage_weighted(owners, owners.len(), |owners, index| {
        tcx.stage_weight(owners[index], cost::TYPECK)
    }, |owners, index| {
        let item_def_id = owners[index];
        let def_kind = tcx.def_kind(item_def_id);
        // Make sure we evaluate all static and (non-associated) const items, even if unused.
        // If any of these fail to evaluate, we do not want this crate to pass compilation.
        match def_kind {
            DefKind::Static { .. } => {
                tcx.ensure_ok().eval_static_initializer(item_def_id);
                check::maybe_check_static_with_link_section(tcx, item_def_id);
            }
            DefKind::Const { .. }
                if !tcx.generics_of(item_def_id).own_requires_monomorphization()
                    && !tcx.is_type_const(item_def_id) =>
            {
                // FIXME(generic_const_items): Passing empty instead of identity args is fishy but
                //                             seems to be fine for now. Revisit this!
                let instance = ty::Instance::new_raw(item_def_id.into(), ty::GenericArgs::empty());
                let cid = GlobalId { instance, promoted: None };
                let typing_env = ty::TypingEnv::fully_monomorphized();
                tcx.ensure_ok().eval_to_const_value_raw(typing_env.as_query_input(cid));
            }
            _ => (),
        }
        // Skip `AnonConst`s and type system `InlineConst`s because we feed their `type_of` in
        // `feed_anon_const_type`.
        // Also skip items for which typeck forwards to parent typeck.
        if !(def_kind == DefKind::AnonConst
            && tcx.anon_const_kind(item_def_id) != ty::AnonConstKind::NonTypeSystemInline
            || tcx.is_typeck_child(item_def_id.to_def_id()))
        {
            tcx.ensure_ok().typeck(item_def_id);
        }
    });

    // This has to be a second pass over the body owners, after every body has been
    // type-checked above. `needs_coroutine_by_move_body_def_id` asks for `type_of`, and for a
    // body owner nested inside a const argument's anon const that goes through `typeck` of the
    // anon const, which needs the anon const's own type. That type is never computed, only fed
    // while the enclosing body is type-checked. Doing this in the pass above lets the parallel
    // front end reach the nested body owner first, computing (and caching) an error type for
    // the anon const that then conflicts with the type fed later on.
    //
    // Each item is a lookup or two, not a walk of the body: one nanosecond an item, so only a
    // crate of a quarter of a million bodies fans it out.
    run_stage_weighted(owners, owners.len(), |_, _| 1, |owners, index| {
        let item_def_id = owners[index];
        // Ensure we generate the new `DefId` before finishing `check_crate`.
        // Afterwards we freeze the list of `DefId`s.
        if tcx.needs_coroutine_by_move_body_def_id(item_def_id.to_def_id()) {
            tcx.ensure_done().coroutine_by_move_body_def_id(item_def_id);
        }
    });

    if tcx.features().rustc_attrs() {
        tcx.sess.time("dumping_rustc_attr_data", || {
            // tidy-alphabetical-start
            collect::dump::clauses_and_item_bounds(tcx);
            collect::dump::def_parents(tcx);
            collect::dump::generics(tcx);
            collect::dump::object_lifetime_defaults(tcx);
            collect::dump::opaque_hidden_types(tcx);
            collect::dump::vtables(tcx);
            outlives::dump::inferred_outlives(tcx);
            variance::dump::variances(tcx);
            // tidy-alphabetical-end
        });
    }

    tcx.ensure_ok().check_unused_traits(());
}

/// Lower a [`hir::Ty`] to a [`Ty`].
///
/// <div class="warning">
///
/// This function is **quasi-deprecated**. It can cause ICEs if called inside of a body
/// (of a function or constant) and especially if it contains inferred types (`_`).
///
/// It's used in rustdoc and Clippy.
///
/// </div>
pub fn lower_ty<'tcx>(tcx: TyCtxt<'tcx>, hir_ty: &hir::Ty<'_>) -> Ty<'tcx> {
    // In case there are any projections, etc., find the "environment"
    // def-ID that will be used to determine the traits/predicates in
    // scope. This is derived from the enclosing item-like thing.
    let env_def_id = tcx.hir_get_parent_item(hir_ty.hir_id);
    collect::ItemCtxt::new(tcx, env_def_id.def_id)
        .lowerer()
        .lower_ty_maybe_return_type_notation(hir_ty)
}

/// This is for rustdoc.
// FIXME(const_generics): having special methods for rustdoc in `rustc_hir_analysis` is cursed
pub fn lower_const_arg_for_rustdoc<'tcx>(
    tcx: TyCtxt<'tcx>,
    hir_ct: &hir::ConstArg<'_>,
    ty: Ty<'tcx>,
) -> Const<'tcx> {
    let env_def_id = tcx.hir_get_parent_item(hir_ct.hir_id);
    collect::ItemCtxt::new(tcx, env_def_id.def_id).lowerer().lower_const_arg(hir_ct, ty)
}
