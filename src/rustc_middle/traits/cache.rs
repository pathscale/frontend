//! Cache for candidate selection.

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

use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_data_structures::sync::Lock;

use crate::rustc_middle::dep_graph::DepNodeIndex;
use crate::rustc_middle::ty::TyCtxt;

pub struct WithDepNodeCache<Key, Value> {
    hashmap: Lock<FxHashMap<Key, WithDepNode<Value>>>,
}

impl<Key: Clone, Value: Clone> Clone for WithDepNodeCache<Key, Value> {
    fn clone(&self) -> Self {
        Self { hashmap: Lock::new(self.hashmap.borrow().clone()) }
    }
}

impl<Key, Value> Default for WithDepNodeCache<Key, Value> {
    fn default() -> Self {
        Self { hashmap: Default::default() }
    }
}

impl<Key: Eq + Hash, Value: Clone> WithDepNodeCache<Key, Value> {
    pub fn get<'tcx>(&self, key: &Key, tcx: TyCtxt<'tcx>) -> Option<Value> {
        Some(self.hashmap.borrow().get(key)?.get(tcx))
    }

    pub fn insert(&self, key: Key, dep_node: DepNodeIndex, value: Value) {
        self.hashmap.borrow_mut().insert(key, WithDepNode::new(dep_node, value));
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WithDepNode<T> {
    dep_node: DepNodeIndex,
    cached_value: T,
}

impl<T: Clone> WithDepNode<T> {
    pub(crate) fn new(dep_node: DepNodeIndex, cached_value: T) -> Self {
        WithDepNode { dep_node, cached_value }
    }

    pub(crate) fn get<'tcx>(&self, tcx: TyCtxt<'tcx>) -> T {
        tcx.dep_graph.read_index(self.dep_node);
        self.cached_value.clone()
    }
}
