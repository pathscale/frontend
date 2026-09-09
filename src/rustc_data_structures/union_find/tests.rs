// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::UnionFind;

#[test]
fn empty() {
    let mut sets = UnionFind::<u32>::new(10);

    for i in 1..10 {
        assert_eq!(sets.find(i), i);
    }
}

#[test]
fn transitive() {
    let mut sets = UnionFind::<u32>::new(10);

    sets.unify(3, 7);
    sets.unify(4, 2);

    assert_eq!(sets.find(7), sets.find(3));
    assert_eq!(sets.find(2), sets.find(4));
    assert_ne!(sets.find(3), sets.find(4));

    sets.unify(7, 4);

    assert_eq!(sets.find(7), sets.find(3));
    assert_eq!(sets.find(2), sets.find(4));
    assert_eq!(sets.find(3), sets.find(4));

    for i in [0, 1, 5, 6, 8, 9] {
        assert_eq!(sets.find(i), i);
    }
}
