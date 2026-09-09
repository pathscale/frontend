// tidy-alphabetical-start
// tidy-alphabetical-end

// The proc-macro bridge: it loads modules from disk, runs proc macros (which are std by
// construction - they are dylibs built against it), and prints `-Zmacro-stats` with
// `eprint!`. Not a candidate for no_std while proc macros exist.
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
mod build;
mod diagnostics;
mod mbe;
mod placeholders;
mod proc_macro_server;
mod stats;

pub use mbe::macro_rules::{MacroRulesMacroExpander, compile_declarative_macro};
pub mod base;
pub mod config;
pub mod expand;
pub mod module;
pub mod proc_macro;

pub fn provide(providers: &mut crate::rustc_middle::query::Providers) {
    providers.derive_macro_expansion = proc_macro::provide_derive_macro_expansion;
}
pub use crate::configure;
