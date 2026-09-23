//! Defines the set of legal keys that can be used in queries.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::fmt::Debug;
use core::hash::Hash;

use crate::rustc_ast::tokenstream::TokenStream;
use crate::rustc_data_structures::sso::SsoHashSet;
use crate::rustc_data_structures::stable_hash::StableHash;
use crate::rustc_hir::OwnerId;
use crate::rustc_hir::def_id::{CrateNum, DefId, LOCAL_CRATE, LocalDefId, LocalModId};
use crate::rustc_span::def_id::ModId;
use crate::rustc_span::{DUMMY_SP, Ident, LocalExpnId, Span, Symbol};

use crate::rustc_middle::dep_graph::DepNodeIndex;
use crate::rustc_middle::infer::canonical::CanonicalQueryInput;
use crate::rustc_middle::mono::CollectionMode;
use crate::rustc_middle::query::{DefIdCache, DefaultCache, SingleCache, VecCache};
use crate::rustc_middle::ty::fast_reject::SimplifiedType;
use crate::rustc_middle::ty::layout::ValidityRequirement;
use crate::rustc_middle::ty::{self, GenericArg, GenericArgsRef, Ty, TyCtxt};
use crate::rustc_middle::{mir, traits};

/// Placeholder for `CrateNum`'s "local" counterpart
#[derive(Copy, Clone, Debug)]
pub struct LocalCrate;

// Upstream is a trait alias (`trait_alias`, unstable); a supertrait-only trait with a blanket
// impl is the stable equivalent, and its supertraits are elaborated the same way.
pub trait QueryKeyBounds: Copy + Debug + Eq + Hash + StableHash {}

impl<T: Copy + Debug + Eq + Hash + StableHash> QueryKeyBounds for T {}

/// Controls what types can legally be used as the key for a query.
pub trait QueryKey: Sized + QueryKeyBounds {
    /// The type of in-memory cache to use for queries with this key type.
    ///
    /// In practice the cache type must implement [`QueryCache`], though that
    /// constraint is not enforced here.
    ///
    /// [`QueryCache`]: crate::rustc_middle::query::QueryCache
    ///
    /// Stable Rust has no associated type defaults, so every impl names this type; the
    /// usual choice is `DefaultCache<Self, V>`.
    type Cache<V>;

    /// Stable Rust has no associated type defaults, so every impl names this type; keys
    /// with no local counterpart use `crate::Never`.
    type LocalQueryKey;

    /// In the event that a cycle occurs, if no explicit span has been
    /// given for a query with key `self`, what span should we use?
    fn default_span(&self, tcx: TyCtxt<'_>) -> Span;

    /// If the key is a [`DefId`] or `DefId`--equivalent, return that `DefId`.
    /// Otherwise, return `None`.
    fn key_as_def_id(&self) -> Option<DefId> {
        None
    }

    /// Given an instance of this key, what crate is it referring to?
    /// This is used to find the provider.
    fn as_local_key(&self) -> Option<Self::LocalQueryKey> {
        None
    }
}

impl QueryKey for () {
    type Cache<V> = SingleCache<V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for ty::ShimKind<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(self.def_id())
    }
}

impl<'tcx> QueryKey for ty::InstanceKind<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(self.def_id())
    }
}

impl<'tcx> QueryKey for ty::Instance<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(self.def_id())
    }
}

impl<'tcx> QueryKey for mir::interpret::GlobalId<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.instance.default_span(tcx)
    }
}

impl<'tcx> QueryKey for (Ty<'tcx>, Option<ty::ExistentialTraitRef<'tcx>>) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for ty::LitToConstInput<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl QueryKey for CrateNum {
    type Cache<V> = VecCache<Self, V, DepNodeIndex>;

    type LocalQueryKey = LocalCrate;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }

    #[inline(always)]
    fn as_local_key(&self) -> Option<Self::LocalQueryKey> {
        (*self == LOCAL_CRATE).then_some(LocalCrate)
    }
}

impl QueryKey for OwnerId {
    type Cache<V> = VecCache<Self, V, DepNodeIndex>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.to_def_id().default_span(tcx)
    }

    fn key_as_def_id(&self) -> Option<DefId> {
        Some(self.to_def_id())
    }
}

impl QueryKey for LocalDefId {
    type Cache<V> = VecCache<Self, V, DepNodeIndex>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.to_def_id().default_span(tcx)
    }

    fn key_as_def_id(&self) -> Option<DefId> {
        Some(self.to_def_id())
    }
}

impl QueryKey for DefId {
    type Cache<V> = DefIdCache<V>;
    type LocalQueryKey = LocalDefId;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(*self)
    }

    #[inline(always)]
    fn key_as_def_id(&self) -> Option<DefId> {
        Some(*self)
    }

    #[inline(always)]
    fn as_local_key(&self) -> Option<Self::LocalQueryKey> {
        self.as_local()
    }
}

impl QueryKey for ModId {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = LocalModId;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(self.to_def_id())
    }

    #[inline(always)]
    fn key_as_def_id(&self) -> Option<DefId> {
        Some(self.to_def_id())
    }

    #[inline(always)]
    fn as_local_key(&self) -> Option<Self::LocalQueryKey> {
        self.as_local()
    }
}

impl QueryKey for LocalModId {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(*self)
    }

    #[inline(always)]
    fn key_as_def_id(&self) -> Option<DefId> {
        Some(self.to_def_id())
    }
}

impl QueryKey for SimplifiedType {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl QueryKey for (DefId, DefId) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.1.default_span(tcx)
    }
}

impl QueryKey for (DefId, Ident) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(self.0)
    }

    #[inline(always)]
    fn key_as_def_id(&self) -> Option<DefId> {
        Some(self.0)
    }
}

