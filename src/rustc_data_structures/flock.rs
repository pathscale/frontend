//! Simple file-locking apis for each OS.
//!
//! This is not meant to be in the standard library, it does nothing with
//! green/native threading. This is just a bare-bones enough solution for
//! librustdoc, it is not production quality at all.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

// One platform. The `linux`, `redox` and `windows` arms are gone with their modules: each one
// reached for `std` - `OpenOptions`, `OpenOptionsExt`, `AsRawFd`, `AsRawHandle` - and none of
// them is a target this compiler builds for, so nobody could have compiled the code that would
// have told them it was broken. `windows.rs` in fact was: it named `eko::file`
// constructors that do not exist.
//
// `unix` is `fcntl(F_SETLK)` through `libc-wrapper`. `unsupported` stays for a target that has
// no locking at all.
cfg_select! {
    unix => {
        mod unix;
        use unix as imp;
    }
    _ => {
        mod unsupported;
        use unsupported as imp;
    }
}

pub use imp::Lock;
