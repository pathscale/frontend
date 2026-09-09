//! This pass removes `PlaceMention` statement, which has no effect at codegen.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_middle::mir::*;
use crate::rustc_middle::ty::TyCtxt;
use tracing::trace;

use crate::rustc_mir_transform::PassPolicy;

pub(super) struct RemovePlaceMention;

impl<'tcx> crate::rustc_mir_transform::MirPass<'tcx> for RemovePlaceMention {
    fn policy(&self, sess: &crate::rustc_session::Session) -> PassPolicy {
        PassPolicy::optional_non_optimization(!sess.opts.unstable_opts.mir_preserve_ub)
    }

    fn run_pass(&self, _: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        trace!("Running RemovePlaceMention on {:?}", body.source);
        for data in body.basic_blocks.as_mut_preserves_cfg() {
            data.retain_statements(|statement| match statement.kind {
                StatementKind::PlaceMention(..) | StatementKind::Nop => false,
                _ => true,
            })
        }
    }
}
