//! Generic compiler-resolved evidence attached to one selected source call.
//!
//! This module records call substitutions, the callable's declared trait predicates, and the
//! compiler-selected implementation for each monomorphic predicate. It never infers selection
//! from the set of impls in scope. Unsupported identities and failed compiler queries remain
//! explicit `Unknown` facts.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::rustc_hir::def::DefKind;
use crate::rustc_hir::def_id::{DefId, LocalDefId};
use crate::rustc_middle::traits::ImplSource;
use crate::rustc_middle::ty::{
    self, ClauseKind, GenericArgsRef, RegionExt, TyCtxt, TypeVisitableExt, Unnormalized,
};

use super::effects::StaticFact;
use super::{
    ResolvedCallableAssociatedTypeProjection, ResolvedCallableConstIdentity,
    ResolvedCallableDeclaredTraitPredicate, ResolvedCallableGenericArgumentIdentity,
    ResolvedCallableGenericParameterKind, ResolvedCallableGenericSubstitution,
    ResolvedCallableGenericTypeIdentity, ResolvedCallableTraitMethodBody,
    ResolvedCallableTraitMethodImplementation, ResolvedCallableTraitObligation,
    ResolvedCallableTraitPolarity, ResolvedCallableTraitRefIdentity,
    ResolvedCallableTraitSelection, ResolvedPrimitive, ResolvedReferenceMutability,
    ResolvedRegionIdentity, StableDefPathIdentity,
};

const MAX_PROOF_IDENTITY_DEPTH: u8 = 8;
const MAX_PROOF_TUPLE_ARITY: usize = 64;
const MAX_PROOF_CLAUSES: usize = 64;
const MAX_PROOF_ASSOCIATED_TYPES: usize = 32;
const MAX_PROOF_TRAIT_METHODS: usize = 64;
const MAX_PROOF_GENERIC_ARGS: usize = 64;
const MAX_PROOF_SOLVER_QUERIES: usize = 64;

struct ProofBudget {
    solver_queries_left: usize,
}

impl ProofBudget {
    fn new() -> Self {
        Self {
            solver_queries_left: MAX_PROOF_SOLVER_QUERIES,
        }
    }

    fn consume_solver_query(&mut self) -> bool {
        if self.solver_queries_left == 0 {
            return false;
        }
        self.solver_queries_left -= 1;
        true
    }
}

/// Capture compiler-resolved substitutions for the callable at the source call expression.
pub(super) fn callable_substitutions<'tcx>(
    tcx: TyCtxt<'tcx>,
    typeck: &crate::rustc_middle::ty::TypeckResults<'tcx>,
    def_id: DefId,
    args_hir_id: crate::rustc_hir::HirId,
    caller: LocalDefId,
) -> StaticFact<Vec<ResolvedCallableGenericSubstitution>> {
    let args = match callable_args(tcx, typeck, def_id, args_hir_id) {
        Ok(args) => args,
        Err(why) => return unknown(why),
    };
    generic_substitutions(tcx, def_id, args, caller.to_def_id(), true)
}

/// Capture the canonical, uninstantiated trait predicates declared on a callable.
/// Generic parameters are preserved by stable parameter definition identity.
pub(super) fn callable_declared_trait_predicates(
    tcx: TyCtxt<'_>,
    def_id: DefId,
) -> StaticFact<Vec<ResolvedCallableDeclaredTraitPredicate>> {
    let clauses = tcx.clauses_of(def_id).instantiate_identity(tcx);
    if clauses.clauses.len() > MAX_PROOF_CLAUSES {
        return unknown("callable_predicate_count_limit");
    }
    let mut predicates = Vec::new();
    for (ordinal, clause) in clauses.clauses.into_iter().enumerate() {
        let Ok(ordinal) = u32::try_from(ordinal) else {
            return unknown("callable_predicate_ordinal_overflow");
        };
        let kind = clause.skip_norm_wip().kind();
        let has_bound_vars = kind.no_bound_vars().is_none();
        let ClauseKind::Trait(predicate) = kind.skip_binder() else {
            continue;
        };
        let trait_ref = if has_bound_vars {
            unknown("higher_ranked_trait_predicate")
        } else {
            trait_ref_identity(tcx, predicate.trait_ref, def_id, true, 0)
        };
        predicates.push(ResolvedCallableDeclaredTraitPredicate {
            ordinal,
            polarity: polarity(predicate.polarity),
            has_bound_vars,
            trait_ref,
        });
    }
    StaticFact::Known { value: predicates }
}

/// Capture instantiated declared trait obligations and ask rustc for each concrete selection.
pub(super) fn callable_trait_obligations<'tcx>(
    tcx: TyCtxt<'tcx>,
    typeck: &crate::rustc_middle::ty::TypeckResults<'tcx>,
    def_id: DefId,
    args_hir_id: crate::rustc_hir::HirId,
    caller: LocalDefId,
) -> StaticFact<Vec<ResolvedCallableTraitObligation>> {
    let args = match callable_args(tcx, typeck, def_id, args_hir_id) {
        Ok(args) => args,
        Err(why) => return unknown(why),
    };
    let clauses = tcx.clauses_of(def_id).instantiate(tcx, args);
    if clauses.clauses.len() > MAX_PROOF_CLAUSES {
        return unknown("callable_obligation_count_limit");
    }
    let mut budget = ProofBudget::new();
    let mut obligations = Vec::new();
    for (ordinal, clause) in clauses.clauses.into_iter().enumerate() {
        let Ok(ordinal) = u32::try_from(ordinal) else {
            return unknown("callable_obligation_ordinal_overflow");
        };
        let kind = clause.skip_norm_wip().kind();
        let has_bound_vars = kind.no_bound_vars().is_none();
        let ClauseKind::Trait(predicate) = kind.skip_binder() else {
            continue;
        };
        let trait_id = predicate.trait_ref.def_id;
        let (trait_ref, selection) = if has_bound_vars {
            (
                trait_ref_identity(tcx, predicate.trait_ref, caller.to_def_id(), true, 0),
                unknown("higher_ranked_trait_obligation"),
            )
        } else if predicate.polarity == ty::ClausePolarity::Negative {
            (
                trait_ref_identity(tcx, predicate.trait_ref, caller.to_def_id(), true, 0),
                unknown("negative_trait_obligation"),
            )
        } else {
            select_trait_implementation(tcx, predicate.trait_ref, caller, 0, &mut budget)
        };
        obligations.push(ResolvedCallableTraitObligation {
            ordinal,
            polarity: polarity(predicate.polarity),
            trait_ref,
            trait_source_attestation: super::callable_source_attestation::attest_definition(
                tcx,
                trait_id,
                super::DefinitionSourceKind::Trait,
            ),
            selection,
        });
    }
    StaticFact::Known { value: obligations }
}

