// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Vendored from ena 0.14.4, converted to `no_std`. See `vendor/ena/VENDORED.md`.
//
// The conversion is a rename plus three deletions: every `std::` path in this crate was a
// `core` item except `Vec`, which comes from `alloc`, and the `persistent` feature, the
// `bench` feature and the test modules are gone. No algorithm is touched.

//! An implementation of union-find. See the `unify` module for more
//! details.



pub mod snapshot_vec;
pub mod undo_log;
pub mod unify;
