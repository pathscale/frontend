// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use hashbrown::{HashMap, HashSet};
use core::hash::{BuildHasherDefault, Hasher};

pub type UnhashMap<K, V> = HashMap<K, V, BuildHasherDefault<Unhasher>>;
pub type UnhashSet<V> = HashSet<V, BuildHasherDefault<Unhasher>>;
pub type UnindexMap<K, V> = indexmap::IndexMap<K, V, BuildHasherDefault<Unhasher>>;

/// This no-op hasher expects only a single `write_u64` call. It's intended for
/// map keys that already have hash-like quality, like `Fingerprint`.
#[derive(Default)]
pub struct Unhasher {
    value: u64,
}

impl Hasher for Unhasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.value
    }

    fn write(&mut self, _bytes: &[u8]) {
        unimplemented!("use write_u64");
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        debug_assert_eq!(0, self.value, "Unhasher doesn't mix values!");
        self.value = value;
    }
}
