// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub mod bug;

#[derive(Default, Copy, Clone)]
pub struct Providers {
    pub queries: crate::rustc_middle::queries::Providers,
    pub extern_queries: crate::rustc_middle::queries::ExternProviders,
    pub hooks: crate::rustc_middle::hooks::Providers,
}
