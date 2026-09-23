//! Various data structures used by the Rust compiler. The intention
//! is that code in here should not be *specific* to rustc, so that
//! it can be easily unit tested and so forth.
//!
//! # Note
//!
//! This API is completely unstable and subject to change.

// tidy-alphabetical-start
// tidy-alphabetical-end

// This allows derive macros to reference this crate
#![allow(internal_features)]
#![cfg_attr(test, feature(test))]
#![deny(unsafe_op_in_unsafe_fn)]
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
// Real std remains: `Mutex`, `mpsc` and `thread` are the parallelism primitives the
// frontend is built on, `HashMap` has no alloc equivalent, and `io`/`Path` back the profiler.
// This is the crate to route through `libc-wrapper` when the binary must stop linking std.

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

extern crate self as rustc_data_structures;

use core::fmt;

pub use atomic_ref::AtomicRef;
pub use crate::ena::{snapshot_vec, undo_log, unify};
// Re-export `hashbrown::hash_table`, because it's part of our API
// (via `ShardedHashMap`), and because it lets other compiler crates use the
// lower-level `HashTable` API without a tricky `hashbrown` dependency.
pub use hashbrown::hash_table;
pub use crate::static_assert_size;
// Re-export some data-structure crates which are part of our public API.
pub use {either, indexmap, smallvec, thin_vec};
pub mod aligned;
pub mod base_n;
pub mod binary_search_util;
pub mod fingerprint;
pub mod flat_map_in_place;
pub mod flock;
pub mod frozen;
pub mod fx;
pub mod graph;
pub mod intern;
pub mod iter_ext;
pub mod marker;
pub mod memmap;
pub mod obligation_forest;
pub mod owned_slice;
pub mod packed;
pub mod profiling;
pub mod range_set;
pub mod sharded;
pub mod small_c_str;
pub mod snapshot_map;
pub mod sorted_map;
pub mod sso;
pub mod stable_hash;
pub mod steal;
pub mod svh;
pub mod sync;
pub mod tagged_ptr;
pub mod temp_dir;
pub mod thousands;
pub mod transitive_relation;
pub mod unhash;
pub mod union_find;
pub mod unord;
pub mod vec_cache;

mod assert_matches;
mod atomic_ref;

/// This calls the passed function while ensuring it won't be inlined into the caller.
#[inline(never)]
#[cold]
pub fn outline<F: FnOnce() -> R, R>(f: F) -> R {
    f()
}

/// Returns a structure that calls `f` when dropped.
pub fn defer<F: FnOnce()>(f: F) -> OnDrop<F> {
    OnDrop(Some(f))
}

pub struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    /// Disables on-drop call.
    #[inline]
    pub fn disable(mut self) {
        self.0.take();
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    #[inline]
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// This is a marker for a fatal compiler error used with `resume_unwind`.
pub struct FatalErrorMarker;

/// Turns a closure that takes an `&mut Formatter` into something that can be display-formatted.
pub fn make_display(f: impl Fn(&mut fmt::Formatter<'_>) -> fmt::Result) -> impl fmt::Display {
    struct Printer<F> {
        f: F,
    }
    impl<F> fmt::Display for Printer<F>
    where
        F: Fn(&mut fmt::Formatter<'_>) -> fmt::Result,
    {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            (self.f)(fmt)
        }
    }

    Printer { f }
}

// See comment in compiler/rustc_middle/src/tests.rs and issue #27438.
#[doc(hidden)]
pub fn __noop_fix_for_windows_dllimport_issue() {}

/// Stable stand-in for `${ignore($x)}` (unstable `macro_metavar_expr`).
///
/// Inside `$( ... )?` a repetition is driven by the metavariables it mentions, and `${ignore}`
/// mentioned one without emitting it. `$crate::metavar_ignore!([$x] tokens)` mentions `$x` in
/// the brackets and expands to just `tokens`. Being a macro call, it only works where a macro
/// call is allowed: item, statement, expression, pattern and type positions.
#[doc(hidden)]
#[macro_export]
macro_rules! metavar_ignore {
    ([$($ignored:tt)*] $($keep:tt)*) => {
        $($keep)*
    };
}

#[macro_export]
macro_rules! external_bitflags_debug {
    ($Name:ident) => {
        impl ::core::fmt::Debug for $Name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::bitflags::parser::to_writer(self, f)
            }
        }
    };
}
pub use crate::define_id_collections;
pub use crate::define_stable_id_collections;
pub use crate::external_bitflags_debug;
