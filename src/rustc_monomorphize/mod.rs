// tidy-alphabetical-start
// tidy-alphabetical-end

// `util.rs` warns with `eprintln!`, and the offload manifest is written to a file.
// The partitioning itself is pure; only those boundaries require std.

// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
#[macro_use]
use collector::MonoItemCollectionStrategy;
use crate::rustc_hir::attrs::lang_items::LangItem;
use alloc::vec::Vec;
use crate::rustc_middle::mono::MonoItem;
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_middle::query::TyCtxtAt;
use crate::rustc_middle::ty::adjustment::CustomCoerceUnsized;
use crate::rustc_middle::ty::{self, Ty};
use crate::rustc_middle::util::Providers;
use crate::rustc_middle::{bug, traits};
use crate::rustc_span::ErrorGuaranteed;

pub mod collector;
mod diagnostics;
mod graph_checks;
mod mono_checks;
mod offload;
mod partitioning;
mod util;

// Exposed so `rustc_codegen_ssa::base::codegen_crate` can trigger the
// host-metadata manifest write.
pub use offload::manifest::write_host_metadata_offload_manifest;

fn custom_coerce_unsize_info<'tcx>(
    tcx: TyCtxtAt<'tcx>,
    source_ty: Ty<'tcx>,
    target_ty: Ty<'tcx>,
) -> Result<CustomCoerceUnsized, ErrorGuaranteed> {
    let trait_ref = ty::TraitRef::new(
        tcx.tcx,
        tcx.require_lang_item(LangItem::CoerceUnsized, tcx.span),
        [source_ty, target_ty],
    );

    match tcx
        .codegen_select_candidate(ty::TypingEnv::fully_monomorphized().as_query_input(trait_ref))
    {
        Ok(traits::ImplSource::UserDefined(traits::ImplSourceUserDefinedData {
            impl_def_id,
            ..
        })) => Ok(tcx.coerce_unsized_info(*impl_def_id)?.custom_kind.unwrap()),
        impl_source => {
            bug!(
                "invalid `CoerceUnsized` from {source_ty} to {target_ty}: impl_source: {:?}",
                impl_source
            );
        }
    }
}

pub fn provide(providers: &mut Providers) {
    partitioning::provide(providers);
    mono_checks::provide(&mut providers.queries);
}

/// Every monomorphised item the crate would codegen, without partitioning them.
///
/// # Why this exists
///
/// `collect_and_partition_mono_items` does four things: collects the items, runs the
/// target-specific checks over the whole graph, partitions the result into codegen units, and
/// asserts that every symbol is distinct. A consumer that only wants to *know what the items
/// are* pays for all four, and then has to undo the third - an instance can be placed in more
/// than one codegen unit, so iterating the partitions yields duplicates that have to be filtered
/// back out with a hash set.
///
/// A consumer that emits its own objects never asks rustc to codegen, so partitioning is work it
/// throws away. Measured with `-Z time-passes` on a 158 KB unit: the collector graph walk is 82 ms and
/// `partition_and_assert_distinct_symbols` is 6 ms, so the saving is small - but the shape is
/// wrong either way, because asking for a partitioned form and reversing it is not a cost
/// question, it is the wrong question.
///
/// The `UsageMap` is deliberately not returned. It is the collector's internal graph and
/// exposing it would make an implementation detail part of this crate's interface.
pub fn collect_mono_items(tcx: TyCtxt<'_>, eager: bool) -> Vec<MonoItem<'_>> {
    let strategy = if eager {
        MonoItemCollectionStrategy::Eager
    } else {
        MonoItemCollectionStrategy::Lazy
    };
    collector::collect_crate_mono_items(tcx, strategy).0
}
