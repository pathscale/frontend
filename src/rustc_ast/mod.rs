//! The Rust Abstract Syntax Tree (AST).
//!
//! # Note
//!
//! This API is completely unstable and subject to change.

// tidy-alphabetical-start
// tidy-alphabetical-end

// `attr/version.rs` reads `RUSTC_OVERRIDE_VERSION_STRING` from the environment and caches
// it in a `OnceLock`. A test hook, and environmental input rather than computation - it
// goes when the version is passed in rather than read.

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
pub mod util {
    pub mod case;
    pub mod classify;
    pub mod comments;
    pub mod literal;
    pub mod parser;
    pub mod unicode;
}

pub mod ast;
pub mod ast_traits;
pub mod attr;
pub mod entry;
pub mod expand;
pub mod format;
pub mod mut_visit;
pub mod node_id;
pub mod token;
pub mod tokenstream;
pub mod visit;

pub use self::ast::*;
pub use self::ast_traits::{AstNodeWrapper, HasAttrs, HasNodeId, HasTokens};
pub use crate::common_visitor_and_walkers;
