// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub use rustc_hash::{FxBuildHasher, FxHasher};

// Dependency swap, not a port: `std::collections::HashMap` is hashbrown with a std wrapper
// around it, so this is the same table and the same hasher without libstd. rustc-hash only
// defines `FxHashMap` behind its own `std` feature, which is why the aliases are spelled out
// here instead. ~935 call sites reach libstd through these four lines and no longer do.
pub type FxHashMap<K, V> = hashbrown::HashMap<K, V, FxBuildHasher>;
pub type FxHashSet<V> = hashbrown::HashSet<V, FxBuildHasher>;

pub type StdEntry<'a, K, V> = hashbrown::hash_map::Entry<'a, K, V, FxBuildHasher>;

pub type FxIndexMap<K, V> = indexmap::IndexMap<K, V, FxBuildHasher>;
pub type FxIndexSet<V> = indexmap::IndexSet<V, FxBuildHasher>;
pub type IndexEntry<'a, K, V> = indexmap::map::Entry<'a, K, V>;
pub type IndexOccupiedEntry<'a, K, V> = indexmap::map::OccupiedEntry<'a, K, V>;

pub use indexmap::set::MutableValues;

#[macro_export]
macro_rules! define_id_collections {
    ($map_name:ident, $set_name:ident, $entry_name:ident, $key:ty) => {
        pub type $map_name<T> = $crate::rustc_data_structures::unord::UnordMap<$key, T>;
        pub type $set_name = $crate::rustc_data_structures::unord::UnordSet<$key>;
        pub type $entry_name<'a, T> = $crate::rustc_data_structures::fx::StdEntry<'a, $key, T>;
    };
}

#[macro_export]
macro_rules! define_stable_id_collections {
    ($map_name:ident, $set_name:ident, $entry_name:ident, $key:ty) => {
        pub type $map_name<T> = $crate::rustc_data_structures::fx::FxIndexMap<$key, T>;
        pub type $set_name = $crate::rustc_data_structures::fx::FxIndexSet<$key>;
        pub type $entry_name<'a, T> = $crate::rustc_data_structures::fx::IndexEntry<'a, $key, T>;
    };
}

pub mod default {
    use super::{FxBuildHasher, FxHashMap, FxHashSet};

    // FIXME: These two functions will become unnecessary after
    // <https://github.com/rust-lang/rustc-hash/pull/63> lands and we start using the corresponding
    // `rustc-hash` version. After that we can use `Default::default()` instead.
    pub const fn fx_hash_map<K, V>() -> FxHashMap<K, V> {
        FxHashMap::with_hasher(FxBuildHasher)
    }

    pub const fn fx_hash_set<V>() -> FxHashSet<V> {
        FxHashSet::with_hasher(FxBuildHasher)
    }
}