fn callable_args<'tcx>(
    tcx: TyCtxt<'tcx>,
    typeck: &crate::rustc_middle::ty::TypeckResults<'tcx>,
    def_id: DefId,
    args_hir_id: crate::rustc_hir::HirId,
) -> Result<GenericArgsRef<'tcx>, &'static str> {
    let count = tcx.generics_of(def_id).count();
    if count > MAX_PROOF_GENERIC_ARGS {
        return Err("call_generic_argument_count_limit");
    }
    if count == 0 {
        return Ok(tcx.mk_args(&[]));
    }
    let Some(args) = typeck.node_args_opt(args_hir_id) else {
        return Err("call_generic_arguments_unavailable");
    };
    if args.len() != count {
        return Err("call_generic_argument_count_mismatch");
    }
    Ok(args)
}

fn select_trait_implementation<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_ref: ty::TraitRef<'tcx>,
    caller: LocalDefId,
    depth: u8,
    budget: &mut ProofBudget,
) -> (
    StaticFact<ResolvedCallableTraitRefIdentity>,
    StaticFact<ResolvedCallableTraitSelection>,
) {
    let caller_def_id = caller.to_def_id();
    let original_identity = trait_ref_identity(tcx, trait_ref, caller_def_id, true, depth);
    if depth > MAX_PROOF_IDENTITY_DEPTH {
        return (original_identity, unknown("trait_selection_depth_limit"));
    }
    if !fully_monomorphic(&trait_ref) {
        return (
            original_identity,
            unknown("trait_obligation_not_fully_monomorphic"),
        );
    }

    let typing_env = ty::TypingEnv::fully_monomorphized();
    let Ok(normalized) =
        tcx.try_normalize_erasing_regions(typing_env, Unnormalized::new_wip(trait_ref))
    else {
        return (
            original_identity,
            unknown("trait_obligation_normalization_failed"),
        );
    };
    if !fully_monomorphic(&normalized) || normalized.has_aliases() {
        return (
            trait_ref_identity(tcx, normalized, caller_def_id, false, depth),
            unknown("trait_obligation_not_fully_normalized"),
        );
    }

    let normalized_identity = trait_ref_identity(tcx, normalized, caller_def_id, false, depth);
    if !trait_ref_identity_is_complete(&normalized_identity) {
        return (
            normalized_identity,
            unknown("trait_obligation_identity_unavailable"),
        );
    }
    if !budget.consume_solver_query() {
        return (
            normalized_identity,
            unknown("trait_solver_query_count_limit"),
        );
    }

    // Query only after rejecting parameters, inference variables, placeholders, bound variables,
    // error types, unresolved aliases, and unrepresentable argument identities. Query failures
    // are facts, never reasons to panic or to fall back to impl enumeration.
    match tcx.codegen_select_candidate(typing_env.as_query_input(normalized)) {
        Ok(ImplSource::UserDefined(selected)) => (
            normalized_identity,
            StaticFact::Known {
                value: user_defined_selection(
                    tcx,
                    selected.impl_def_id,
                    selected.args,
                    caller,
                    depth,
                    budget,
                ),
            },
        ),
        Ok(ImplSource::Param(_)) => (
            normalized_identity,
            StaticFact::Known {
                value: ResolvedCallableTraitSelection::Param,
            },
        ),
        Ok(ImplSource::Builtin(_, _)) => (
            normalized_identity,
            StaticFact::Known {
                value: ResolvedCallableTraitSelection::Builtin,
            },
        ),
        Err(_) => (
            normalized_identity,
            unknown("trait_solver_selection_failed"),
        ),
    }
}

fn user_defined_selection<'tcx>(
    tcx: TyCtxt<'tcx>,
    impl_def_id: DefId,
    impl_args: GenericArgsRef<'tcx>,
    caller: LocalDefId,
    depth: u8,
    budget: &mut ProofBudget,
) -> ResolvedCallableTraitSelection {
    let implementation = super::stable_def_path_identity(tcx, impl_def_id);
    let implementation_arguments =
        generic_substitutions(tcx, impl_def_id, impl_args, caller.to_def_id(), false);
    let implementation_source_attestation = super::callable_source_attestation::attest_definition(
        tcx,
        impl_def_id,
        super::DefinitionSourceKind::TraitImplementation,
    );
    let implemented_trait = implemented_trait_ref(tcx, impl_def_id, impl_args, caller, depth);
    let associated_types =
        selected_associated_types(tcx, impl_def_id, impl_args, caller, depth, budget);
    let methods = selected_trait_methods(tcx, impl_def_id, impl_args, caller, budget);
    ResolvedCallableTraitSelection::UserDefined {
        implementation,
        implementation_arguments,
        implemented_trait,
        implementation_source_attestation,
        associated_types,
        methods,
    }
}

