// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::*;

impl<'graph, G: DirectedGraph> DirectedGraph for &'graph G {
    type Node = G::Node;

    fn num_nodes(&self) -> usize {
        (**self).num_nodes()
    }
}

impl<'graph, G: StartNode> StartNode for &'graph G {
    fn start_node(&self) -> Self::Node {
        (**self).start_node()
    }
}

impl<'graph, G: Successors> Successors for &'graph G {
    fn successors(&self, node: Self::Node) -> impl Iterator<Item = Self::Node> {
        (**self).successors(node)
    }
}

impl<'graph, G: Predecessors> Predecessors for &'graph G {
    fn predecessors(&self, node: Self::Node) -> impl Iterator<Item = Self::Node> {
        (**self).predecessors(node)
    }
}
