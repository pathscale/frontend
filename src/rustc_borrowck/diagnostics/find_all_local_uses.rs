// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::collections::BTreeSet;

use crate::rustc_middle::mir::visit::{PlaceContext, Visitor};
use crate::rustc_middle::mir::{Body, Local, Location};

/// Find all uses of (including assignments to) a [`Local`].
///
/// Uses `BTreeSet` so output is deterministic.
pub(super) fn find(body: &Body<'_>, local: Local) -> BTreeSet<Location> {
    let mut visitor = AllLocalUsesVisitor { for_local: local, uses: BTreeSet::default() };
    visitor.visit_body(body);
    visitor.uses
}

struct AllLocalUsesVisitor {
    for_local: Local,
    uses: BTreeSet<Location>,
}

impl<'tcx> Visitor<'tcx> for AllLocalUsesVisitor {
    fn visit_local(&mut self, local: Local, _context: PlaceContext, location: Location) {
        if local == self.for_local {
            self.uses.insert(location);
        }
    }
}
