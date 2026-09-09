// FIXME(#27438): Right now, the unit tests of `rustc_middle` don't refer to any actual functions
//                generated in `rustc_data_structures` (all references are through generic functions),
//                but statics are referenced from time to time. Due to this Windows `dllimport` bug
//                we won't actually correctly link in the statics unless we also reference a function,
//                so be sure to reference a dummy function.
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

#[test]
fn noop() {
    crate::rustc_data_structures::__noop_fix_for_windows_dllimport_issue();
}
