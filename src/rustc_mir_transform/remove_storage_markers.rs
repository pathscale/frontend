//! This pass removes storage markers if they won't be emitted during codegen.

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

pub(super) struct RemoveStorageMarkers;

impl<'tcx> crate::rustc_mir_transform::MirPass<'tcx> for RemoveStorageMarkers {
    fn policy(&self, sess: &crate::rustc_session::Session) -> PassPolicy {
        PassPolicy::optional_non_optimization(
            sess.mir_opt_level() > 0 && !sess.emit_lifetime_markers(),
        )
    }

    fn run_pass(&self, _tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        trace!("Running RemoveStorageMarkers on {:?}", body.source);
        for data in body.basic_blocks.as_mut_preserves_cfg() {
            data.retain_statements(|statement| match statement.kind {
                StatementKind::StorageLive(..)
                | StatementKind::StorageDead(..)
                | StatementKind::Nop => false,
                _ => true,
            })
        }
    }
}
