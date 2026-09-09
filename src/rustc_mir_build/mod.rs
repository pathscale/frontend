//! Construction of MIR from HIR.

// tidy-alphabetical-start
// tidy-alphabetical-end

// The `builder` module used to be named `build`, but that was causing GitHub's
// "Go to file" feature to silently ignore all files in the module, probably
// because it assumes that "build" is a build-output directory. See #134365.
// `-Zdump-mir` writes the built MIR to `io::stdout()`. Debug output, same surface as the
// other dumpers, and it goes with them.

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
mod builder;
mod check_tail_calls;
mod check_unsafety;
mod diagnostics;
pub mod thir;

use crate::rustc_middle::util::Providers;

pub fn provide(providers: &mut Providers) {
    providers.queries.check_match = thir::pattern::check_match;
    providers.queries.lit_to_const = thir::constant::lit_to_const;
    providers.queries.closure_saved_names_of_captured_variables =
        builder::closure_saved_names_of_captured_variables;
    providers.queries.check_unsafety = check_unsafety::check_unsafety;
    providers.queries.check_tail_calls = check_tail_calls::check_tail_calls;
    providers.queries.thir_body = thir::cx::thir_body;
    providers.hooks.build_mir_inner_impl = builder::build_mir_inner_impl;
}
