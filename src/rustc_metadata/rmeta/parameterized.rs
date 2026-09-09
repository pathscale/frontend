// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::hash::Hash;

use crate::rustc_data_structures::unord::UnordMap;
use crate::rustc_hir::def_id::DefIndex;
use crate::rustc_index::{Idx, IndexVec};
use crate::rustc_middle::ty::{Binder, EarlyBinder, GenericArg, Region};
use crate::rustc_span::Symbol;

use crate::rustc_metadata::rmeta::{LazyArray, LazyValue};

pub(crate) trait ParameterizedOverTcx: 'static {
    type Value<'tcx>;
}

impl<T: ParameterizedOverTcx> ParameterizedOverTcx for Option<T> {
    type Value<'tcx> = Option<T::Value<'tcx>>;
}

impl<A: ParameterizedOverTcx, B: ParameterizedOverTcx> ParameterizedOverTcx for (A, B) {
    type Value<'tcx> = (A::Value<'tcx>, B::Value<'tcx>);
}

impl<T: ParameterizedOverTcx> ParameterizedOverTcx for Vec<T> {
    type Value<'tcx> = Vec<T::Value<'tcx>>;
}

impl<I: Idx + 'static, T: ParameterizedOverTcx> ParameterizedOverTcx for IndexVec<I, T> {
    type Value<'tcx> = IndexVec<I, T::Value<'tcx>>;
}

impl<I: Hash + Eq + 'static, T: ParameterizedOverTcx> ParameterizedOverTcx for UnordMap<I, T> {
    type Value<'tcx> = UnordMap<I, T::Value<'tcx>>;
}

impl<T: ParameterizedOverTcx> ParameterizedOverTcx for Binder<'static, T> {
    type Value<'tcx> = Binder<'tcx, T::Value<'tcx>>;
}

impl<T: ParameterizedOverTcx> ParameterizedOverTcx for EarlyBinder<'static, T> {
    type Value<'tcx> = EarlyBinder<'tcx, T::Value<'tcx>>;
}

impl<T: ParameterizedOverTcx> ParameterizedOverTcx for LazyValue<T> {
    type Value<'tcx> = LazyValue<T::Value<'tcx>>;
}

impl<T: ParameterizedOverTcx> ParameterizedOverTcx for LazyArray<T> {
    type Value<'tcx> = LazyArray<T::Value<'tcx>>;
}

impl ParameterizedOverTcx for Region<'static> {
    type Value<'tcx> = Region<'tcx>;
}

impl ParameterizedOverTcx for GenericArg<'static> {
    type Value<'tcx> = GenericArg<'tcx>;
}

macro_rules! trivially_parameterized_over_tcx {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl ParameterizedOverTcx for $ty {
                #[allow(unused_lifetimes)]
                type Value<'tcx> = $ty;
            }
        )*
    }
}

