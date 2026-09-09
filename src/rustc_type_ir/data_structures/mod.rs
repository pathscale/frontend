pub use crate::ena::unify::{NoError, UnifyKey, UnifyValue};
use rustc_hash::FxBuildHasher;
// Same dependency swap as `crate::rustc_data_structures::fx`: hashbrown with the Fx hasher is the
// table `std::collections::HashMap` is built on, so this is the same structure without libstd.
// rustc-hash only defines `FxHashMap` behind its own `std` feature.
pub type HashMap<K, V> = hashbrown::HashMap<K, V, FxBuildHasher>;
pub type HashSet<V> = hashbrown::HashSet<V, FxBuildHasher>;

pub type IndexMap<K, V> = indexmap::IndexMap<K, V, FxBuildHasher>;
pub type IndexSet<V> = indexmap::IndexSet<V, FxBuildHasher>;

mod delayed_map;

#[cfg(feature = "nightly")]
mod impl_ {
    pub use crate::rustc_data_structures::sso::{SsoHashMap, SsoHashSet};
}

#[cfg(not(feature = "nightly"))]
mod impl_ {
    pub use hashbrown::{HashMap as SsoHashMap, HashSet as SsoHashSet};
}

pub use delayed_map::{DelayedMap, DelayedSet};
pub use impl_::*;
