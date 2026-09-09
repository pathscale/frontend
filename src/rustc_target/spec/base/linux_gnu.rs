// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_target::spec::{Cc, Env, LinkerFlavor, Lld, TargetOptions, base};

pub(crate) fn opts() -> TargetOptions {
    let mut base = TargetOptions {
        env: Env::Gnu,
        static_position_independent_executables: true,
        ..base::linux::opts()
    };

    // When we're asked to use the `rust-lld` linker by default, set the appropriate lld-using
    // linker flavor, and self-contained linker component.
    if option_env!("CFG_DEFAULT_LINKER_SELF_CONTAINED_LLD_CC").is_some() {
        base.linker_flavor = LinkerFlavor::Gnu(Cc::Yes, Lld::Yes);
        base.link_self_contained = crate::rustc_target::spec::LinkSelfContainedDefault::with_linker();
    }

    base
}
