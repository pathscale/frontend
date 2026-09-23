// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::alloc::Layout;
use core::cmp::Ordering;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::ops::Deref;
use core::{fmt, iter, mem, ptr, slice};

use crate::rustc_serialize::{Encodable, Encoder};
use crate::rustc_type_ir::FlagComputation;

use super::{DebruijnIndex, TyCtxt, TypeFlags};
use crate::rustc_middle::arena::Arena;

/// `List<T>` is a bit like `&[T]`, but with some critical differences.
/// - IMPORTANT: Every `List<T>` is *required* to have unique contents. The
///   type's correctness relies on this, *but it does not enforce it*.
///   Therefore, any code that creates a `List<T>` must ensure uniqueness
///   itself. In practice this is achieved by interning.
/// - The length is stored within the `List<T>`, so `&List<Ty>` is a thin
///   pointer.
/// - Because of this, you cannot get a `List<T>` that is a sub-list of another
///   `List<T>`. You can get a sub-slice `&[T]`, however.
/// - `List<T>` can be used with `TaggedRef`, which is useful within
///   structs whose size must be minimized.
/// - Because of the uniqueness assumption, we can use the address of a
///   `List<T>` for faster equality comparisons and hashing.
/// - `T` must be `Copy`. This lets `List<T>` be stored in a dropless arena and
///   iterators return a `T` rather than a `&T`.
/// - `T` must not be zero-sized.
pub type List<T> = RawList<(), T>;

/// A generic type that can be used to prepend a [`List`] with some header.
///
/// The header will be ignored for value-based operations like [`PartialEq`],
/// [`Hash`] and [`Encodable`].
#[repr(C)]
pub struct RawList<H, T> {
    skel: ListSkeleton<H, T>,

    // `List`/`RawList` is variable-sized, and `&List`/`&RawList` must be thin pointers.
    //
    // Upstream ends the struct in an extern type, which makes it unsized (so
    // `size_of::<List<Foo>>` does not compile) while keeping references thin. Extern
    // types are unstable, so `RawList` is now a `Sized` struct whose size is the skeleton
    // alone: references are still thin, the layout and alignment are unchanged, and the
    // elements still live past the end as laid out by `from_arena`. What is lost is the
    // compile-time refusal of `size_of::<RawList<..>>()`, which would now report the
    // skeleton's size, and of moving a `RawList` by value; nothing does either, and the
    // type is neither `Copy` nor `Clone`, so a move out of a `&RawList` still fails.
    //
    // The extern type also made `RawList` `!Send`; this marker keeps it so. (`Sync` is
    // granted explicitly below, as before.)
    _not_send: PhantomData<*const ()>,
}

/// A [`RawList`] without the unsized tail. This type is used for layout computation
/// and constructing empty lists.
#[repr(C)]
struct ListSkeleton<H, T> {
    header: H,
    len: usize,
    /// Although this claims to be a zero-length array, in practice `len`
    /// elements are actually present. This is achieved with manual memory
    /// layout in `from_arena`. See also the comment on `RawList::_not_send`.
    data: [T; 0],
}

impl<T> Default for &List<T> {
    fn default() -> Self {
        List::empty()
    }
}

impl<H, T> RawList<H, T> {
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.skel.len
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[T] {
        self
    }

    // The slice methods this type used to get for free, spelled out.
    //
    // `RawList` derefs to `[T]`, and upstream that is how `list.is_empty()` resolves: autoderef
    // walks `&RawList` -> `RawList` -> `[T]` and finds the inherent slice method. Here it does
    // not, and `len` above is the reason the failure looks arbitrary - `len` is inherent on this
    // type, so it kept working while `is_empty` beside it stopped.
    //
    // The cause is `#![feature(sized_hierarchy)]`. Upstream exactly three compiler crates enable
    // it - `rustc_data_structures`, `rustc_middle` and `rustc_serialize` - and this crate is all
    // seventy of them, so a feature list that is the union of theirs turns it on everywhere.
    // Under it, autoderef wants `MetaSized` on what it steps through, and `RawList` is not:
    // it ends in an extern type, which is `PointeeSized` and nothing more. `query/erase.rs`
    // records the same fact from the other side, where it is load-bearing for interning.
    //
    // Dropping the feature is not open: `InternedInSet<T: ?Sized + PointeeSized>` interns these
    // very lists, and without it `?Sized` means `MetaSized`, which `RawList` cannot satisfy.
    // Features are crate-level, so there is no version of this that is on for three modules.
    //
    // An inherent method is found without autoderef, so these restore what `Deref` gave, with
    // the signatures `[T]` has, in the one file that owns the type. The alternative the compiler
    // suggests is importing `SliceLike` into each caller, which is ninety-seven files and a
    // different trait with a `T: Copy` bound and by-value returns.
    //
    // Update: `sized_hierarchy` and the extern type are both gone now (stable Rust has
    // neither), so `RawList` is `Sized` and autoderef reaches `[T]` again. These methods are
    // redundant but have the same signatures as the slice ones, so they are left in place.

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    #[inline(always)]
    pub fn get<I>(&self, index: I) -> Option<&I::Output>
    where
        I: core::slice::SliceIndex<[T]>,
    {
        self.as_slice().get(index)
    }

