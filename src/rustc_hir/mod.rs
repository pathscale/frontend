//! HIR datatypes. See the [rustc dev guide] for more info.
//!
//! [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/hir.html

// tidy-alphabetical-start

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
// Only for `IntoDiagArg::into_diag_arg`, whose signature takes a `&mut Option<PathBuf>` -
// a parameter this crate never reads (it is `_` at every site here). The std dependency
// belongs to the diagnostics layer that defines the trait, not to this crate, and it goes
// when that layer does.
// tidy-alphabetical-end

mod arena;
pub mod def;
mod hir;
pub mod intravisit;
pub mod lints;
pub mod pat_util;
mod stable_hash_impls;
mod target_impls;

#[doc(no_inline)]
pub use hir::*;
pub use crate::rustc_attr_ir::{self as attrs, find_attr};
pub use crate::rustc_hir_id::*;
pub use crate::rustc_span::def_id;
// FIXME: Remove this use tree, replace by `crate::rustc_hir::attrs` or `rustc_attr_ir` imports
#[doc(hidden)]
pub use {
    attrs::target::{self, AssocCtxt, MethodKind, Target},
    attrs::{
        AttrArgs, AttrItem, AttrPath, Attribute, ConstStability, DefaultBodyStability,
        HashIgnoredAttrId, PartialConstStability, Stability, StabilityLevel, StableSince,
        UnstableReason, VERSION_PLACEHOLDER,
    },
};

pub use crate::rustc_hir::arena::Arena;
