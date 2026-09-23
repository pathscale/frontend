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

use crate::rustc_data_structures::fingerprint::Fingerprint;
use crate::rustc_data_structures::stable_hash::{StableHash, StableHasher};
use crate::rustc_hir::def_id::{CrateNum, DefId, LOCAL_CRATE, LocalDefId, LocalModId, ModId};
use crate::rustc_hir::definitions::DefPathHash;
use crate::rustc_hir::{HirId, ItemLocalId, OwnerId};

use crate::rustc_middle::dep_graph::{DepNode, KeyFingerprintStyle};
use crate::rustc_middle::ty::TyCtxt;

/// Trait for query keys as seen by dependency-node tracking.
///
/// The defaults are the opaque behaviour: hash the key with `StableHash`, and never
/// recover it. They used to live in a blanket `impl<T: StableHash + Debug>` that the
/// impls below specialized. Stable Rust cannot specialize, so the defaults moved into
/// the trait and every opaque key type opts in with an empty impl (listed after the
/// specialized ones). A key type missing from that list is a compile error at its
/// query, not a silent change of fingerprint.
pub trait DepNodeKey<'tcx>: Debug + Sized + StableHash {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::Opaque
    }

    /// This method turns a query key into an opaque `Fingerprint` to be used
    /// in `DepNode`.
    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        tcx.with_stable_hashing_context(|mut hcx| {
            let mut hasher = StableHasher::new();
            self.stable_hash(&mut hcx, &mut hasher);
            hasher.finish()
        })
    }

    /// This method tries to recover the query key from the given `DepNode`,
    /// something which is needed when forcing `DepNode`s during red-green
    /// evaluation. The query system will only call this method if
    /// `fingerprint_style()` is not `FingerprintStyle::Opaque`.
    /// It is always valid to return `None` here, in which case incremental
    /// compilation will treat the query as having changed instead of forcing it.
    #[inline(always)]
    fn try_recover_key(_: TyCtxt<'tcx>, _: &DepNode) -> Option<Self> {
        None
    }
}

impl<'tcx> DepNodeKey<'tcx> for () {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::Unit
    }

    #[inline(always)]
    fn to_fingerprint(&self, _: TyCtxt<'tcx>) -> Fingerprint {
        Fingerprint::ZERO
    }

    #[inline(always)]
    fn try_recover_key(_: TyCtxt<'tcx>, _: &DepNode) -> Option<Self> {
        Some(())
    }
}

impl<'tcx> DepNodeKey<'tcx> for DefId {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::DefPathHash
    }

    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        tcx.def_path_hash(*self).0
    }

    #[inline(always)]
    fn try_recover_key(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> Option<Self> {
        dep_node.extract_def_id(tcx)
    }
}

impl<'tcx> DepNodeKey<'tcx> for LocalDefId {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::DefPathHash
    }

    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        self.to_def_id().to_fingerprint(tcx)
    }

    #[inline(always)]
    fn try_recover_key(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> Option<Self> {
        dep_node.extract_def_id(tcx).map(|id| id.expect_local())
    }
}

impl<'tcx> DepNodeKey<'tcx> for OwnerId {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::DefPathHash
    }

    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        self.to_def_id().to_fingerprint(tcx)
    }

    #[inline(always)]
    fn try_recover_key(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> Option<Self> {
        dep_node.extract_def_id(tcx).map(|id| OwnerId { def_id: id.expect_local() })
    }
}

impl<'tcx> DepNodeKey<'tcx> for CrateNum {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::DefPathHash
    }

    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        let def_id = self.as_def_id();
        def_id.to_fingerprint(tcx)
    }

    #[inline(always)]
    fn try_recover_key(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> Option<Self> {
        dep_node.extract_def_id(tcx).map(|id| id.krate)
    }
}

impl<'tcx> DepNodeKey<'tcx> for (DefId, DefId) {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::Opaque
    }

    // We actually would not need to specialize the implementation of this
    // method but it's faster to combine the hashes than to instantiate a full
    // hashing context and stable-hashing state.
    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        let (def_id_0, def_id_1) = *self;

        let def_path_hash_0 = tcx.def_path_hash(def_id_0);
        let def_path_hash_1 = tcx.def_path_hash(def_id_1);

        def_path_hash_0.0.combine(def_path_hash_1.0)
    }
}

impl<'tcx> DepNodeKey<'tcx> for HirId {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::HirId
    }

    // We actually would not need to specialize the implementation of this
    // method but it's faster to combine the hashes than to instantiate a full
    // hashing context and stable-hashing state.
    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        let HirId { owner, local_id } = *self;
        let def_path_hash = tcx.def_path_hash(owner.to_def_id());
        Fingerprint::new(
            // `owner` is local, so is completely defined by the local hash
            def_path_hash.local_hash(),
            local_id.as_u32() as u64,
        )
    }

    #[inline(always)]
    fn try_recover_key(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> Option<Self> {
        if tcx.key_fingerprint_style(dep_node.kind) == KeyFingerprintStyle::HirId {
            let (local_hash, local_id) = Fingerprint::from(dep_node.key_fingerprint).split();
            let def_path_hash = DefPathHash::new(tcx.stable_crate_id(LOCAL_CRATE), local_hash);
            let def_id = tcx.def_path_hash_to_def_id(def_path_hash)?.expect_local();
            let local_id = local_id
                .as_u64()
                .try_into()
                .unwrap_or_else(|_| panic!("local id should be u32, found {local_id:?}"));
            Some(HirId { owner: OwnerId { def_id }, local_id: ItemLocalId::from_u32(local_id) })
        } else {
            None
        }
    }
}

