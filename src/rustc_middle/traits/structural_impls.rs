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

use crate::rustc_middle::traits;

// Structural impls for the structs in `traits`.

impl<'tcx, N: fmt::Debug> fmt::Debug for traits::ImplSource<'tcx, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            super::ImplSource::UserDefined(v) => write!(f, "{v:?}"),

            super::ImplSource::Builtin(source, d) => {
                write!(f, "Builtin({source:?}, {d:?})")
            }

            super::ImplSource::Param(n) => {
                write!(f, "ImplSourceParamData({n:?})")
            }
        }
    }
}

impl<'tcx, N: fmt::Debug> fmt::Debug for traits::ImplSourceUserDefinedData<'tcx, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ImplSourceUserDefinedData(impl_def_id={:?}, args={:?}, nested={:?})",
            self.impl_def_id, self.args, self.nested
        )
    }
}
