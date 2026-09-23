//! Stable spellings of the unstable `Iterator` and slice methods the frontend calls.
//!
//! Nothing here may depend on nightly, so `iter_intersperse`, `iter_order_by` and
//! `array_windows` are not available. Each method below does what its core namesake does, item for item and in the same
//! order, under a different name. The name differs on purpose: if the core method is ever
//! stabilised, a same-named trait method would be shadowed by it without a word, and the
//! `unstable_name_collisions` lint fires on every call site until then.

use core::iter::Peekable;

pub trait IterExt: Iterator + Sized {
    /// `Iterator::intersperse`: the items with a clone of `sep` between each adjacent pair, and
    /// none before the first or after the last.
    fn separated_by(self, sep: Self::Item) -> SeparatedBy<Self>
    where
        Self::Item: Clone,
    {
        SeparatedBy { iter: self.peekable(), sep, needs_sep: false }
    }

    /// `Iterator::eq_by`: true when both sides have the same length and `eq` holds for every
    /// pair. Walks both in lock step and stops at the first pair that differs.
    fn eq_with<J: IntoIterator>(
        self,
        other: J,
        mut eq: impl FnMut(Self::Item, J::Item) -> bool,
    ) -> bool {
        let mut other = other.into_iter();
        for a in self {
            match other.next() {
                Some(b) => {
                    if !eq(a, b) {
                        return false;
                    }
                }
                None => return false,
            }
        }
        other.next().is_none()
    }
}

impl<I: Iterator> IterExt for I {}

/// The iterator returned by [`SliceExt::windows_array`].
pub type WindowsArray<'a, T, const N: usize> =
    core::iter::Map<core::slice::Windows<'a, T>, fn(&'a [T]) -> &'a [T; N]>;

pub trait SliceExt<T> {
    /// `slice::array_windows` (unstable `array_windows`): every run of `N` adjacent elements,
    /// as an array reference. Panics if `N` is zero, as the core method does.
    fn windows_array<const N: usize>(&self) -> WindowsArray<'_, T, N>;
}

impl<T> SliceExt<T> for [T] {
    fn windows_array<const N: usize>(&self) -> WindowsArray<'_, T, N> {
        // `windows(N)` yields exactly `N` elements each time, so the conversion cannot fail.
        self.windows(N).map(|window| window.try_into().unwrap())
    }
}

/// The iterator returned by [`IterExt::separated_by`].
pub struct SeparatedBy<I: Iterator> {
    iter: Peekable<I>,
    sep: I::Item,
    needs_sep: bool,
}

impl<I: Iterator> Iterator for SeparatedBy<I>
where
    I::Item: Clone,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        // A separator is owed after every item, but only paid when another item follows.
        if self.needs_sep && self.iter.peek().is_some() {
            self.needs_sep = false;
            Some(self.sep.clone())
        } else {
            let item = self.iter.next();
            self.needs_sep = item.is_some();
            item
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (lo, hi) = self.iter.size_hint();
        let owed = usize::from(self.needs_sep && lo > 0);
        let grow = |n: usize| n.saturating_sub(1).saturating_add(n);
        let hi_owed = usize::from(self.needs_sep);
        (
            grow(lo).saturating_add(owed),
            hi.and_then(|h| h.checked_add(h.saturating_sub(1))?.checked_add(hi_owed)),
        )
    }
}
