//! Crate containing the implementation of the next-generation trait solver.
//!
//! This crate may also contain things that are used by the old trait solver,
//! but were uplifted in the process of making the new trait solver generic.
//! So if you got to this crate from the old solver, it's totally normal.
// core has the macro behind the feature; std re-exports it at its root ungated.

// tidy-alphabetical-start
// tidy-alphabetical-end


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
pub mod canonical;
pub mod coherence;
pub mod delegate;
pub mod normalize;
pub mod placeholder;
pub mod solve;