    #[inline(always)]
    pub fn first(&self) -> Option<&T> {
        self.as_slice().first()
    }

    #[inline(always)]
    pub fn last(&self) -> Option<&T> {
        self.as_slice().last()
    }

    #[inline(always)]
    pub fn split_first(&self) -> Option<(&T, &[T])> {
        self.as_slice().split_first()
    }

    #[inline(always)]
    pub fn split_last(&self) -> Option<(&T, &[T])> {
        self.as_slice().split_last()
    }

    #[inline(always)]
    pub fn contains(&self, x: &T) -> bool
    where
        T: PartialEq,
    {
        self.as_slice().contains(x)
    }

    #[inline(always)]
    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.as_slice().to_vec()
    }

    /// Allocates a list from `arena` and copies the contents of `slice` into it.
    ///
    /// WARNING: the contents *must be unique*, such that no list with these
    /// contents has been previously created. If not, operations such as `eq`
    /// and `hash` might give incorrect results.
    ///
    /// Panics if `T` is `Drop`, or `T` is zero-sized, or the slice is empty
    /// (because the empty list exists statically, and is available via
    /// `empty()`).
    #[inline]
    pub(super) fn from_arena<'tcx>(
        arena: &'tcx Arena<'tcx>,
        header: H,
        slice: &[T],
    ) -> &'tcx RawList<H, T>
    where
        T: Copy,
    {
        assert!(!mem::needs_drop::<T>());
        assert!(size_of::<T>() != 0);
        assert!(!slice.is_empty());

        let (layout, _offset) =
            Layout::new::<ListSkeleton<H, T>>().extend(Layout::for_value::<[T]>(slice)).unwrap();

        let mem = arena.dropless.alloc_raw(layout) as *mut RawList<H, T>;
        unsafe {
            // Write the header
            (&raw mut (*mem).skel.header).write(header);

            // Write the length
            (&raw mut (*mem).skel.len).write(slice.len());

            // Write the elements
            (&raw mut (*mem).skel.data)
                .cast::<T>()
                .copy_from_nonoverlapping(slice.as_ptr(), slice.len());

            &*mem
        }
    }

    // If this method didn't exist, we would use `slice.iter` due to
    // deref coercion.
    //
    // This would be weird, as `self.into_iter` iterates over `T` directly.
    #[inline(always)]
    pub fn iter(&self) -> <&'_ RawList<H, T> as IntoIterator>::IntoIter
    where
        T: Copy,
    {
        self.into_iter()
    }
}

impl<'a, H, T: Copy> crate::rustc_type_ir::inherent::SliceLike for &'a RawList<H, T> {
    type Item = T;

    type IntoIter = iter::Copied<<&'a [T] as IntoIterator>::IntoIter>;

    fn iter(self) -> Self::IntoIter {
        (*self).iter()
    }

    fn as_slice(&self) -> &[Self::Item] {
        (*self).as_slice()
    }
}

impl<'tcx> crate::rustc_type_ir::inherent::BoundVarKinds<TyCtxt<'tcx>>
    for &'tcx RawList<(), crate::rustc_middle::ty::BoundVariableKind<'tcx>>
{
    fn from_vars(
        tcx: TyCtxt<'tcx>,
        iter: impl IntoIterator<Item = crate::rustc_middle::ty::BoundVariableKind<'tcx>>,
    ) -> Self {
        tcx.mk_bound_variable_kinds_from_iter(iter.into_iter())
    }
}

macro_rules! impl_list_empty {
    ($header_ty:ty, $header_init:expr) => {
        impl<T> RawList<$header_ty, T> {
            /// Returns a reference to the (per header unique, static) empty list.
            #[inline(always)]
            pub fn empty<'a>() -> &'a RawList<$header_ty, T> {
                #[repr(align(64))]
                struct MaxAlign;

                static EMPTY: ListSkeleton<$header_ty, MaxAlign> =
                    ListSkeleton { header: $header_init, len: 0, data: [] };

                assert!(core::mem::align_of::<T>() <= core::mem::align_of::<MaxAlign>());

                // SAFETY: `EMPTY` is sufficiently aligned to be an empty list for all
                // types with `align_of(T) <= align_of(MaxAlign)`, which we checked above.
                unsafe { &*((&raw const EMPTY) as *const RawList<$header_ty, T>) }
            }
        }
    };
}

impl_list_empty!((), ());

impl<H, T: fmt::Debug> fmt::Debug for RawList<H, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<H, S: Encoder, T: Encodable<S>> Encodable<S> for RawList<H, T> {
    #[inline]
    fn encode(&self, s: &mut S) {
        (**self).encode(s);
    }
}

