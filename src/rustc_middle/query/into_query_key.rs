// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_hir::OwnerId;
use crate::rustc_hir::def_id::{DefId, LocalDefId, LocalModId, ModId};

/// Argument-conversion trait used by some queries and other `TyCtxt` methods.
///
/// A function that accepts an `impl IntoQueryKey<DefId>` argument can be thought
/// of as taking a [`DefId`], except that callers can also pass a [`LocalDefId`]
/// or values of other narrower ID types, as long as they have a trivial conversion
/// to `DefId`.
///
/// Using a dedicated trait instead of [`Into`] makes the purpose of the conversion
/// more explicit, and makes occurrences easier to search for.
pub trait IntoQueryKey<K> {
    /// Argument conversion from `Self` to `K`.
    /// This should always be a very cheap conversion, e.g. [`LocalDefId::to_def_id`].
    fn into_query_key(self) -> K;
}

/// Any type can be converted to itself.
///
/// This is useful in generic or macro-generated code where we don't know whether
/// conversion is actually needed, so that we can do a conversion unconditionally.
impl<K> IntoQueryKey<K> for K {
    #[inline(always)]
    fn into_query_key(self) -> K {
        self
    }
}

impl IntoQueryKey<LocalDefId> for OwnerId {
    #[inline(always)]
    fn into_query_key(self) -> LocalDefId {
        self.def_id
    }
}

impl IntoQueryKey<DefId> for LocalDefId {
    #[inline(always)]
    fn into_query_key(self) -> DefId {
        self.to_def_id()
    }
}

impl IntoQueryKey<DefId> for OwnerId {
    #[inline(always)]
    fn into_query_key(self) -> DefId {
        self.to_def_id()
    }
}

impl IntoQueryKey<DefId> for ModId {
    #[inline(always)]
    fn into_query_key(self) -> DefId {
        self.to_def_id()
    }
}

impl IntoQueryKey<DefId> for LocalModId {
    #[inline(always)]
    fn into_query_key(self) -> DefId {
        self.to_def_id()
    }
}

impl IntoQueryKey<LocalDefId> for LocalModId {
    #[inline(always)]
    fn into_query_key(self) -> LocalDefId {
        self.into()
    }
}
