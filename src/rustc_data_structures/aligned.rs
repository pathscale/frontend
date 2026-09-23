// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::num::NonZero;

/// A power-of-two alignment in bytes.
///
/// A local stand-in for `core::mem::Alignment`, which is the unstable `ptr_alignment_type`. It
/// carries only what the tagged-pointer code reads back out of it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Alignment(NonZero<usize>);

impl Alignment {
    /// The alignment of `T`.
    pub const fn of<T>() -> Self {
        match NonZero::new(core::mem::align_of::<T>()) {
            Some(align) => Alignment(align),
            None => unreachable!(),
        }
    }

    pub const fn as_nonzero_usize(self) -> NonZero<usize> {
        self.0
    }
}

/// Returns the ABI-required minimum alignment of a type in bytes.
///
/// This is equivalent to [`align_of`], but also works for some unsized
/// types (e.g. slices or rustc's `List`s).
pub const fn align_of<T: ?Sized + Aligned>() -> Alignment {
    T::ALIGN
}

/// A type with a statically known alignment.
///
/// # Safety
///
/// `Self::ALIGN` must be equal to the alignment of `Self`. For sized types it
/// is [`align_of::<Self>()`], for unsized types it depends on the type, for
/// example `[T]` has alignment of `T`.
///
/// [`align_of::<Self>()`]: align_of
// Upstream bounds this `: PointeeSized` (unstable `sized_hierarchy`); a trait's `Self` is
// already `?Sized`, which is the stable meaning, so the bound is simply dropped.
pub unsafe trait Aligned {
    /// Alignment of `Self`.
    const ALIGN: Alignment;
}

unsafe impl<T> Aligned for T {
    const ALIGN: Alignment = Alignment::of::<Self>();
}

unsafe impl<T> Aligned for [T] {
    const ALIGN: Alignment = Alignment::of::<T>();
}
