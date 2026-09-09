// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_hir::def_id::DefId;
pub use crate::rustc_type_ir::fast_reject::*;

use super::TyCtxt;

pub type DeepRejectCtxt<
    'tcx,
    const INSTANTIATE_LHS_WITH_INFER: bool,
    const INSTANTIATE_RHS_WITH_INFER: bool,
> = crate::rustc_type_ir::fast_reject::DeepRejectCtxt<
    TyCtxt<'tcx>,
    INSTANTIATE_LHS_WITH_INFER,
    INSTANTIATE_RHS_WITH_INFER,
>;

pub type SimplifiedType = crate::rustc_type_ir::fast_reject::SimplifiedType<DefId>;