fn implemented_trait_ref<'tcx>(
    tcx: TyCtxt<'tcx>,
    impl_def_id: DefId,
    impl_args: GenericArgsRef<'tcx>,
    caller: LocalDefId,
    depth: u8,
) -> StaticFact<ResolvedCallableTraitRefIdentity> {
    if !tcx.impl_is_of_trait(impl_def_id) {
        return unknown("selected_definition_is_not_trait_impl");
    }
    if !fully_monomorphic(&impl_args) {
        return unknown("selected_impl_arguments_not_fully_monomorphic");
    }
    let typing_env = ty::TypingEnv::fully_monomorphized();
    let raw = tcx.impl_trait_ref(impl_def_id).instantiate(tcx, impl_args);
    let Ok(normalized) = tcx.try_normalize_erasing_regions(typing_env, raw)
    else {
        return unknown("selected_impl_trait_ref_normalization_failed");
    };
    if !fully_monomorphic(&normalized) || normalized.has_aliases() {
        return unknown("selected_impl_trait_ref_not_fully_normalized");
    }
    let identity = trait_ref_identity(tcx, normalized, caller.to_def_id(), false, depth);
    if trait_ref_identity_is_complete(&identity) {
        identity
    } else {
        unknown("selected_impl_trait_ref_identity_unavailable")
    }
}

fn selected_associated_types<'tcx>(
    tcx: TyCtxt<'tcx>,
    impl_def_id: DefId,
    impl_args: GenericArgsRef<'tcx>,
    caller: LocalDefId,
    depth: u8,
    budget: &mut ProofBudget,
) -> StaticFact<Vec<ResolvedCallableAssociatedTypeProjection>> {
    if !tcx.impl_is_of_trait(impl_def_id) {
        return unknown("selected_definition_is_not_trait_impl");
    }
    let parent = tcx
        .impl_trait_ref(impl_def_id)
        .instantiate(tcx, impl_args)
        .skip_norm_wip();
    if !fully_monomorphic(&parent) {
        return unknown("selected_impl_trait_ref_not_fully_monomorphic");
    }
    let trait_id = parent.def_id;
    let items = tcx
        .associated_items(trait_id)
        .in_definition_order()
        .filter(|item| item.is_type())
        .collect::<Vec<_>>();
    if items.len() > MAX_PROOF_ASSOCIATED_TYPES {
        return unknown("associated_type_count_limit");
    }
    let mut projections = Vec::with_capacity(items.len());
    for trait_item in items {
        let trait_associated_type = super::stable_def_path_identity(tcx, trait_item.def_id);
        let trait_item_source_attestation = super::callable_source_attestation::attest_definition(
            tcx,
            trait_item.def_id,
            super::DefinitionSourceKind::AssociatedType,
        );
        let (projection_arguments, projection_args) =
            match associated_type_arguments(tcx, trait_item.def_id, trait_id, parent.args) {
                Ok(args) => (
                    generic_substitutions(tcx, trait_item.def_id, args, caller.to_def_id(), false),
                    Some(args),
                ),
                Err(why) => (unknown(why), None),
            };
        let (
            implementation_item,
            implementation_item_arguments,
            implementation_item_source_attestation,
        ) = if let Some(&impl_item_id) = tcx
            .impl_item_implementor_ids(impl_def_id)
            .get(&trait_item.def_id)
        {
            let maps_to_trait_item = tcx.opt_associated_item(impl_item_id).is_some_and(|item| {
                item.trait_item_def_id() == Some(trait_item.def_id)
                    && item.impl_container(tcx) == Some(impl_def_id)
            });
            if tcx.def_kind(impl_item_id) != DefKind::AssocTy || !maps_to_trait_item {
                (
                    unknown("selected_associated_type_mapping_kind_mismatch"),
                    unknown("selected_associated_type_mapping_kind_mismatch"),
                    unknown("selected_associated_type_mapping_kind_mismatch"),
                )
            } else {
                let implementation_args = match projection_args {
                    Some(projection_args) => {
                        let args = projection_args.rebase_onto(tcx, trait_id, impl_args);
                        let generics = tcx.generics_of(impl_item_id);
                        if generics.parent != Some(impl_def_id)
                            || generics.parent_count != impl_args.len()
                            || !tcx.check_args_compatible(impl_item_id, args)
                        {
                            unknown("selected_associated_type_argument_rebase_failed")
                        } else {
                            generic_substitutions(
                                tcx,
                                impl_item_id,
                                args,
                                caller.to_def_id(),
                                false,
                            )
                        }
                    }
                    None => unknown("associated_type_projection_arguments_unavailable"),
                };
                (
                    StaticFact::Known {
                        value: super::stable_def_path_identity(tcx, impl_item_id),
                    },
                    implementation_args,
                    super::callable_source_attestation::attest_definition(
                        tcx,
                        impl_item_id,
                        super::DefinitionSourceKind::AssociatedType,
                    ),
                )
            }
        } else {
            (
                unknown("selected_associated_type_mapping_unavailable"),
                unknown("selected_associated_type_mapping_unavailable"),
                unknown("selected_associated_type_mapping_unavailable"),
            )
        };
        let (value, normalized_projection) = match projection_args {
            Some(args) => {
                let projection =
                    ty::Ty::new_projection_from_args(tcx, ty::IsRigid::No, trait_item.def_id, args);
                let typing_env = ty::TypingEnv::fully_monomorphized();
                match tcx
                    .try_normalize_erasing_regions(typing_env, Unnormalized::new_wip(projection))
                {
                    Ok(value) if fully_monomorphic(&value) && !value.has_aliases() => (
                        generic_type_identity(tcx, value, caller.to_def_id(), false, depth + 1),
                        Some(value),
                    ),
                    Ok(_) => (unknown("associated_type_value_not_fully_normalized"), None),
                    Err(_) => (
                        unknown("associated_type_projection_normalization_failed"),
                        None,
                    ),
                }
            }
            None => (
                unknown("associated_type_projection_arguments_unavailable"),
                None,
            ),
        };
        let trait_obligations = match (projection_args, normalized_projection) {
            (Some(args), Some(projected)) => associated_type_trait_obligations(
                tcx,
                trait_item.def_id,
                args,
                projected,
                caller,
                depth + 1,
                budget,
            ),
            _ => unknown("associated_type_projection_unavailable_for_bounds"),
        };
        projections.push(ResolvedCallableAssociatedTypeProjection {
            trait_associated_type,
            trait_item_source_attestation,
            projection_arguments,
            implementation_item,
            implementation_item_arguments,
            implementation_item_source_attestation,
            value,
            trait_obligations,
        });
    }
    StaticFact::Known { value: projections }
}

