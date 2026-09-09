/// A macro for triggering an ICE.
/// Calling `bug` instead of panicking will result in a nicer error message and should
/// therefore be preferred over `panic`/`unreachable` or others.
///
/// If you have a span available, you should use [`span_bug`] instead.
///
/// If the bug should only be emitted when compilation didn't fail,
/// [`DiagCtxtHandle::span_delayed_bug`] may be useful.
///
/// [`DiagCtxtHandle::span_delayed_bug`]: crate::rustc_errors::DiagCtxtHandle::span_delayed_bug
/// [`span_bug`]: crate::span_bug
// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

#[macro_export]
macro_rules! bug {
    () => (
        $crate::bug!("impossible case reached")
    );
    ($($arg:tt)+) => (
        $crate::rustc_middle::util::bug::bug_fmt(::core::format_args!($($arg)+))
    );
}

/// A macro for triggering an ICE with a span.
/// Calling `span_bug!` instead of panicking will result in a nicer error message and point
/// at the code the compiler was compiling when it ICEd. This is the preferred way to trigger
/// ICEs.
///
/// If the bug should only be emitted when compilation didn't fail,
/// [`DiagCtxtHandle::span_delayed_bug`] may be useful.
///
/// [`DiagCtxtHandle::span_delayed_bug`]: crate::rustc_errors::DiagCtxtHandle::span_delayed_bug
#[macro_export]
macro_rules! span_bug {
    ($span:expr, $($arg:tt)+) => (
        $crate::rustc_middle::util::bug::span_bug_fmt($span, ::core::format_args!($($arg)+))
    );
}

///////////////////////////////////////////////////////////////////////////
// Lift and TypeFoldable/TypeVisitable macros
//
// When possible, use one of these (relatively) convenient macros to write
// the impls for you.

macro_rules! TrivialLiftImpls {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl<'tcx> $crate::rustc_middle::ty::Lift<$crate::rustc_middle::ty::TyCtxt<'tcx>> for $ty {
                type Lifted = Self;
                fn lift_to_interner(self, _: $crate::rustc_middle::ty::TyCtxt<'tcx>) -> Self {
                    self
                }
            }
        )+
    };
}

/// Used for types that are `Copy` and which **do not care about arena
/// allocated data** (i.e., don't need to be folded).
macro_rules! TrivialTypeTraversalImpls {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl<'tcx> $crate::rustc_middle::ty::TypeFoldable<$crate::rustc_middle::ty::TyCtxt<'tcx>> for $ty {
                fn try_fold_with<F: $crate::rustc_middle::ty::FallibleTypeFolder<$crate::rustc_middle::ty::TyCtxt<'tcx>>>(
                    self,
                    _: &mut F,
                ) -> ::core::result::Result<Self, F::Error> {
                    Ok(self)
                }

                #[inline]
                fn fold_with<F: $crate::rustc_middle::ty::TypeFolder<$crate::rustc_middle::ty::TyCtxt<'tcx>>>(
                    self,
                    _: &mut F,
                ) -> Self {
                    self
                }
            }

            impl<'tcx> $crate::rustc_middle::ty::TypeVisitable<$crate::rustc_middle::ty::TyCtxt<'tcx>> for $ty {
                #[inline]
                fn visit_with<F: $crate::rustc_middle::ty::TypeVisitor<$crate::rustc_middle::ty::TyCtxt<'tcx>>>(
                    &self,
                    _: &mut F)
                    -> F::Result
                {
                    <F::Result as $crate::rustc_middle::ty::VisitorResult>::output()
                }
            }
        )+
    };
}

macro_rules! TrivialTypeTraversalAndLiftImpls {
    ($($t:tt)*) => {
        TrivialTypeTraversalImpls! { $($t)* }
        TrivialLiftImpls! { $($t)* }
    }
}
