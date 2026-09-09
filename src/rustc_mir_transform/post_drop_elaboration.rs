// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_const_eval::check_consts;
use crate::rustc_middle::mir::*;
use crate::rustc_middle::ty::TyCtxt;

use crate::rustc_mir_transform::MirLint;

pub(super) struct CheckLiveDrops;

impl<'tcx> MirLint<'tcx> for CheckLiveDrops {
    fn run_lint(&self, tcx: TyCtxt<'tcx>, body: &Body<'tcx>) {
        check_consts::post_drop_elaboration::check_live_drops(tcx, body);
    }
}