fn associated_type_arguments<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_associated_type: DefId,
    trait_id: DefId,
    trait_parent_args: GenericArgsRef<'tcx>,
) -> Result<GenericArgsRef<'tcx>, &'static str> {
    let generics = tcx.generics_of(trait_associated_type);
    if generics.parent != Some(trait_id) || generics.parent_count != trait_parent_args.len() {
        return Err("associated_type_parent_argument_mismatch");
    }
    let mut arguments = trait_parent_args.iter().collect::<Vec<_>>();
    for parameter in generics.own_params.iter() {
        match parameter.kind {
            ty::GenericParamDefKind::Lifetime => {
                arguments.push(tcx.lifetimes.re_erased.into());
            }
            ty::GenericParamDefKind::Type { .. } => {
                return Err("associated_type_own_type_argument_unavailable");
            }
            ty::GenericParamDefKind::Const { .. } => {
                return Err("associated_type_own_const_argument_unavailable");
            }
        }
    }
    let arguments = tcx.mk_args(&arguments);
    if !tcx.check_args_compatible(trait_associated_type, arguments) {
        return Err("associated_type_argument_kind_mismatch");
    }
    Ok(arguments)
}

fn associated_type_trait_obligations<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_associated_type: DefId,
    arguments: GenericArgsRef<'tcx>,
    normalized_projection: ty::Ty<'tcx>,
    caller: LocalDefId,
    depth: u8,
    budget: &mut ProofBudget,
) -> StaticFact<Vec<ResolvedCallableTraitObligation>> {
    let clauses = tcx
        .item_bounds(trait_associated_type)
        .instantiate(tcx, arguments)
        .skip_norm_wip();
    if clauses.len() > MAX_PROOF_CLAUSES {
        return unknown("associated_type_trait_bound_count_limit");
    }
    let typing_env = ty::TypingEnv::fully_monomorphized();
    let mut obligations = Vec::new();
    for (ordinal, clause) in clauses.iter().enumerate() {
        let Ok(ordinal) = u32::try_from(ordinal) else {
            return unknown("associated_type_trait_bound_ordinal_overflow");
        };
        let kind = clause.kind();
        let has_bound_vars = kind.no_bound_vars().is_none();
        let ClauseKind::Trait(predicate) = kind.skip_binder() else {
            continue;
        };
        let trait_id = predicate.trait_ref.def_id;
        let (trait_ref, selection) = if has_bound_vars {
            (
                trait_ref_identity(tcx, predicate.trait_ref, caller.to_def_id(), true, depth),
                unknown("higher_ranked_associated_type_trait_bound"),
            )
        } else if predicate.polarity == ty::ClausePolarity::Negative {
            (
                trait_ref_identity(tcx, predicate.trait_ref, caller.to_def_id(), true, depth),
                unknown("negative_associated_type_trait_bound"),
            )
        } else {
            match tcx.try_normalize_erasing_regions(
                typing_env,
                Unnormalized::new_wip(predicate.trait_ref),
            ) {
                Ok(normalized) if fully_monomorphic(&normalized) && !normalized.has_aliases() => {
                    if normalized.self_ty() != normalized_projection {
                        (
                            trait_ref_identity(tcx, normalized, caller.to_def_id(), false, depth),
                            unknown("associated_bound_self_projection_mismatch"),
                        )
                    } else {
                        select_trait_implementation(tcx, normalized, caller, depth, budget)
                    }
                }
                Ok(normalized) => (
                    trait_ref_identity(tcx, normalized, caller.to_def_id(), false, depth),
                    unknown("associated_type_trait_bound_not_fully_normalized"),
                ),
                Err(_) => (
                    trait_ref_identity(tcx, predicate.trait_ref, caller.to_def_id(), true, depth),
                    unknown("associated_type_trait_bound_normalization_failed"),
                ),
            }
        };
        obligations.push(ResolvedCallableTraitObligation {
            ordinal,
            polarity: polarity(predicate.polarity),
            trait_ref,
            trait_source_attestation: super::callable_source_attestation::attest_definition(
                tcx,
                trait_id,
                super::DefinitionSourceKind::Trait,
            ),
            selection,
        });
    }
    StaticFact::Known { value: obligations }
}

fn selected_trait_methods<'tcx>(
    tcx: TyCtxt<'tcx>,
    impl_def_id: DefId,
    impl_args: GenericArgsRef<'tcx>,
    caller: LocalDefId,
    budget: &mut ProofBudget,
) -> StaticFact<Vec<ResolvedCallableTraitMethodImplementation>> {
    if !tcx.impl_is_of_trait(impl_def_id) {
        return unknown("selected_definition_is_not_trait_impl");
    }
    let trait_ref = tcx
        .impl_trait_ref(impl_def_id)
        .instantiate(tcx, impl_args)
        .skip_norm_wip();
    if !fully_monomorphic(&trait_ref) {
        return unknown("selected_method_trait_ref_not_fully_monomorphic");
    }
    let trait_id = trait_ref.def_id;
    let items = tcx
        .associated_items(trait_id)
        .in_definition_order()
        .filter(|item| item.is_fn())
        .collect::<Vec<_>>();
    if items.len() > MAX_PROOF_TRAIT_METHODS {
        return unknown("trait_method_count_limit");
    }
    let mut methods = Vec::with_capacity(items.len());
    for trait_method in items {
        methods.push(ResolvedCallableTraitMethodImplementation {
            trait_method: super::stable_def_path_identity(tcx, trait_method.def_id),
            trait_method_source_attestation: super::callable_source_attestation::attest_definition(
                tcx,
                trait_method.def_id,
                super::DefinitionSourceKind::TraitMethod,
            ),
            selected_body: selected_trait_method_body(
                tcx,
                trait_id,
                trait_ref.args,
                impl_def_id,
                trait_method.def_id,
                caller,
                budget,
            ),
        });
    }
    StaticFact::Known { value: methods }
}

