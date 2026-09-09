//! Calculation and management of a Strict Version Hash for crates
//!
//! The SVH is used for incremental compilation to track when HIR
//! nodes have changed between compilations, and also to detect
//! mismatches where we have two versions of the same crate that were
//! compiled from distinct sources.

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

use rustc_macros::{Decodable_NoContext, Encodable_NoContext, StableHash};

use crate::rustc_data_structures::fingerprint::Fingerprint;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
#[derive(Encodable_NoContext, Decodable_NoContext, StableHash)]
pub struct Svh {
    hash: Fingerprint,
}

impl Svh {
    /// Creates a new `Svh` given the hash. If you actually want to
    /// compute the SVH from some HIR, you want the `calculate_svh`
    /// function found in `rustc_incremental`.
    pub fn new(hash: Fingerprint) -> Svh {
        Svh { hash }
    }

    pub fn as_u128(self) -> u128 {
        self.hash.as_u128()
    }

    pub fn to_hex(self) -> String {
        format!("{:032x}", self.hash.as_u128())
    }
}

impl fmt::Display for Svh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.to_hex())
    }
}