trivially_parameterized_over_tcx! {
    bool,
    u32,
    u64,
    usize,
    alloc::string::String,
    // tidy-alphabetical-start
    crate::rustc_metadata::rmeta::AttrFlags,
    crate::rustc_metadata::rmeta::CrateDep,
    crate::rustc_metadata::rmeta::CrateHeader,
    crate::rustc_metadata::rmeta::CrateRoot,
    crate::rustc_metadata::rmeta::IncoherentImpls,
    crate::rustc_metadata::rmeta::ProcMacroKind,
    crate::rustc_metadata::rmeta::RawDefId,
    crate::rustc_metadata::rmeta::TraitImpls,
    crate::rustc_metadata::rmeta::VariantData,
    crate::rustc_abi::ReprOptions,
    crate::rustc_ast::DelimArgs,
    crate::rustc_crate_store::ForeignModule,
    crate::rustc_crate_store::LinkagePreference,
    crate::rustc_crate_store::NativeLib,
    crate::rustc_hir::Attribute,
    crate::rustc_hir::ConstStability,
    crate::rustc_hir::Constness,
    crate::rustc_hir::CoroutineKind,
    crate::rustc_hir::DefaultBodyStability,
    crate::rustc_hir::Defaultness,
    crate::rustc_hir::OpaqueTyOrigin<crate::rustc_hir::def_id::DefId>,
    crate::rustc_hir::PreciseCapturingArgKind<Symbol, Symbol>,
    crate::rustc_hir::Safety,
    crate::rustc_hir::Stability,
    crate::rustc_hir::attrs::Deprecation,
    crate::rustc_hir::attrs::EiiDecl,
    crate::rustc_hir::attrs::EiiImpl,
    crate::rustc_hir::attrs::StrippedCfgItem<crate::rustc_hir::def_id::DefIndex>,
    crate::rustc_hir::attrs::lang_items::LangItem,
    crate::rustc_hir::def::DefKind,
    crate::rustc_hir::def::DocLinkResMap,
    crate::rustc_hir::def_id::DefId,
    crate::rustc_hir::def_id::DefIndex,
    crate::rustc_hir::definitions::DefKey,
    crate::rustc_index::bit_set::DenseBitSet<u32>,
    crate::rustc_middle::metadata::AmbigModChild,
    crate::rustc_middle::metadata::ModChild,
    crate::rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrs,
    crate::rustc_middle::middle::debugger_visualizer::DebuggerVisualizerFile,
    crate::rustc_middle::middle::deduced_param_attrs::DeducedParamAttrs,
    crate::rustc_middle::middle::exported_symbols::SymbolExportInfo,
    crate::rustc_middle::middle::lib_features::FeatureStability,
    crate::rustc_middle::middle::resolve_bound_vars::ObjectLifetimeDefault,
    crate::rustc_middle::mir::ConstQualifs,
    crate::rustc_middle::mir::ConstValue,
    crate::rustc_middle::ty::AnonConstKind,
    crate::rustc_middle::ty::AssocContainer,
    crate::rustc_middle::ty::AsyncDestructor,
    crate::rustc_middle::ty::Asyncness,
    crate::rustc_middle::ty::Destructor,
    crate::rustc_middle::ty::Generics,
    crate::rustc_middle::ty::ImplTraitInTraitData,
    crate::rustc_middle::ty::IntrinsicDef,
    crate::rustc_middle::ty::RestrictionKind,
    crate::rustc_middle::ty::TraitDef,
    crate::rustc_middle::ty::Variance,
    crate::rustc_middle::ty::Visibility<DefIndex>,
    crate::rustc_middle::ty::adjustment::CoerceUnsizedInfo,
    crate::rustc_middle::ty::fast_reject::SimplifiedType,
    crate::rustc_session::config::TargetModifier,
    crate::rustc_session::config::mitigation_coverage::DeniedPartialMitigation,
    crate::rustc_span::ExpnData,
    crate::rustc_span::ExpnHash,
    crate::rustc_span::ExpnId,
    crate::rustc_span::Ident,
    crate::rustc_span::SourceFile,
    crate::rustc_span::Span,
    crate::rustc_span::Symbol,
    crate::rustc_span::hygiene::SyntaxContextKey,
    // tidy-alphabetical-end
}

// HACK(compiler-errors): This macro rule can only take a fake path,
// not a real, due to parsing ambiguity reasons.
macro_rules! parameterized_over_tcx {
    ($($( $fake_path:ident )::+ ),+ $(,)?) => {
        $(
            impl ParameterizedOverTcx for $( $fake_path )::+ <'static> {
                type Value<'tcx> = $( $fake_path )::+ <'tcx>;
            }
        )*
    }
}

parameterized_over_tcx! {
    // tidy-alphabetical-start
    crate::rustc_metadata::rmeta::DefPathHashMapRef,
    crate::rustc_middle::middle::exported_symbols::ExportedSymbol,
    crate::rustc_middle::mir::Body,
    crate::rustc_middle::mir::CoroutineLayout,
    crate::rustc_middle::mir::interpret::ConstAllocation,
    crate::rustc_middle::ty::Clause,
    crate::rustc_middle::ty::Const,
    crate::rustc_middle::ty::ConstConditions,
    crate::rustc_middle::ty::FnSig,
    crate::rustc_middle::ty::GenericClauses,
    crate::rustc_middle::ty::ImplTraitHeader,
    crate::rustc_middle::ty::TraitRef,
    crate::rustc_middle::ty::Ty,
    // tidy-alphabetical-end
}
