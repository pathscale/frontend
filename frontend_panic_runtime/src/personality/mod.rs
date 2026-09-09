//! The `eh_personality` lang item, ported from `library/std/src/sys/personality/`.
//!
//! # Why this is here and not a dependency
//!
//! It is the *only* thing `std` owns that a `no_std` binary needs in order to unwind.
//! `__rust_start_panic` and `__rust_panic_cleanup` come from `panic_unwind`, and
//! `_Unwind_RaiseException`/`_Unwind_Resume` from `unwind`; neither of those crates links `std`.
//! But `eh_personality` is a weak lang item that only `std` defines, and
//! `rustc_passes/src/weak_lang_items.rs` rejects the whole build when it is missing, with a
//! message that names `std` because `std` is where it usually comes from.
//!
//! # It is a copy, deliberately
//!
//! `gcc.rs` and `dwarf/` are `library/std/src/sys/personality/` unchanged except for one line:
//! `use crate::ffi::c_int` became `use core::ffi::c_int`. That was the only `std` path in 741
//! lines. Copying rather than reimplementing is the same argument as `crates/vendors/odht`: this
//! code has to agree exactly with what LLVM emitted into the `.eh_frame` and `.gnu_extab`
//! sections of every object in the binary, and "agrees with the specification" is a weaker claim
//! than "is the code that has been agreeing with it in every Rust program ever shipped".
//!
//! Keep the diff to one line. When the toolchain is bumped, re-copy rather than patch.
mod dwarf;

// aarch64-apple-darwin is `target_family = "unix"`, which is the arm `std`'s own `cfg_select`
// sends to `gcc.rs`. The other arms are MSVC and wasm (aborting stubs, since the platform
// personality is used instead) and the bare-metal targets that do not unwind at all. Neither
// applies to anything this compiler is built for, so the dispatch is not reproduced.
mod gcc;
