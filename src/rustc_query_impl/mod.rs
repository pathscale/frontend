// tidy-alphabetical-start
// tidy-alphabetical-end

// `job.rs` reports query cycles across threads. Threading, not query execution. (`self_profile.rs`
// wrote profiling events through `io::Write` and was named here too; it went with `measureme`.)
#![allow(internal_features)]

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
use crate::rustc_data_structures::sync::AtomicU64;
use crate::rustc_middle::arena::Arena;
use crate::rustc_middle::queries::{ExternProviders, Providers};
use crate::rustc_middle::query::{QuerySystem, QueryWaitGraph};
use crate::rustc_middle::query::on_disk_cache::OnDiskCache;

pub use crate::rustc_query_impl::job::{
    CollectActiveJobsKind, QueryJobMap, break_query_cycle, collect_active_query_jobs,
    print_query_stack,
};

mod dep_kind_vtables;
mod diagnostics;
mod execution;
mod handle_cycle_error;
mod incremental;
mod job;
mod query_vtables;

pub fn query_system<'tcx>(
    arena: &'tcx Arena<'tcx>,
    local_providers: Providers,
    extern_providers: ExternProviders,
    on_disk_cache: Option<OnDiskCache>,
    incremental: bool,
) -> QuerySystem<'tcx> {
    QuerySystem {
        arenas: Default::default(),
        dep_kind_vtables: dep_kind_vtables::make_dep_kind_vtables(arena),
        query_vtables: query_vtables::make_query_vtables(incremental),
        side_effects: Default::default(),
        used_features: Default::default(),
        on_disk_cache,
        local_providers,
        extern_providers,
        jobs: AtomicU64::new(1),
        wait_graph: QueryWaitGraph::new(),
        diagnostics: Default::default(),
    }
}

pub fn provide(providers: &mut crate::rustc_middle::util::Providers) {
    // `providers.hooks.alloc_self_profile_query_strings` was registered here. It walked every
    // query cache at the end of the session to turn each `DepNodeIndex` into a `measureme`
    // string id. With no string table there is nothing to allocate, so the hook and the module
    // behind it are gone rather than left as a no-op.
    providers.hooks.verify_query_key_hashes = incremental::verify_query_key_hashes;
    providers.hooks.encode_query_values = incremental::encode_query_values;
}
