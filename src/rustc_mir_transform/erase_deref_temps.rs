//! This pass converts all `DerefTemp` locals into normal temporaries
//! and turns their `CopyForDeref` rvalues into normal copies.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_middle::mir::visit::MutVisitor;
use crate::rustc_middle::mir::*;
use crate::rustc_middle::ty::TyCtxt;

use crate::rustc_mir_transform::PassPolicy;

struct EraseDerefTempsVisitor<'tcx> {
    tcx: TyCtxt<'tcx>,
}

impl<'tcx> MutVisitor<'tcx> for EraseDerefTempsVisitor<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn visit_rvalue(&mut self, rvalue: &mut Rvalue<'tcx>, _: Location) {
        if let &mut Rvalue::CopyForDeref(place) = rvalue {
            // We do *NOT* want a retag here! This assignment might copy a mutable reference we
            // can't actually copy, we just need it temporarily to create another pointer.
            *rvalue = Rvalue::Use(Operand::Copy(place), WithRetag::No)
        }
    }

    fn visit_local_decl(&mut self, _: Local, local_decl: &mut LocalDecl<'tcx>) {
        if local_decl.is_deref_temp() {
            let info = local_decl.local_info.as_mut().unwrap_crate_local();
            **info = LocalInfo::Boring;
        }
    }
}

pub(super) struct EraseDerefTemps;

impl<'tcx> crate::rustc_mir_transform::MirPass<'tcx> for EraseDerefTemps {
    fn run_pass(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        EraseDerefTempsVisitor { tcx }.visit_body_preserves_cfg(body);
    }

    fn policy(&self, _sess: &crate::rustc_session::Session) -> PassPolicy {
        // Later MIR stages assume that CopyForDeref is gone.
        PassPolicy::Required
    }
}
