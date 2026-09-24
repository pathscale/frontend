//! Various checks
//!
//! # Note
//!
//! This API is completely unstable and subject to change.

// tidy-alphabetical-start
// tidy-alphabetical-end

// Real std use: `-Zinput-stats` prints its table with `eprint!`, and the diagnostics here
// carry an `io::Error` and a `Path` for the debugging dumps. Both are output surfaces, so
// both go when the caller owns its own output rather than writing to a terminal.

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
use crate::rustc_middle::query::Providers;

pub mod abi_test;
mod canonical_symbols;
mod check_attr;
mod check_export;
pub mod dead;
mod debugger_visualizer;
pub mod delegation;
mod diagnostic_items;
mod diagnostics;
mod eii;
pub mod entry;
pub mod hir_id_validator;
pub mod input_stats;
pub mod item_likes;
mod lang_items;
pub mod layout_test;
mod lib_features;
mod reachable;
pub mod stability;
mod upvars;
mod weak_lang_items;

pub fn provide(providers: &mut Providers) {
    canonical_symbols::provide(providers);
    check_attr::provide(providers);
    dead::provide(providers);
    debugger_visualizer::provide(providers);
    diagnostic_items::provide(providers);
    entry::provide(providers);
    lang_items::provide(providers);
    lib_features::provide(providers);
    reachable::provide(providers);
    stability::provide(providers);
    upvars::provide(providers);
    check_export::provide(providers);
    providers.check_externally_implementable_items = eii::check_externally_implementable_items;
}
