// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_crate_store::ForeignModule;
use crate::rustc_data_structures::fx::FxIndexMap;
use crate::rustc_hir as hir;
use crate::rustc_hir::def::DefKind;
use crate::rustc_hir::def_id::DefId;
use crate::rustc_middle::query::LocalCrate;
use crate::rustc_middle::ty::TyCtxt;

pub(crate) fn collect(tcx: TyCtxt<'_>, LocalCrate: LocalCrate) -> FxIndexMap<DefId, ForeignModule> {
    let mut modules = FxIndexMap::default();

    // We need to collect all the `ForeignMod`, even if they are empty.
    for id in tcx.hir_free_items() {
        if !matches!(tcx.def_kind(id.owner_id), DefKind::ForeignMod) {
            continue;
        }

        let def_id = id.owner_id.to_def_id();
        let item = tcx.hir_item(id);

        if let hir::ItemKind::ForeignMod { abi, items } = item.kind {
            let foreign_items = items.iter().map(|it| it.owner_id.to_def_id()).collect();
            modules.insert(def_id, ForeignModule { def_id, abi, foreign_items });
        }
    }

    modules
}
