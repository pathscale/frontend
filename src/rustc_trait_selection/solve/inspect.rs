// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.

pub use crate::rustc_next_trait_solver::solve::inspect::*;

mod analyse;
pub use analyse::*;