impl<H, T: PartialEq> PartialEq for RawList<H, T> {
    #[inline]
    fn eq(&self, other: &RawList<H, T>) -> bool {
        // Pointer equality implies list equality (due to the unique contents
        // assumption).
        ptr::eq(self, other)
    }
}

impl<H, T: Eq> Eq for RawList<H, T> {}

impl<H, T> Ord for RawList<H, T>
where
    T: Ord,
{
    fn cmp(&self, other: &RawList<H, T>) -> Ordering {
        // Pointer equality implies list equality (due to the unique contents
        // assumption), but the contents must be compared otherwise.
        if self == other { Ordering::Equal } else { <[T] as Ord>::cmp(&**self, &**other) }
    }
}

impl<H, T> PartialOrd for RawList<H, T>
where
    T: PartialOrd,
{
    fn partial_cmp(&self, other: &RawList<H, T>) -> Option<Ordering> {
        // Pointer equality implies list equality (due to the unique contents
        // assumption), but the contents must be compared otherwise.
        if self == other {
            Some(Ordering::Equal)
        } else {
            <[T] as PartialOrd>::partial_cmp(&**self, &**other)
        }
    }
}

impl<Hdr, T> Hash for RawList<Hdr, T> {
    #[inline]
    fn hash<H: Hasher>(&self, s: &mut H) {
        // Pointer hashing is sufficient (due to the unique contents
        // assumption).
        ptr::from_ref(self).hash(s)
    }
}

impl<H, T> Deref for RawList<H, T> {
    type Target = [T];
    #[inline(always)]
    fn deref(&self) -> &[T] {
        self.as_ref()
    }
}

impl<H, T> AsRef<[T]> for RawList<H, T> {
    #[inline(always)]
    fn as_ref(&self) -> &[T] {
        let data_ptr = (&raw const self.skel.data).cast::<T>();
        // SAFETY: `data_ptr` has the same provenance as `self` and can therefore
        // access the `self.skel.len` elements stored at `self.skel.data`.
        // Note that we specifically don't reborrow `&self.skel.data`, because that
        // would give us a pointer with provenance over 0 bytes.
        unsafe { slice::from_raw_parts(data_ptr, self.skel.len) }
    }
}

impl<'a, H, T: Copy> IntoIterator for &'a RawList<H, T> {
    type Item = T;
    type IntoIter = iter::Copied<<&'a [T] as IntoIterator>::IntoIter>;
    #[inline(always)]
    fn into_iter(self) -> Self::IntoIter {
        self[..].iter().copied()
    }
}

unsafe impl<H: Sync, T: Sync> Sync for RawList<H, T> {}

// Upstream grants `DynSync` here because the extern type opts `RawList` out of the auto
// impls. `DynSync` is now blanket-implemented (see `rustc_data_structures/marker.rs`), so
// a second impl would conflict.

// Upstream implements `Aligned` here as `align_of::<ListSkeleton<H, T>>()`. `RawList` is now
// `Sized` with the skeleton's alignment, so the blanket `impl<T> Aligned for T` in
// `rustc_data_structures/aligned.rs` already gives the same value and a second impl would
// conflict.

/// A [`List`] that additionally stores type information inline to speed up
/// [`TypeVisitableExt`](super::TypeVisitableExt) operations.
pub type ListWithCachedTypeInfo<T> = RawList<TypeInfo, T>;

impl<T> ListWithCachedTypeInfo<T> {
    #[inline(always)]
    pub fn flags(&self) -> TypeFlags {
        self.skel.header.flags
    }

    #[inline(always)]
    pub fn outer_exclusive_binder(&self) -> DebruijnIndex {
        self.skel.header.outer_exclusive_binder
    }
}

impl_list_empty!(TypeInfo, TypeInfo::empty());

/// The additional info that is stored in [`ListWithCachedTypeInfo`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeInfo {
    flags: TypeFlags,
    outer_exclusive_binder: DebruijnIndex,
}

impl TypeInfo {
    const fn empty() -> Self {
        Self { flags: TypeFlags::empty(), outer_exclusive_binder: super::INNERMOST }
    }
}

impl<'tcx> From<FlagComputation<TyCtxt<'tcx>>> for TypeInfo {
    fn from(computation: FlagComputation<TyCtxt<'tcx>>) -> TypeInfo {
        TypeInfo {
            flags: computation.flags,
            outer_exclusive_binder: computation.outer_exclusive_binder,
        }
    }
}

#[cfg(target_pointer_width = "64")]
mod size_asserts {
    use crate::static_assert_size;

    use super::*;
    // tidy-alphabetical-start
    static_assert_size!(&List<u32>, 8); // thin pointer
    static_assert_size!(&RawList<u8, u32>, 8); // thin pointer
    // tidy-alphabetical-end
}
