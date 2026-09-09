//! Declares [`crate::rustc_middle::arena::Arena`], which can allocate values of any
//! `Copy` type, and any `!Copy` type explicitly listed below.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_serialize::Decodable;

use crate::rustc_middle::ty::codec::{RefDecodable, TyDecoder};
use crate::rustc_middle::ty::{Ty, TyCtxt};

// If a type `T` supported by the arena also needs to support decoding into `&'tcx T`
// backed by an arena allocation (via `RefDecodable`), add it to the list in
// `impl_ref_decodable_into_arena!`.

crate::rustc_arena::declare_arena! {
    layout: crate::rustc_abi::LayoutData<crate::rustc_abi::FieldIdx, crate::rustc_abi::VariantIdx>,
    proxy_coroutine_layout: crate::rustc_middle::mir::CoroutineLayout<'tcx>,
    fn_abi: crate::rustc_target::callconv::FnAbi<'tcx, Ty<'tcx>>,
    adt_def: crate::rustc_middle::ty::AdtDefData,
    steal_thir: crate::rustc_data_structures::steal::Steal<crate::rustc_middle::thir::Thir<'tcx>>,
    steal_mir: crate::rustc_data_structures::steal::Steal<crate::rustc_middle::mir::Body<'tcx>>,
    mir: crate::rustc_middle::mir::Body<'tcx>,
    steal_promoted:
        crate::rustc_data_structures::steal::Steal<
            crate::rustc_index::IndexVec<
                crate::rustc_middle::mir::Promoted,
                crate::rustc_middle::mir::Body<'tcx>
            >
        >,
    promoted:
        crate::rustc_index::IndexVec<
            crate::rustc_middle::mir::Promoted,
            crate::rustc_middle::mir::Body<'tcx>
        >,
    typeck_results: crate::rustc_middle::ty::TypeckResults<'tcx>,
    borrowck_result:
        crate::rustc_data_structures::fx::FxIndexMap<
            crate::rustc_hir::def_id::LocalDefId,
            crate::rustc_middle::ty::DefinitionSiteHiddenType<'tcx>,
        >,
    resolver: crate::rustc_data_structures::steal::Steal<crate::rustc_middle::ty::ResolverAstLowering<'tcx>>,
    index_ast:
        crate::rustc_index::IndexVec<
            crate::rustc_span::def_id::LocalDefId,
            crate::rustc_data_structures::steal::Steal<(
                alloc::sync::Arc<crate::rustc_middle::ty::ResolverAstLowering<'tcx>>,
                crate::rustc_ast::AstOwner
            )>
        >,
    crate_alone: crate::rustc_data_structures::steal::Steal<crate::rustc_ast::Crate>,
    crate_for_resolver: crate::rustc_data_structures::steal::Steal<(crate::rustc_ast::Crate, crate::rustc_ast::AttrVec)>,
    resolutions: crate::rustc_middle::ty::ResolverGlobalCtxt,
    const_allocs: crate::rustc_middle::mir::interpret::Allocation,
    region_scope_tree: crate::rustc_middle::middle::region::ScopeTree,
    // Required for the incremental on-disk cache
    mir_keys: crate::rustc_hir::def_id::DefIdSet,
    dropck_outlives:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx,
                crate::rustc_middle::traits::query::DropckOutlivesResult<'tcx>
            >
        >,
    normalize_canonicalized_projection:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx,
                crate::rustc_middle::traits::query::NormalizationResult<'tcx>
            >
        >,
    implied_outlives_bounds:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx,
                Vec<crate::rustc_middle::traits::query::OutlivesBound<'tcx>>
            >
        >,
    mir_borrowck_implied_outlives_bounds:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx,
                crate::rustc_middle::traits::query::MirBorrowckImpliedOutlivesBounds<'tcx>
            >
        >,
    dtorck_constraint: crate::rustc_middle::traits::query::DropckConstraint<'tcx>,
    candidate_step: crate::rustc_middle::traits::query::CandidateStep<'tcx>,
    autoderef_bad_ty: crate::rustc_middle::traits::query::MethodAutoderefBadTy<'tcx>,
    query_region_constraints: crate::rustc_middle::infer::canonical::QueryRegionConstraints<'tcx>,
    type_op_subtype:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx, ()>
        >,
    type_op_normalize_poly_fn_sig:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx, crate::rustc_middle::ty::PolyFnSig<'tcx>>
        >,
    type_op_normalize_fn_sig:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx, crate::rustc_middle::ty::FnSig<'tcx>>
        >,
    type_op_normalize_clause:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx, crate::rustc_middle::ty::Clause<'tcx>>
        >,
    type_op_normalize_ty:
        crate::rustc_middle::infer::canonical::Canonical<'tcx,
            crate::rustc_middle::infer::canonical::QueryResponse<'tcx, Ty<'tcx>>
        >,
    inspect_probe: crate::rustc_middle::traits::solve::inspect::Probe<TyCtxt<'tcx>>,
    effective_visibilities: crate::rustc_middle::middle::privacy::EffectiveVisibilities,
    upvars_mentioned: crate::rustc_data_structures::fx::FxIndexMap<crate::rustc_hir::HirId, crate::rustc_hir::Upvar>,
    dyn_compatibility_violations: crate::rustc_middle::traits::DynCompatibilityViolation,
    codegen_unit: crate::rustc_middle::mono::CodegenUnit<'tcx>,
    attribute: crate::rustc_hir::Attribute,
    name_set: crate::rustc_data_structures::unord::UnordSet<crate::rustc_span::Symbol>,
    autodiff_item: crate::rustc_hir::attrs::AutoDiffItem,
    ordered_name_set: crate::rustc_data_structures::fx::FxIndexSet<crate::rustc_span::Symbol>,
    stable_order_of_exportable_impls:
        crate::rustc_data_structures::fx::FxIndexMap<crate::rustc_hir::def_id::DefId, usize>,

    // Note that this deliberately duplicates items in the `crate::rustc_hir::arena`,
    // since we need to allocate this type on both the `rustc_hir` arena
    // (during lowering) and the `rustc_middle` arena (for decoding MIR)
    asm_template: crate::rustc_ast::InlineAsmTemplatePiece,
    used_trait_imports: crate::rustc_data_structures::unord::UnordSet<crate::rustc_hir::def_id::LocalDefId>,
    is_late_bound_map: crate::rustc_data_structures::fx::FxIndexSet<crate::rustc_hir::ItemLocalId>,
    impl_source: crate::rustc_middle::traits::ImplSource<'tcx, ()>,

    dep_kind_vtable: crate::rustc_middle::dep_graph::DepKindVTable<'tcx>,

    trait_impl_trait_tys:
        crate::rustc_data_structures::unord::UnordMap<
            crate::rustc_hir::def_id::DefId,
            crate::rustc_middle::ty::EarlyBinder<'tcx, Ty<'tcx>>
        >,
    external_constraints: crate::rustc_middle::traits::solve::ExternalConstraintsData<TyCtxt<'tcx>>,
    doc_link_resolutions: crate::rustc_hir::def::DocLinkResMap,
    stripped_cfg_items: crate::rustc_hir::attrs::StrippedCfgItem,
    mod_child: crate::rustc_middle::metadata::ModChild,
    features: crate::rustc_feature::Features,
    specialization_graph: crate::rustc_middle::traits::specialization_graph::Graph,
    crate_inherent_impls: crate::rustc_middle::ty::CrateInherentImpls,
    hir_owner_nodes: crate::rustc_hir::OwnerNodes<'tcx>,
    token_stream: crate::rustc_ast::tokenstream::TokenStream,
    maybe_owner: crate::rustc_middle::hir::ProjectedMaybeOwner<'tcx>,
    owner_info: crate::rustc_middle::hir::ProjectedOwnerInfo<'tcx>,
    parenting: crate::rustc_hir::def_id::LocalDefIdMap<crate::rustc_hir::ItemLocalId>,
    trait_candidates: crate::rustc_hir::ItemLocalMap<&'tcx [crate::rustc_hir::TraitCandidate<'tcx>]>,
    delayed_lints: crate::rustc_data_structures::steal::Steal<crate::rustc_hir::lints::DelayedLints>,
}

