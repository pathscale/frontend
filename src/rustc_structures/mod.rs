//! Basic structs that end up being used in attributes for one reason or another,
//! but are not used exclusively in or around attributes.

// Only for `IntoDiagArg::into_diag_arg`, whose signature takes a `&mut Option<PathBuf>`
// this crate never reads - it is `_` at both sites. The dependency belongs to the
// diagnostics layer that defines the trait, and goes when the diagnostics layer owns the trait outright.
#![deny(unstable_features, reason = "ends up in dependencies of rust-analyzer")]

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
mod collapse_macro_debug_info;
mod crate_type;
mod limit;
mod native_lib_kind;
mod sanitizer_set;

pub use collapse_macro_debug_info::CollapseMacroDebuginfo;
pub use crate_type::CrateType;
pub use limit::Limit;
pub use native_lib_kind::NativeLibKind;
pub use sanitizer_set::SanitizerSet;