fn selected_trait_method_body<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_id: DefId,
    trait_parent_args: GenericArgsRef<'tcx>,
    impl_def_id: DefId,
    trait_method: DefId,
    caller: LocalDefId,
    budget: &mut ProofBudget,
) -> StaticFact<ResolvedCallableTraitMethodBody> {
    let generics = tcx.generics_of(trait_method);
    if generics.parent != Some(trait_id) || generics.parent_count != trait_parent_args.len() {
        return unknown("trait_method_parent_argument_mismatch");
    }
    let mut arguments = trait_parent_args.iter().collect::<Vec<_>>();
    for parameter in generics.own_params.iter() {
        match parameter.kind {
            ty::GenericParamDefKind::Lifetime => {
                arguments.push(tcx.lifetimes.re_erased.into());
            }
            ty::GenericParamDefKind::Type { .. } => {
                return unknown("trait_method_own_type_argument_unavailable");
            }
            ty::GenericParamDefKind::Const { .. } => {
                return unknown("trait_method_own_const_argument_unavailable");
            }
        }
    }
    let method_args = tcx.mk_args(&arguments);
    if !tcx.check_args_compatible(trait_method, method_args) {
        return unknown("trait_method_argument_kind_mismatch");
    }
    if !budget.consume_solver_query() {
        return unknown("trait_method_resolution_query_count_limit");
    }
    let typing_env = ty::TypingEnv::fully_monomorphized();
    let instance = match ty::Instance::try_resolve(tcx, typing_env, trait_method, method_args) {
        Ok(Some(instance)) => instance,
        Ok(None) => return unknown("selected_trait_method_body_unresolved"),
        Err(_) => return unknown("selected_trait_method_instance_query_failed"),
    };
    let ty::InstanceKind::Item(definition) = instance.def else {
        return unknown("selected_trait_method_body_not_item");
    };
    if !matches!(tcx.def_kind(definition), DefKind::Fn | DefKind::AssocFn) {
        return unknown("selected_trait_method_body_kind_mismatch");
    }
    let resolved_trait_method = tcx
        .opt_associated_item(definition)
        .and_then(|item| item.trait_item_def_id())
        .or_else(|| (definition == trait_method).then_some(trait_method));
    if resolved_trait_method != Some(trait_method) {
        return unknown("selected_trait_method_identity_mismatch");
    }
    if let Some(&mapped) = tcx
        .impl_item_implementor_ids(impl_def_id)
        .get(&trait_method)
    {
        if mapped != definition {
            return unknown("selected_trait_method_map_instance_disagreement");
        }
    }
    let defining_owner = tcx.parent(definition);
    let (source_kind, defining_owner_source_attestation) = match tcx.def_kind(defining_owner) {
        DefKind::Impl { of_trait: true } => (
            super::DefinitionSourceKind::ImplementationMethod,
            super::callable_source_attestation::attest_definition(
                tcx,
                defining_owner,
                super::DefinitionSourceKind::TraitImplementation,
            ),
        ),
        DefKind::Trait => (
            super::DefinitionSourceKind::TraitMethod,
            super::callable_source_attestation::attest_definition(
                tcx,
                defining_owner,
                super::DefinitionSourceKind::Trait,
            ),
        ),
        _ => return unknown("selected_trait_method_body_owner_unavailable"),
    };
    StaticFact::Known {
        value: ResolvedCallableTraitMethodBody {
            definition: super::stable_def_path_identity(tcx, definition),
            defining_owner: super::stable_def_path_identity(tcx, defining_owner),
            defining_owner_source_attestation,
            arguments: generic_substitutions(
                tcx,
                definition,
                instance.args,
                caller.to_def_id(),
                false,
            ),
            source_attestation: super::callable_source_attestation::attest_definition(
                tcx,
                definition,
                source_kind,
            ),
        },
    }
}

fn generic_substitutions<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    args: GenericArgsRef<'tcx>,
    binding_item: DefId,
    allow_parameters: bool,
) -> StaticFact<Vec<ResolvedCallableGenericSubstitution>> {
    let generics = tcx.generics_of(def_id);
    if generics.count() != args.len() {
        return unknown("generic_argument_count_mismatch");
    }
    if args.len() > MAX_PROOF_GENERIC_ARGS {
        return unknown("generic_argument_count_limit");
    }
    let mut substitutions = Vec::with_capacity(args.len());
    for (index, argument) in args.iter().enumerate() {
        if index >= generics.count() {
            return unknown("generic_parameter_identity_unavailable");
        }
        let parameter = generics.param_at(index, tcx);
        substitutions.push(ResolvedCallableGenericSubstitution {
            parameter: super::stable_def_path_identity(tcx, parameter.def_id),
            parameter_kind: match parameter.kind {
                ty::GenericParamDefKind::Lifetime => ResolvedCallableGenericParameterKind::Lifetime,
                ty::GenericParamDefKind::Type { .. } => ResolvedCallableGenericParameterKind::Type,
                ty::GenericParamDefKind::Const { .. } => {
                    ResolvedCallableGenericParameterKind::Const
                }
            },
            argument: generic_argument_identity(tcx, argument, binding_item, allow_parameters, 0),
        });
    }
    StaticFact::Known {
        value: substitutions,
    }
}

