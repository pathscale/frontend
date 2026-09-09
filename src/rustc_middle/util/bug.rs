// These functions are used by macro expansion for `bug!` and `span_bug!`.

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
use core::panic::Location;

use crate::rustc_errors::MultiSpan;
use crate::rustc_span::Span;

use crate::rustc_middle::ty::{TyCtxt, tls};

// This wrapper makes for more compact code at callsites than calling `opt_span_buf_fmt` directly.
#[cold]
#[inline(never)]
#[track_caller]
pub fn bug_fmt(args: fmt::Arguments<'_>) -> ! {
    opt_span_bug_fmt(None::<Span>, args, Location::caller());
}

// This wrapper makes for more compact code at callsites than calling `opt_span_buf_fmt` directly.
#[cold]
#[inline(never)]
#[track_caller]
pub fn span_bug_fmt<S: Into<MultiSpan>>(span: S, args: fmt::Arguments<'_>) -> ! {
    opt_span_bug_fmt(Some(span), args, Location::caller());
}

#[track_caller]
fn opt_span_bug_fmt<S: Into<MultiSpan>>(
    span: Option<S>,
    args: fmt::Arguments<'_>,
    location: &Location<'_>,
) -> ! {
    tls::with_opt(
        #[track_caller]
        move |tcx| {
            let msg = format!("{location}: {args}");
            match (tcx, span) {
                (Some(tcx), Some(span)) => tcx.dcx().span_bug(span, msg),
                (Some(tcx), None) => tcx.dcx().bug(msg),
                // Was `panic_any(msg)`, which carried the `String` as the panic payload for a
                // catcher to downcast. **CONTAINMENT DROPPED, `panic = "abort"`:** nothing can
                // catch a payload any more, so the message is formatted into the panic instead
                // of being carried as one.
                (None, _) => panic!("{msg}"),
            }
        },
    )
}

/// A query to trigger a delayed bug. Clearly, if one has a `tcx` one can already trigger a
/// delayed bug, so what is the point of this? It exists to help us test the interaction of delayed
/// bugs with the query system and incremental.
pub fn trigger_delayed_bug(tcx: TyCtxt<'_>, key: crate::rustc_hir::def_id::DefId) {
    tcx.dcx().span_delayed_bug(
        tcx.def_span(key),
        "delayed bug triggered by #[rustc_delayed_bug_from_inside_query]",
    );
}

pub fn provide(providers: &mut crate::rustc_middle::query::Providers) {
    *providers = crate::rustc_middle::query::Providers { trigger_delayed_bug, ..*providers };
}