#[inline]
fn decode_arena_allocatable<'tcx, D, C, T>(decoder: &mut D) -> &'tcx T
where
    D: TyDecoder<'tcx>,
    T: ArenaAllocatable<'tcx, C> + Decodable<D>,
{
    let value: T = Decodable::decode(decoder);
    decoder.interner().arena.alloc(value)
}

#[inline]
fn decode_arena_allocatable_slice<'tcx, D, C, T>(decoder: &mut D) -> &'tcx [T]
where
    D: TyDecoder<'tcx>,
    T: ArenaAllocatable<'tcx, C> + Decodable<D>,
{
    let values: Vec<T> = Decodable::decode(decoder);
    decoder.interner().arena.alloc_from_iter(values)
}

macro_rules! impl_ref_decodable_into_arena {
    (
        $(
            $ty:ty,
        )*
    ) => {
        $(
            impl<'tcx, D: TyDecoder<'tcx>> RefDecodable<'tcx, D> for $ty {
                #[inline]
                fn decode(decoder: &mut D) -> &'tcx Self {
                    decode_arena_allocatable(decoder)
                }
            }

            impl<'tcx, D: TyDecoder<'tcx>> RefDecodable<'tcx, D> for [$ty] {
                #[inline]
                fn decode(decoder: &mut D) -> &'tcx Self {
                    decode_arena_allocatable_slice(decoder)
                }
            }
        )*
    }
}

// For each of these types, implements `RefDecodable` for `T` (and `[T]`) by
// decoding to `T` and then moving the value or values into an arena allocation.
//
// Types in this list must be `ArenaAllocatable`, either because they are `Copy`
// or because they are listed in the `declare_arena!` invocation.
impl_ref_decodable_into_arena! {
    // tidy-alphabetical-start
    (crate::rustc_middle::middle::exported_symbols::ExportedSymbol<'tcx>, crate::rustc_middle::middle::exported_symbols::SymbolExportInfo),
    crate::rustc_ast::InlineAsmTemplatePiece,
    crate::rustc_ast::tokenstream::TokenStream,
    crate::rustc_data_structures::unord::UnordMap<crate::rustc_span::def_id::DefId, crate::rustc_middle::ty::EarlyBinder<'tcx, Ty<'tcx>>>,
    crate::rustc_data_structures::unord::UnordSet<crate::rustc_span::def_id::LocalDefId>,
    crate::rustc_hir::Attribute,
    crate::rustc_index::IndexVec<crate::rustc_middle::mir::Promoted, crate::rustc_middle::mir::Body<'tcx>>,
    crate::rustc_middle::middle::deduced_param_attrs::DeducedParamAttrs,
    crate::rustc_middle::mir::Body<'tcx>,
    crate::rustc_middle::traits::ImplSource<'tcx, ()>,
    crate::rustc_middle::traits::specialization_graph::Graph,
    crate::rustc_middle::ty::TypeckResults<'tcx>,
    crate::rustc_middle::ty::Variance,
    crate::rustc_span::Ident,
    crate::rustc_span::Span,
    crate::rustc_span::def_id::DefId,
    crate::rustc_span::def_id::LocalDefId,
    // tidy-alphabetical-end
}