impl<'tcx> DepNodeKey<'tcx> for ModId {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::DefPathHash
    }

    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        self.to_def_id().to_fingerprint(tcx)
    }

    #[inline(always)]
    fn try_recover_key(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> Option<Self> {
        DefId::try_recover_key(tcx, dep_node).map(ModId::new_unchecked)
    }
}

impl<'tcx> DepNodeKey<'tcx> for LocalModId {
    #[inline(always)]
    fn key_fingerprint_style() -> KeyFingerprintStyle {
        KeyFingerprintStyle::DefPathHash
    }

    #[inline(always)]
    fn to_fingerprint(&self, tcx: TyCtxt<'tcx>) -> Fingerprint {
        self.to_def_id().to_fingerprint(tcx)
    }

    #[inline(always)]
    fn try_recover_key(tcx: TyCtxt<'tcx>, dep_node: &DepNode) -> Option<Self> {
        LocalDefId::try_recover_key(tcx, dep_node).map(LocalModId::new_unchecked)
    }
}

// Opaque keys: the trait's default methods apply. This list replaces the old blanket impl
// (see `DepNodeKey`), so it must name every query key type in `query/keys.rs` that has no
// impl above, plus the non-query keys handed to `DepNode::construct` (`Symbol`, `MonoItem`).
// The key's lifetime is kept independent of `'tcx`, as it was under the blanket impl.
mod opaque_keys {
    use core::fmt::Debug;

    use super::DepNodeKey;
    use crate::rustc_ast::tokenstream::TokenStream;
    use crate::rustc_data_structures::stable_hash::StableHash;
    use crate::rustc_hir::def_id::{CrateNum, DefId, LocalDefId};
    use crate::rustc_middle::infer::canonical::CanonicalQueryInput;
    use crate::rustc_middle::mono::{CollectionMode, MonoItem};
    use crate::rustc_middle::ty::fast_reject::SimplifiedType;
    use crate::rustc_middle::ty::layout::ValidityRequirement;
    use crate::rustc_middle::ty::{self, GenericArg, GenericArgsRef, Ty};
    use crate::rustc_middle::{mir, traits};
    use crate::rustc_span::{Ident, LocalExpnId, Symbol};

    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::ShimKind<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::InstanceKind<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::Instance<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for mir::interpret::GlobalId<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (Ty<'k>, Option<ty::ExistentialTraitRef<'k>>) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::LitToConstInput<'k> {}
    impl<'tcx> DepNodeKey<'tcx> for SimplifiedType {}
    impl<'tcx> DepNodeKey<'tcx> for (DefId, Ident) {}
    impl<'tcx> DepNodeKey<'tcx> for (LocalDefId, LocalDefId, Ident) {}
    impl<'tcx> DepNodeKey<'tcx> for (CrateNum, DefId) {}
    impl<'tcx> DepNodeKey<'tcx> for (CrateNum, SimplifiedType) {}
    impl<'tcx> DepNodeKey<'tcx> for (DefId, ty::SizedTraitKind) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for GenericArgsRef<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (DefId, GenericArgsRef<'k>) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::TraitRef<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for GenericArg<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for Ty<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (Ty<'k>, Ty<'k>) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::Clauses<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::AliasTyKind<'k> {}
    impl<'tcx, 'k, T> DepNodeKey<'tcx> for ty::PseudoCanonicalInput<'k, T> where
        Self: StableHash + Debug
    {
    }
    impl<'tcx> DepNodeKey<'tcx> for Symbol {}
    impl<'tcx> DepNodeKey<'tcx> for Option<Symbol> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for &'k [u8] {}
    impl<'tcx, 'k, T> DepNodeKey<'tcx> for CanonicalQueryInput<'k, T> where Self: StableHash + Debug {}
    impl<'tcx, 'k, T> DepNodeKey<'tcx> for (CanonicalQueryInput<'k, T>, bool) where
        Self: StableHash + Debug
    {
    }
    impl<'tcx, 'k, T> DepNodeKey<'tcx> for (CanonicalQueryInput<'k, T>, usize) where
        Self: StableHash + Debug
    {
    }
    impl<'tcx, 'k> DepNodeKey<'tcx> for (Ty<'k>, crate::rustc_abi::VariantIdx) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (ty::Predicate<'k>, traits::WellFormedLoc) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (ty::PolyFnSig<'k>, &'k ty::List<Ty<'k>>) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (ty::Instance<'k>, &'k ty::List<Ty<'k>>) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for ty::Value<'k> {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (LocalExpnId, &'k TokenStream) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (ValidityRequirement, ty::PseudoCanonicalInput<'k, Ty<'k>>) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for (ty::Instance<'k>, CollectionMode) {}
    impl<'tcx, 'k> DepNodeKey<'tcx> for MonoItem<'k> {}
}