fn trait_ref_identity<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_ref: ty::TraitRef<'tcx>,
    binding_item: DefId,
    allow_parameters: bool,
    depth: u8,
) -> StaticFact<ResolvedCallableTraitRefIdentity> {
    if depth > MAX_PROOF_IDENTITY_DEPTH {
        return unknown("trait_ref_nesting_limit");
    }
    if trait_ref.args.len() > MAX_PROOF_GENERIC_ARGS {
        return unknown("trait_ref_argument_count_limit");
    }
    StaticFact::Known {
        value: ResolvedCallableTraitRefIdentity {
            definition: super::stable_def_path_identity(tcx, trait_ref.def_id),
            arguments: trait_ref
                .args
                .iter()
                .map(|argument| {
                    generic_argument_identity(
                        tcx,
                        argument,
                        binding_item,
                        allow_parameters,
                        depth + 1,
                    )
                })
                .collect(),
        },
    }
}

fn generic_argument_identity<'tcx>(
    tcx: TyCtxt<'tcx>,
    argument: ty::GenericArg<'tcx>,
    binding_item: DefId,
    allow_parameters: bool,
    depth: u8,
) -> StaticFact<ResolvedCallableGenericArgumentIdentity> {
    if depth > MAX_PROOF_IDENTITY_DEPTH {
        return unknown("generic_argument_nesting_limit");
    }
    if argument.references_error()
        || argument.has_infer()
        || argument.has_placeholders()
        || argument.has_escaping_bound_vars()
        || (!allow_parameters && argument.has_param())
    {
        return unknown("generic_argument_not_resolved_for_identity");
    }
    match argument.kind() {
        ty::GenericArgKind::Type(value) => {
            generic_type_identity(tcx, value, binding_item, allow_parameters, depth + 1)
                .map_type_argument()
        }
        ty::GenericArgKind::Lifetime(value) => {
            region_identity(tcx, value, binding_item).map_lifetime_argument()
        }
        ty::GenericArgKind::Const(value) => StaticFact::Known {
            value: ResolvedCallableGenericArgumentIdentity::Const {
                value: const_identity(tcx, value, binding_item, allow_parameters, depth + 1),
            },
        },
    }
}

fn generic_type_identity<'tcx>(
    tcx: TyCtxt<'tcx>,
    value: ty::Ty<'tcx>,
    binding_item: DefId,
    allow_parameters: bool,
    depth: u8,
) -> StaticFact<ResolvedCallableGenericTypeIdentity> {
    if depth > MAX_PROOF_IDENTITY_DEPTH {
        return unknown("generic_type_nesting_limit");
    }
    if value.references_error()
        || value.has_infer()
        || value.has_placeholders()
        || value.has_escaping_bound_vars()
        || (!allow_parameters && value.has_param())
    {
        return unknown("generic_type_not_resolved_for_identity");
    }
    use ty::TyKind;
    match *value.kind() {
        TyKind::Bool => primitive(ResolvedPrimitive::Bool),
        TyKind::Char => primitive(ResolvedPrimitive::Char),
        TyKind::Int(ty::IntTy::I8) => primitive(ResolvedPrimitive::I8),
        TyKind::Int(ty::IntTy::I16) => primitive(ResolvedPrimitive::I16),
        TyKind::Int(ty::IntTy::I32) => primitive(ResolvedPrimitive::I32),
        TyKind::Int(ty::IntTy::I64) => primitive(ResolvedPrimitive::I64),
        TyKind::Int(ty::IntTy::I128) => primitive(ResolvedPrimitive::I128),
        TyKind::Int(ty::IntTy::Isize) => primitive(ResolvedPrimitive::Isize),
        TyKind::Uint(ty::UintTy::U8) => primitive(ResolvedPrimitive::U8),
        TyKind::Uint(ty::UintTy::U16) => primitive(ResolvedPrimitive::U16),
        TyKind::Uint(ty::UintTy::U32) => primitive(ResolvedPrimitive::U32),
        TyKind::Uint(ty::UintTy::U64) => primitive(ResolvedPrimitive::U64),
        TyKind::Uint(ty::UintTy::U128) => primitive(ResolvedPrimitive::U128),
        TyKind::Uint(ty::UintTy::Usize) => primitive(ResolvedPrimitive::Usize),
        TyKind::Float(ty::FloatTy::F16) => primitive(ResolvedPrimitive::F16),
        TyKind::Float(ty::FloatTy::F32) => primitive(ResolvedPrimitive::F32),
        TyKind::Float(ty::FloatTy::F64) => primitive(ResolvedPrimitive::F64),
        TyKind::Float(ty::FloatTy::F128) => primitive(ResolvedPrimitive::F128),
        TyKind::Str => primitive(ResolvedPrimitive::Str),
        TyKind::Never => primitive(ResolvedPrimitive::Never),
        TyKind::Tuple(types) => {
            if types.len() > MAX_PROOF_TUPLE_ARITY {
                return unknown("tuple_arity_limit");
            }
            let mut elements = Vec::with_capacity(types.len());
            for element in types.iter() {
                let StaticFact::Known { value } =
                    generic_type_identity(tcx, element, binding_item, allow_parameters, depth + 1)
                else {
                    return unknown("tuple_element_identity_unavailable");
                };
                elements.push(value);
            }
            StaticFact::Known {
                value: ResolvedCallableGenericTypeIdentity::Tuple { elements },
            }
        }
        TyKind::Ref(region, referent, mutability) => {
            let StaticFact::Known { value: region } = region_identity(tcx, region, binding_item)
            else {
                return unknown("reference_region_identity_unavailable");
            };
            let StaticFact::Known { value: referent } =
                generic_type_identity(tcx, referent, binding_item, allow_parameters, depth + 1)
            else {
                return unknown("reference_referent_identity_unavailable");
            };
            StaticFact::Known {
                value: ResolvedCallableGenericTypeIdentity::Reference {
                    mutability: match mutability {
                        ty::Mutability::Not => ResolvedReferenceMutability::Shared,
                        ty::Mutability::Mut => ResolvedReferenceMutability::Mutable,
                    },
                    region,
                    referent: Box::new(referent),
                },
            }
        }
        TyKind::Adt(definition, args) => StaticFact::Known {
            // This identity is intentionally separate from the narrower legacy type identity.
            // Keep arbitrary nominal ADTs, while bounding serialization and traversal cost.
            value: if args.len() > MAX_PROOF_GENERIC_ARGS {
                return unknown("adt_generic_argument_count_limit");
            } else {
                ResolvedCallableGenericTypeIdentity::Adt {
                    definition: super::stable_def_path_identity(tcx, definition.did()),
                    source_attestation: Some(
                        super::callable_source_attestation::attest_definition(
                            tcx,
                            definition.did(),
                            super::DefinitionSourceKind::NominalType,
                        ),
                    ),
                    arguments: args
                        .iter()
                        .map(|argument| {
                            generic_argument_identity(
                                tcx,
                                argument,
                                binding_item,
                                allow_parameters,
                                depth + 1,
                            )
                        })
                        .collect(),
                }
            },
        },
        TyKind::Param(parameter) if allow_parameters => {
            let Some(parameter) = parameter_identity(tcx, binding_item, parameter.index) else {
                return unknown("type_parameter_identity_unavailable");
            };
            StaticFact::Known {
                value: ResolvedCallableGenericTypeIdentity::Parameter { parameter },
            }
        }
        _ => unknown("unsupported_generic_type_identity"),
    }
}

