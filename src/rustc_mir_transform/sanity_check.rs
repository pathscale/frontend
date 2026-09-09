// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_middle::mir::Body;
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_mir_dataflow::rustc_peek::sanity_check;

pub(super) struct SanityCheck;

impl<'tcx> crate::rustc_mir_transform::MirLint<'tcx> for SanityCheck {
    fn run_lint(&self, tcx: TyCtxt<'tcx>, body: &Body<'tcx>) {
        sanity_check(tcx, body);
    }
}