impl QueryKey for (LocalDefId, LocalDefId, Ident) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.1.default_span(tcx)
    }
}

impl QueryKey for (CrateNum, DefId) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = DefId;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.1.default_span(tcx)
    }

    #[inline(always)]
    fn as_local_key(&self) -> Option<Self::LocalQueryKey> {
        (self.0 == LOCAL_CRATE).then(|| self.1)
    }
}

impl QueryKey for (CrateNum, SimplifiedType) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = SimplifiedType;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }

    #[inline(always)]
    fn as_local_key(&self) -> Option<Self::LocalQueryKey> {
        (self.0 == LOCAL_CRATE).then(|| self.1)
    }
}

impl QueryKey for (DefId, ty::SizedTraitKind) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.0.default_span(tcx)
    }
}

impl<'tcx> QueryKey for GenericArgsRef<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for (DefId, GenericArgsRef<'tcx>) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.0.default_span(tcx)
    }
}

impl<'tcx> QueryKey for ty::TraitRef<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        tcx.def_span(self.def_id)
    }
}

impl<'tcx> QueryKey for GenericArg<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for Ty<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        def_id_of_type(*self).map(|def_id| tcx.def_span(def_id)).unwrap_or(DUMMY_SP)
    }
}

impl<'tcx> QueryKey for (Ty<'tcx>, Ty<'tcx>) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for ty::Clauses<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for ty::AliasTyKind<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        let def_id = match self {
            ty::AliasTyKind::Projection { def_id }
            | ty::AliasTyKind::Inherent { def_id }
            | ty::AliasTyKind::Opaque { def_id }
            | ty::AliasTyKind::Free { def_id } => def_id,
        };
        tcx.def_span(*def_id)
    }
}

impl<'tcx, T: QueryKey> QueryKey for ty::PseudoCanonicalInput<'tcx, T> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.value.default_span(tcx)
    }
}

impl QueryKey for Symbol {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl QueryKey for Option<Symbol> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for &'tcx [u8] {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

/// Canonical query goals correspond to abstract trait operations that
/// are not tied to any crate in particular.
impl<'tcx, T: QueryKeyBounds> QueryKey for CanonicalQueryInput<'tcx, T> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx, T: QueryKeyBounds> QueryKey for (CanonicalQueryInput<'tcx, T>, bool) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx, T: QueryKeyBounds> QueryKey for (CanonicalQueryInput<'tcx, T>, usize) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for (Ty<'tcx>, crate::rustc_abi::VariantIdx) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for (ty::Predicate<'tcx>, traits::WellFormedLoc) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for (ty::PolyFnSig<'tcx>, &'tcx ty::List<Ty<'tcx>>) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for (ty::Instance<'tcx>, &'tcx ty::List<Ty<'tcx>>) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.0.default_span(tcx)
    }
}

impl<'tcx> QueryKey for ty::Value<'tcx> {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for (LocalExpnId, &'tcx TokenStream) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, _tcx: TyCtxt<'_>) -> Span {
        self.0.expn_data().call_site
    }
}

impl<'tcx> QueryKey for (ValidityRequirement, ty::PseudoCanonicalInput<'tcx, Ty<'tcx>>) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    // Just forward to `Ty<'tcx>`

    fn default_span(&self, _: TyCtxt<'_>) -> Span {
        DUMMY_SP
    }
}

impl<'tcx> QueryKey for (ty::Instance<'tcx>, CollectionMode) {
    type Cache<V> = DefaultCache<Self, V>;
    type LocalQueryKey = crate::Never;

    fn default_span(&self, tcx: TyCtxt<'_>) -> Span {
        self.0.default_span(tcx)
    }
}

/// Gets a `DefId` associated with a type
///
/// Visited set is needed to avoid full iteration over
/// deeply nested tuples that have no DefId.
fn def_id_of_type_cached<'a>(ty: Ty<'a>, visited: &mut SsoHashSet<Ty<'a>>) -> Option<DefId> {
    match *ty.kind() {
        ty::Adt(adt_def, _) => Some(adt_def.did()),

        ty::Dynamic(data, ..) => data.principal_def_id(),

        ty::Pat(subty, _) | ty::Array(subty, _) | ty::Slice(subty) => {
            def_id_of_type_cached(subty, visited)
        }

        ty::RawPtr(ty, _) => def_id_of_type_cached(ty, visited),

        ty::Ref(_, ty, _) => def_id_of_type_cached(ty, visited),

        ty::Tuple(tys) => tys.iter().find_map(|ty| {
            if visited.insert(ty) {
                return def_id_of_type_cached(ty, visited);
            }
            return None;
        }),

        ty::FnDef(def_id, _)
        | ty::Closure(def_id, _)
        | ty::CoroutineClosure(def_id, _)
        | ty::Coroutine(def_id, _)
        | ty::CoroutineWitness(def_id, _)
        | ty::Foreign(def_id) => Some(def_id),

        ty::Alias(_, alias) => match alias.kind {
            ty::AliasTyKind::Projection { def_id }
            | ty::AliasTyKind::Inherent { def_id }
            | ty::AliasTyKind::Opaque { def_id }
            | ty::AliasTyKind::Free { def_id } => Some(def_id),
        },

        ty::Bool
        | ty::Char
        | ty::Int(_)
        | ty::Uint(_)
        | ty::Str
        | ty::FnPtr(..)
        | ty::UnsafeBinder(_)
        | ty::Placeholder(..)
        | ty::Param(_)
        | ty::Infer(_)
        | ty::Bound(..)
        | ty::Error(_)
        | ty::Never
        | ty::Float(_) => None,
    }
}

fn def_id_of_type(ty: Ty<'_>) -> Option<DefId> {
    def_id_of_type_cached(ty, &mut SsoHashSet::new())
}
