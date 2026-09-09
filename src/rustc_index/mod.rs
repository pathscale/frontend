// tidy-alphabetical-start
#![cfg_attr(all(feature = "nightly", test), feature(stmt_expr_attributes))]
#![cfg_attr(all(feature = "nightly", test), feature(test))]
#![cfg_attr(feature = "nightly", feature(extend_one, step_trait))]

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
#[allow(unused_imports)]
#[macro_use]
// tidy-alphabetical-end

pub mod bit_set;
#[cfg(feature = "nightly")]
pub mod interval;

mod idx;
mod slice;
mod vec;

pub use idx::{Idx, IntoSliceIdx};
pub use rustc_macros::newtype_index;
pub use slice::IndexSlice;
#[doc(no_inline)]
pub use vec::IndexVec;

/// Type size assertion. The first argument is a type and the second argument is its expected size.
///
/// <div class="warning">
///
/// Emitting hard errors from size assertions like this is generally not
/// recommended, especially in libraries, because they can cause build failures if the layout
/// algorithm or dependencies change. Here in rustc we control the toolchain and layout algorithm,
/// so the former is not a problem. For the latter we have a lockfile as rustc is an application and
/// precompiled library.
///
/// Short version: Don't copy this macro into your own code. Use a `#[test]` instead.
///
/// </div>
#[macro_export]
#[cfg(not(feature = "rustc_randomized_layouts"))]
macro_rules! static_assert_size {
    ($ty:ty, $size:expr) => {
        const _: [(); $size] = [(); ::core::mem::size_of::<$ty>()];
    };
}

#[macro_export]
#[cfg(feature = "rustc_randomized_layouts")]
macro_rules! static_assert_size {
    ($ty:ty, $size:expr) => {
        // no effect other than using the statements.
        // struct sizes are not deterministic under randomized layouts
        const _: (usize, usize) = ($size, ::core::mem::size_of::<$ty>());
    };
}

#[macro_export]
macro_rules! indexvec {
    ($expr:expr; $n:expr) => {
        IndexVec::from_raw(vec![$expr; $n])
    };
    ($($expr:expr),* $(,)?) => {
        IndexVec::from_raw(vec![$($expr),*])
    };
}
pub use crate::indexvec;
pub use crate::static_assert_size;
