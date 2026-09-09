// Vendored from polonius-engine 0.13.0, converted to `no_std`. See `VENDORED.md`.
//
// The conversion touches three things and nothing else:
//
//   * `std::{fmt, hash, str}` -> `core::`, and `std::{borrow::Cow, collections::BTree*}` ->
//     `alloc::`. A rename.
//   * `rustc_hash::{FxHashMap, FxHashSet}` -> the `fx` aliases below. rustc-hash only defines
//     those two behind its own `std` feature, because they are `std::collections::HashMap` with
//     `FxBuildHasher` bolted on. `std`'s `HashMap` *is* hashbrown with a wrapper, so naming
//     hashbrown directly is the same table and the same hasher without libstd. This mirrors
//     `crate::rustc_data_structures::fx`, which the rest of this compiler already goes through.
//   * `std::time::Instant` is gone. It was used only to append `{:?}` of `timer.elapsed()` to
//     five `info!` lines. `core` has no clock, and no caller reads those durations - they are
//     log text - so the durations were dropped from the messages rather than faked with a
//     zero-valued shim that would report "0ns" and be believed.
//
// No rule, no relation, and no public API is touched. The datalog is byte for byte upstream's.

/// Contains the core of the Polonius borrow checking engine.
/// Input is fed in via AllFacts, and outputs are returned via Output

mod facts;
mod output;

/// `#![no_std]` replacements for `rustc_hash::{FxHashMap, FxHashSet}`, which rustc-hash only
/// defines behind its own `std` feature. Same hasher, same table.
pub(crate) mod fx {
    pub(crate) type FxHashMap<K, V> = hashbrown::HashMap<K, V, rustc_hash::FxBuildHasher>;
    pub(crate) type FxHashSet<V> = hashbrown::HashSet<V, rustc_hash::FxBuildHasher>;
}

// Reexports of facts
pub use facts::AllFacts;
pub use facts::Atom;
pub use facts::FactTypes;
pub use output::Algorithm;
pub use output::Output;
