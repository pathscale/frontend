//! Centralized logic for parsing and attributes.
//!
//! ## Architecture
//! This crate is part of a series of crates and modules that handle attribute processing.
//! - [`rustc_attr_ir`]: Defines the data structures that store parsed attributes
//! - `rustc_attr_parsing`: This crate, handles the parsing of attributes
//! - [`crate::rustc_passes::check_attr`] handles attribute validation that cannot be done in this crate
//!
//! The separation between data structures and parsing follows the principle of separation of concerns.
//! Data structures (`rustc_attr_ir`) define what attributes look like after parsing.
//! This crate (`rustc_attr_parsing`) handles how to convert raw tokens into those structures.
//! This split allows other parts of the compiler to use the data structures without needing
//! the parsing logic, making the codebase more modular and maintainable.
//!
//! ## Background
//! Previously, the compiler had a single attribute definition ([`ast::Attribute`]) with parsing and
//! validation scattered throughout the codebase. This was reorganized for better maintainability
//! (see [#131229](https://github.com/rust-lang/rust/issues/131229)).
//!
//! ## Types of Attributes
//! In Rust, attributes are markers that can be attached to items. They come in two main categories.
//!
//! ### 1. Active Attributes
//! These are attribute-like proc-macros that expand into other Rust code.
//! They can be either user-defined or compiler-provided. Examples of compiler-provided active attributes:
//!   - `#[derive(...)]`: Expands into trait implementations
//!   - `#[cfg()]`: Expands based on configuration
//!   - `#[cfg_attr()]`: Conditional attribute application
//!
//! ### 2. Inert Attributes
//! These are pure markers that don't expand into other code. They guide the compilation process.
//! They can be user-defined (in proc-macro helpers) or built-in. Examples of built-in inert attributes:
//!   - `#[stable()]`: Marks stable API items
//!   - `#[inline()]`: Suggests function inlining
//!   - `#[repr()]`: Controls type representation
//!
//! ```text
//!                      Active                 Inert
//!              ┌──────────────────────┬──────────────────────┐
//!              │     (mostly in)      │    these are parsed  │
//!              │ rustc_builtin_macros │        here!         │
//!              │                      │                      │
//!              │    #[derive(...)]    │    #[stable()]       │
//!     Built-in │    #[cfg()]          │    #[inline()]       │
//!              │    #[cfg_attr()]     │    #[repr()]         │
//!              │                      │                      │
//!              ├──────────────────────┼──────────────────────┤
//!              │                      │                      │
//!              │                      │       `b` in         │
//!              │                      │ #[proc_macro_derive( │
//! User created │ #[proc_macro_attr()] │    a,                │
//!              │                      │    attributes(b)     │
//!              │                      │ ]                    │
//!              └──────────────────────┴──────────────────────┘
//! ```
//!
//! ## How This Crate Works
//! In this crate, syntactical attributes (sequences of tokens that look like
//! `#[something(something else)]`) are parsed into more semantic attributes, markers on items.
//! Multiple syntactic attributes might influence a single semantic attribute. For example,
//! `#[stable(...)]` and `#[unstable()]` cannot occur together, and both semantically define
//! a "stability" of an item. So, the stability attribute has an
//! [`AttributeParser`](attributes::AttributeParser) that recognizes both the `#[stable()]`
//! and `#[unstable()]` syntactic attributes, and at the end produces a single
//! [`AttributeKind::Stability`](crate::rustc_attr_ir::AttributeKind::Stability).
//!
//! When multiple instances of the same attribute are allowed, they're combined into a single
//! semantic attribute. For example:
//!
//! ```rust
//! #[repr(C)]
//! #[repr(packed)]
//! struct Meow {}
//! ```
//!
//! This is equivalent to `#[repr(C, packed)]` and results in a single `AttributeKind::Repr`
//! containing both `C` and `packed` annotations.
//!
//! ## Early attribute parsing
//!
//! While lowering to the HIR, all attributes are parsed using the attribute parsers.
//! However, sometimes an attributes' parsed form is needed before the HIR is constructed.
//! This is referred to as "early" attribute parsing,
//! and is performed using the `parse_limited_*` family of functions on `AttributeParser`.
//!
//! [`ast::Attribute`]: crate::rustc_ast::ast::Attribute
//! [`crate::rustc_passes::check_attr`]: ../rustc_passes/check_attr/index.html

// tidy-alphabetical-start
// tidy-alphabetical-end

#![expect(internal_features, reason = "rustc_attrs")]

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
#[macro_use]
mod attributes;
mod check_cfg;
mod context;
mod diagnostics;
mod interface;
pub mod parser;
mod safety;
mod stability;
mod synthetic;
mod target_checking;
mod template;
pub mod validate_attr;

pub use attributes::AttributeSafety;
pub use attributes::cfg::{
    CFG_TEMPLATE, EvalConfigResult, eval_config_entry, parse_cfg, parse_cfg_attr, parse_cfg_entry,
};
pub use attributes::cfg_select::*;
pub use attributes::util::{is_builtin_attr, parse_version};
pub use context::ShouldEmit;
pub use diagnostics::ParsedDescription;
pub use interface::{AttributeParser, EmitAttribute};
pub use crate::rustc_parse::parser::Recovery;
pub use template::AttributeTemplate;
pub use crate::template;
pub use crate::unstable;
