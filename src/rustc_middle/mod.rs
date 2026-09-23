//! The "main crate" of the Rust compiler. This crate contains common
//! type definitions that are used by the other crates in the rustc
//! "family". The following are some prominent examples.
//!
//! - **HIR.** The "high-level (H) intermediate representation (IR)" is
//!   defined in the [`hir`] module.
//! - **THIR.** The "typed high-level (H) intermediate representation (IR)"
//!   is defined in the [`thir`] module.
//! - **MIR.** The "mid-level (M) intermediate representation (IR)" is
//!   defined in the [`mir`] module. This module contains only the
//!   *definition* of the MIR; the passes that transform and operate
//!   on MIR are found in `rustc_const_eval` crate.
//! - **Types.** The internal representation of types used in rustc is
//!   defined in the [`ty`] module. This includes the
//!   [**type context**][ty::TyCtxt] (or `tcx`), which is the central
//!   context during most of compilation, containing the interners and
//!   other things.
//!
//! For more information about how rustc works, see the [rustc dev guide].
//!
//! [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/
//!
//! # Note
//!
//! This API is completely unstable and subject to change.

// tidy-alphabetical-start
// tidy-alphabetical-end

#![allow(internal_features)]#![cfg_attr(doc, feature(intra_doc_pointers))]
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.

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

#[cfg(test)]
mod tests;

#[macro_use]
mod macros;

#[macro_use]
pub mod arena;

pub mod dep_graph;
pub mod diagnostics;
pub mod hir;
pub mod hooks;
pub mod ich;
pub mod infer;
pub mod lint;
pub mod metadata;
pub mod middle;
pub mod mir;
pub mod mono;
pub mod ptrauth;
pub mod queries;
pub mod query;
pub mod thir;
pub mod traits;
pub mod ty;
pub mod util;
pub mod verify_ich;

// Allows macros to refer to this crate as `::rustc_middle`
extern crate self as rustc_middle;
pub use crate::__impl_decoder_methods;
pub use crate::bug;
pub use crate::err_exhaust;
pub use crate::err_inval;
pub use crate::err_machine_stop;
pub use crate::err_ub;
pub use crate::err_ub_format;
pub use crate::err_unsup;
pub use crate::err_unsup_format;
pub use crate::implement_ty_decoder;
pub use crate::span_bug;
pub use crate::throw_exhaust;
pub use crate::throw_inval;
pub use crate::throw_machine_stop;
pub use crate::throw_ub;
pub use crate::throw_ub_format;
pub use crate::throw_unsup;
pub use crate::throw_unsup_format;
