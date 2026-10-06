//! A module's item-likes, indexed in walk order: the input of a per-owner stage.
//!
//! `TyCtxt::hir_visit_item_likes_in_module` visits a module's free items, then its trait items,
//! then its impl items, then its foreign items, each list in the order `hir_module_items` holds
//! it. A per-module pass that visits every item-like on its own, with no state carried from one
//! to the next, can run as a stage over the module's frozen `ModuleItems` instead: item `i` of
//! the stage is the `i`th item-like of that walk, so the stage's item order, and with it the
//! order its diagnostics are replayed in, is the walk's order. Nothing is copied: the stage
//! reads the query's arena-held lists in place by index.
//!
//! The same order is `ModuleItems::definitions` for a module (`hir_module_items` never adds the
//! crate root; only `hir_crate_items` does), so [`item_like_def_id`] is the `i`th definition too.

use crate::rustc_hir::def_id::LocalDefId;
use crate::rustc_hir::intravisit::Visitor;
use crate::rustc_hir::{ForeignItemId, ImplItemId, ItemId, TraitItemId};
use crate::rustc_middle::hir::ModuleItems;
use crate::rustc_middle::ty::TyCtxt;

/// One item-like of a module, by kind.
#[derive(Clone, Copy)]
enum ItemLike<'a> {
    Free(&'a ItemId),
    Trait(&'a TraitItemId),
    Impl(&'a ImplItemId),
    Foreign(&'a ForeignItemId),
}

fn item_like(module: &ModuleItems, index: usize) -> ItemLike<'_> {
    let mut index = index;
    let free = module.free_item_ids();
    if index < free.len() {
        return ItemLike::Free(&free[index]);
    }
    index -= free.len();
    let trait_items = module.trait_item_ids();
    if index < trait_items.len() {
        return ItemLike::Trait(&trait_items[index]);
    }
    index -= trait_items.len();
    let impl_items = module.impl_item_ids();
    if index < impl_items.len() {
        return ItemLike::Impl(&impl_items[index]);
    }
    index -= impl_items.len();
    ItemLike::Foreign(&module.foreign_item_ids()[index])
}

/// How many item-likes `hir_visit_item_likes_in_module` visits for `module`: the length of a
/// per-owner stage over it.
pub fn item_like_count(module: &ModuleItems) -> usize {
    module.free_item_ids().len()
        + module.trait_item_ids().len()
        + module.impl_item_ids().len()
        + module.foreign_item_ids().len()
}

/// Visit the `index`th item-like of `module` exactly as `hir_visit_item_likes_in_module` visits
/// it (`visit_item`, `visit_trait_item`, `visit_impl_item` or `visit_foreign_item`).
pub fn visit_item_like<'tcx, V: Visitor<'tcx>>(
    tcx: TyCtxt<'tcx>,
    module: &ModuleItems,
    index: usize,
    visitor: &mut V,
) -> V::Result {
    match item_like(module, index) {
        ItemLike::Free(&id) => visitor.visit_item(tcx.hir_item(id)),
        ItemLike::Trait(&id) => visitor.visit_trait_item(tcx.hir_trait_item(id)),
        ItemLike::Impl(&id) => visitor.visit_impl_item(tcx.hir_impl_item(id)),
        ItemLike::Foreign(&id) => visitor.visit_foreign_item(tcx.hir_foreign_item(id)),
    }
}

/// The `index`th item-like of `module` as the item of a stage whose pass costs `ns_per_byte`
/// (`sync::cost`): its source, for `run_stage_weighted`. A free `impl`, `trait` or inline `mod`
/// item's source holds its members, which are item-likes of their own; counting them twice
/// overestimates, which errs towards fanning out.
pub fn item_like_weight(
    tcx: TyCtxt<'_>,
    module: &ModuleItems,
    index: usize,
    ns_per_byte: u32,
) -> u32 {
    tcx.stage_weight(item_like_def_id(module, index), ns_per_byte)
}

/// The owner of the `index`th item-like of `module`: the `index`th of
/// `ModuleItems::definitions`.
pub fn item_like_def_id(module: &ModuleItems, index: usize) -> LocalDefId {
    match item_like(module, index) {
        ItemLike::Free(id) => id.owner_id.def_id,
        ItemLike::Trait(id) => id.owner_id.def_id,
        ItemLike::Impl(id) => id.owner_id.def_id,
        ItemLike::Foreign(id) => id.owner_id.def_id,
    }
}
