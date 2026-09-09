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

use crate::polonius_engine::{AllFacts, Atom, Output};
use rustc_macros::extension;
use crate::rustc_middle::mir::Local;
use crate::rustc_middle::ty::{RegionVid, TyCtxt};
use crate::rustc_mir_dataflow::move_paths::MovePathIndex;

use super::{LocationIndex, PoloniusLocationTable};
use crate::rustc_borrowck::BorrowIndex;

#[derive(Copy, Clone, Debug)]
pub struct RustcFacts;

pub type PoloniusOutput = Output<RustcFacts>;

crate::rustc_index::newtype_index! {
    /// A (kinda) newtype of `RegionVid` so we can implement `Atom` on it.
    #[orderable]
    #[debug_format = "'?{}"]
    pub struct PoloniusRegionVid {}
}

impl crate::polonius_engine::Atom for PoloniusRegionVid {
    fn index(self) -> usize {
        self.as_usize()
    }
}
impl From<RegionVid> for PoloniusRegionVid {
    fn from(value: RegionVid) -> Self {
        Self::from_usize(value.as_usize())
    }
}
impl From<PoloniusRegionVid> for RegionVid {
    fn from(value: PoloniusRegionVid) -> Self {
        Self::from_usize(value.as_usize())
    }
}

impl crate::polonius_engine::FactTypes for RustcFacts {
    type Origin = PoloniusRegionVid;
    type Loan = BorrowIndex;
    type Point = LocationIndex;
    type Variable = Local;
    type Path = MovePathIndex;
}

pub type PoloniusFacts = AllFacts<RustcFacts>;

#[extension(pub(crate) trait PoloniusFactsExt)]
impl PoloniusFacts {
    /// Returns `true` if there is a need to gather `PoloniusFacts` given the
    /// current `-Z` flags.
    ///
    /// Upstream also gathers when `-Znll-facts` is set, which asks for the facts to be written
    /// into a directory for an external solver to read. The writer is gone from this fork, so
    /// that flag would have bought a full fact-gathering pass and then dropped the result. The
    /// flag is removed rather than left to cost time silently, and legacy polonius is the only
    /// remaining reason to gather, because it is the only thing left that reads them.
    fn enabled(tcx: TyCtxt<'_>) -> bool {
        tcx.sess.opts.unstable_opts.polonius.is_legacy_enabled()
    }

}

impl Atom for BorrowIndex {
    fn index(self) -> usize {
        self.as_usize()
    }
}

impl Atom for LocationIndex {
    fn index(self) -> usize {
        self.as_usize()
    }
}