fn const_identity<'tcx>(
    tcx: TyCtxt<'tcx>,
    value: ty::Const<'tcx>,
    binding_item: DefId,
    allow_parameters: bool,
    depth: u8,
) -> StaticFact<ResolvedCallableConstIdentity> {
    if depth > MAX_PROOF_IDENTITY_DEPTH {
        return unknown("generic_const_nesting_limit");
    }
    match value.kind() {
        ty::ConstKind::Value(value) => {
            let typing_env = ty::TypingEnv::fully_monomorphized();
            let Some(bits) = value.try_to_bits(tcx, typing_env) else {
                return unknown("generic_const_not_scalar");
            };
            let StaticFact::Known { value: ty } =
                generic_type_identity(tcx, value.ty, binding_item, allow_parameters, depth + 1)
            else {
                return unknown("generic_const_type_identity_unavailable");
            };
            StaticFact::Known {
                value: ResolvedCallableConstIdentity::ScalarValue {
                    ty: Box::new(ty),
                    scalar_bits_hex: format!("{bits:032x}"),
                },
            }
        }
        ty::ConstKind::Param(parameter) if allow_parameters => {
            let parameter_index = parameter.index;
            let Some(parameter) = parameter_identity(tcx, binding_item, parameter_index) else {
                return unknown("const_parameter_identity_unavailable");
            };
            let Some(parameter_def_id) = parameter_def_id(tcx, binding_item, parameter_index)
            else {
                return unknown("const_parameter_type_identity_unavailable");
            };
            let parameter_ty = tcx
                .type_of(parameter_def_id)
                .instantiate_identity()
                .skip_norm_wip();
            let StaticFact::Known {
                value: parameter_ty,
            } = generic_type_identity(tcx, parameter_ty, binding_item, true, depth + 1)
            else {
                return unknown("const_parameter_type_identity_unavailable");
            };
            StaticFact::Known {
                value: ResolvedCallableConstIdentity::Parameter {
                    parameter,
                    ty: Box::new(parameter_ty),
                },
            }
        }
        _ => unknown("generic_const_value_unavailable"),
    }
}

fn region_identity<'tcx>(
    tcx: TyCtxt<'tcx>,
    region: ty::Region<'tcx>,
    binding_item: DefId,
) -> StaticFact<ResolvedRegionIdentity> {
    let identity = match region.kind() {
        ty::ReStatic => ResolvedRegionIdentity::Static,
        ty::ReErased => ResolvedRegionIdentity::Erased,
        ty::ReEarlyParam(_) => {
            let Some(parameter) = region.opt_param_def_id(tcx, binding_item) else {
                return unknown("early_region_parameter_identity_unavailable");
            };
            ResolvedRegionIdentity::EarlyBound {
                parameter: super::stable_def_path_identity(tcx, parameter),
            }
        }
        ty::ReBound(ty::BoundVarIndexKind::Bound(debruijn), bound) => {
            let region_kind = match bound.kind {
                ty::BoundRegionKind::Anon | ty::BoundRegionKind::NamedForPrinting(_) => {
                    super::ResolvedBoundRegionKind::Anonymous
                }
                ty::BoundRegionKind::Named(parameter) => super::ResolvedBoundRegionKind::Named {
                    parameter: super::stable_def_path_identity(tcx, parameter),
                },
                ty::BoundRegionKind::ClosureEnv => {
                    super::ResolvedBoundRegionKind::ClosureEnvironment
                }
            };
            ResolvedRegionIdentity::Bound {
                debruijn: debruijn.as_u32(),
                variable: bound.var.as_u32(),
                region_kind,
            }
        }
        ty::ReBound(ty::BoundVarIndexKind::Canonical, _) => {
            return unknown("canonical_bound_region_identity_unavailable");
        }
        ty::ReLateParam(late) => {
            let region_kind = match late.kind {
                ty::LateParamRegionKind::Anon(index)
                | ty::LateParamRegionKind::NamedAnon(index, _) => {
                    super::ResolvedLateRegionKind::Anonymous { index }
                }
                ty::LateParamRegionKind::Named(parameter) => super::ResolvedLateRegionKind::Named {
                    parameter: super::stable_def_path_identity(tcx, parameter),
                },
                ty::LateParamRegionKind::ClosureEnv => {
                    super::ResolvedLateRegionKind::ClosureEnvironment
                }
            };
            ResolvedRegionIdentity::LateParam {
                scope: super::stable_def_path_identity(tcx, late.scope),
                region_kind,
            }
        }
        ty::ReVar(_) => return unknown("unresolved_region_inference"),
        ty::RePlaceholder(_) => return unknown("placeholder_region_identity_unavailable"),
        ty::ReError(_) => return unknown("region_error"),
    };
    StaticFact::Known { value: identity }
}

