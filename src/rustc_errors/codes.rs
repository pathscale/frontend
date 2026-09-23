//! This module defines the following.
//! - The `ErrCode` type.
//! - A constant for every error code, with a name like `E0123`.
//! - A static table `DIAGNOSTICS` pairing every error code constant with its
//!   long description text.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::fmt;


crate::rustc_index::newtype_index! {
    #[max = 9999] // Because all error codes have four digits.
    #[orderable]
    #[encodable]
    #[debug_format = "ErrCode({})"]
    pub struct ErrCode {}
}

impl fmt::Display for ErrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "E{:04}", self.as_u32())
    }
}

crate::into_diag_arg_using_display!(ErrCode);

// The long-form explanations that used to sit beside these constants are gone with `--explain`,
// which was their only reader (`try_find_description` had no other caller). They were 518
// `error_codes/EXXXX.md` files - 50 KB of text in 2 MB of 4 KB blocks - `include_str!`d into a
// `LazyLock<FxHashMap<ErrCode, &str>>` built at first use.
//
// The *code* is the part that matters and it stays: `DiagInner::code` is an `ErrCode`, and a
// client that wants prose for E0308 can carry its own table keyed on the number. The identifier
// is the machine-readable half; the English is the renderer's problem.
macro_rules! define_error_code_constants_and_diagnostics_table {
    // The constant names used to come from `${concat(E, $num)}` (unstable); the proc macro
    // builds the same `ENNNN` identifiers from the literals.
    ($($num:literal,)*) => (
        rustc_macros::error_code_constants!($($num,)*);
    )
}

// Invoked by bare name rather than as `crate::error_codes!`. `#[macro_export]` puts the macro in
// the crate root's macro namespace, and reaching one that way from inside its own crate is the
// hard error `macro_expanded_macro_exports_accessed_by_absolute_paths`. Textual scope reaches it
// instead, which works because `rustc_error_codes` is declared before `rustc_errors` in `lib.rs`.
error_codes!(define_error_code_constants_and_diagnostics_table);

