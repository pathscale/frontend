//! Registering limits:
//! - recursion_limit: there are various parts of the compiler that must impose arbitrary limits
//!   on how deeply they recurse to prevent stack overflow.
//! - move_size_limit
//! - type_length_limit
//! - pattern_complexity_limit
//!
//! Users can override these limits via an attribute on the crate like
//! `#![recursion_limit="22"]`. This pass just looks for those attributes.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::rustc_hir::{Attribute, find_attr};
use crate::rustc_middle::query::Providers;
use crate::rustc_session::{Limits, Session};
use crate::rustc_structures::Limit;

pub(crate) fn provide(providers: &mut Providers) {
    providers.limits = |tcx, ()| {
        let attrs = tcx.hir_krate_attrs();
        Limits {
            recursion_limit: get_recursion_limit(tcx.hir_krate_attrs(), tcx.sess),
            move_size_limit: find_attr!(attrs, MoveSizeLimit { limit } => *limit)
                .unwrap_or(Limit::new(tcx.sess.opts.unstable_opts.move_size_limit.unwrap_or(0))),
            type_length_limit: find_attr!(attrs, TypeLengthLimit { limit } => *limit)
                .unwrap_or(Limit::new(2usize.pow(24))),
            pattern_complexity_limit: find_attr!(attrs, PatternComplexityLimit { limit } => *limit)
                .unwrap_or(Limit::unlimited()),
        }
    }
}

// This one is separate because it must be read prior to macro expansion.
pub(crate) fn get_recursion_limit(attrs: &[Attribute], sess: &Session) -> Limit {
    let limit_from_crate = find_attr!(attrs, RecursionLimit { limit } => limit.0).unwrap_or(128);
    Limit::new(
        sess.opts
            .unstable_opts
            .min_recursion_limit
            .map_or(limit_from_crate, |min| min.max(limit_from_crate)),
    )
}
