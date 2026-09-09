// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::find_attr;
use crate::bug;
use crate::rustc_middle::ty::{self, TyCtxt};
use crate::rustc_span::sym;

pub(crate) fn inferred_outlives(tcx: TyCtxt<'_>) {
    for id in tcx.hir_free_items() {
        if !find_attr!(tcx, id.owner_id, RustcDumpInferredOutlives) {
            continue;
        }

        let preds = tcx.inferred_outlives_of(id.owner_id);
        let mut preds: Vec<_> = preds
            .iter()
            .map(|(pred, _)| match pred.kind().skip_binder() {
                ty::ClauseKind::RegionOutlives(p) => p.to_string(),
                ty::ClauseKind::TypeOutlives(p) => p.to_string(),
                err => bug!("unexpected clause {:?}", err),
            })
            .collect();
        preds.sort();

        let span = tcx.def_span(id.owner_id);
        let mut err = tcx.dcx().struct_span_err(span, sym::rustc_dump_inferred_outlives.as_str());
        for pred in preds {
            err.note(pred);
        }
        err.emit();
    }
}