fn parameter_identity(
    tcx: TyCtxt<'_>,
    binding_item: DefId,
    index: u32,
) -> Option<StableDefPathIdentity> {
    let parameter = parameter_def(tcx, binding_item, index)?;
    Some(super::stable_def_path_identity(tcx, parameter.def_id))
}

fn parameter_def_id(tcx: TyCtxt<'_>, binding_item: DefId, index: u32) -> Option<DefId> {
    Some(parameter_def(tcx, binding_item, index)?.def_id)
}

fn parameter_def<'tcx>(
    tcx: TyCtxt<'tcx>,
    binding_item: DefId,
    index: u32,
) -> Option<&'tcx ty::GenericParamDef> {
    let generics = tcx.generics_of(binding_item);
    let index = usize::try_from(index).ok()?;
    (index < generics.count()).then(|| generics.param_at(index, tcx))
}

fn primitive(primitive: ResolvedPrimitive) -> StaticFact<ResolvedCallableGenericTypeIdentity> {
    StaticFact::Known {
        value: ResolvedCallableGenericTypeIdentity::Primitive { primitive },
    }
}

fn polarity(polarity: ty::ClausePolarity) -> ResolvedCallableTraitPolarity {
    match polarity {
        ty::ClausePolarity::Positive => ResolvedCallableTraitPolarity::Positive,
        ty::ClausePolarity::Negative => ResolvedCallableTraitPolarity::Negative,
    }
}

fn trait_ref_identity_is_complete(
    trait_ref: &StaticFact<ResolvedCallableTraitRefIdentity>,
) -> bool {
    let StaticFact::Known { value } = trait_ref else {
        return false;
    };
    value
        .arguments
        .iter()
        .all(generic_argument_identity_is_complete)
}

fn generic_argument_identity_is_complete(
    argument: &StaticFact<ResolvedCallableGenericArgumentIdentity>,
) -> bool {
    let StaticFact::Known { value } = argument else {
        return false;
    };
    match value {
        ResolvedCallableGenericArgumentIdentity::Type { value } => type_identity_is_complete(value),
        ResolvedCallableGenericArgumentIdentity::Lifetime { .. } => true,
        ResolvedCallableGenericArgumentIdentity::Const { value } => {
            let StaticFact::Known { value } = value else {
                return false;
            };
            match value {
                ResolvedCallableConstIdentity::ScalarValue { ty, .. } => {
                    type_identity_is_complete(ty)
                }
                ResolvedCallableConstIdentity::Parameter { .. } => false,
            }
        }
    }
}

fn type_identity_is_complete(value: &ResolvedCallableGenericTypeIdentity) -> bool {
    match value {
        ResolvedCallableGenericTypeIdentity::Primitive { .. } => true,
        ResolvedCallableGenericTypeIdentity::Tuple { elements } => {
            elements.iter().all(type_identity_is_complete)
        }
        ResolvedCallableGenericTypeIdentity::Reference { referent, .. } => {
            type_identity_is_complete(referent)
        }
        ResolvedCallableGenericTypeIdentity::Adt { arguments, .. } => {
            arguments.iter().all(generic_argument_identity_is_complete)
        }
        ResolvedCallableGenericTypeIdentity::Parameter { .. } => false,
    }
}

fn fully_monomorphic<'tcx, T: TypeVisitableExt<TyCtxt<'tcx>>>(value: &T) -> bool {
    !value.has_param()
        && !value.has_infer()
        && !value.has_placeholders()
        && !value.references_error()
        && !value.has_escaping_bound_vars()
}

fn unknown<T>(why: &str) -> StaticFact<T> {
    StaticFact::Unknown {
        why: why.to_string(),
    }
}

trait StaticFactMap<T> {
    fn map_type_argument(self) -> StaticFact<ResolvedCallableGenericArgumentIdentity>;
    fn map_lifetime_argument(self) -> StaticFact<ResolvedCallableGenericArgumentIdentity>;
}

impl<T> StaticFactMap<T> for StaticFact<T>
where
    T: Into<ResolvedCallableGenericArgumentIdentity>,
{
    fn map_type_argument(self) -> StaticFact<ResolvedCallableGenericArgumentIdentity> {
        match self {
            StaticFact::Known { value } => StaticFact::Known {
                value: value.into(),
            },
            StaticFact::Unknown { why } => StaticFact::Unknown { why },
        }
    }

    fn map_lifetime_argument(self) -> StaticFact<ResolvedCallableGenericArgumentIdentity> {
        match self {
            StaticFact::Known { value } => StaticFact::Known {
                value: value.into(),
            },
            StaticFact::Unknown { why } => StaticFact::Unknown { why },
        }
    }
}

impl From<ResolvedCallableGenericTypeIdentity> for ResolvedCallableGenericArgumentIdentity {
    fn from(value: ResolvedCallableGenericTypeIdentity) -> Self {
        Self::Type {
            value: Box::new(value),
        }
    }
}

impl From<ResolvedRegionIdentity> for ResolvedCallableGenericArgumentIdentity {
    fn from(value: ResolvedRegionIdentity) -> Self {
        Self::Lifetime { value }
    }
}
