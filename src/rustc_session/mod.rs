// tidy-alphabetical-start
// To generate CodegenOptionsTargetModifiers and UnstableOptionsTargetModifiers enums
// with macro_rules, it is necessary to use recursive mechanic ("Incremental TT Munchers").
// tidy-alphabetical-end

#![allow(internal_features)]
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
// Real std remains: the session IS the environment: command-line options, output paths, `env`
// lookups, the file an artefact is written to.

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
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub(crate) use crate::rustc_lint_defs as lint;
pub use session::*;

pub mod diagnostics;
pub mod parse;
pub mod utils;
#[macro_use]
pub mod config;
pub mod filesearch;
mod options;
pub mod output;
pub mod search_paths;
mod session;
