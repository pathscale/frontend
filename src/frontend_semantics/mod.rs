//! Rust frontend semantics needed before lowering to a downstream IR.
//!
//! Attributes affect MIR, target features affect parsing and ABI checks, and exported symbols
//! root monomorphization. None of those jobs requires a rustc backend abstraction, object writer,
//! linker, rlink file, or compiled-module model.

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
#[cfg(test)]
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_data_structures::unord::UnordSet;
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_middle::util::Providers;
use crate::rustc_span::Symbol;

pub mod codegen_attrs;
mod diagnostics;
mod symbols;
pub mod target_features;

/// Target facts the Rust frontend needs to build cfgs and validate ABIs.
pub struct TargetConfig {
    pub internal_target_features: UnordSet<Symbol>,
    pub has_reliable_f16: bool,
    pub has_reliable_f16_math: bool,
    pub has_reliable_f128: bool,
    pub has_reliable_f128_math: bool,
}

/// Install only the semantic queries consumed by analysis and monomorphization.
pub fn provide(providers: &mut Providers) {
    symbols::provide(providers);
    target_features::provide(&mut providers.queries);
    codegen_attrs::provide(&mut providers.queries);
    providers.queries.global_backend_features = |_tcx: TyCtxt<'_>, ()| vec![];
    providers.queries.backend_optimization_level = |tcx, _| tcx.sess.opts.optimize;
}
