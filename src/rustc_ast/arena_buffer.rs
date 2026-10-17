//! Owned list elements in a contiguous, chunk-backed header and element allocation.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::{Bound, RangeBounds};
use core::ptr::NonNull;
use core::{fmt, ptr, slice};

use crate::rustc_ast::ptr::{
    alloc_slice_bytes, alloc_slice_bytes_in, current_slot, dealloc_slice_bytes,
    try_grow_in_chunk_in_place_in, with_drop_batch,
};
use crate::rustc_data_structures::flat_map_in_place::FlatMapInPlaceVec;

#[repr(C)]
struct BufferHeader<T> {
    len: usize,
    cap: usize,
    _marker: PhantomData<T>,
}

#[repr(transparent)]
pub(crate) struct ArenaBuffer<T>(Option<NonNull<BufferHeader<T>>>);

const _: () = assert!(core::mem::size_of::<ArenaBuffer<u32>>() == core::mem::size_of::<usize>());

// Own the allocation separately so element destruction can unwind without leaking storage.
struct Storage(NonNull<u8>, Layout);

impl Drop for Storage {
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns the live allocation with its original size and
        // alignment. All initialized elements have been dropped or moved before release.
        unsafe { dealloc_slice_bytes(self.0, self.1) };
    }
}

impl<T> ArenaBuffer<T> {
    fn layout(capacity: usize) -> (Layout, usize) {
        let elements = Layout::array::<MaybeUninit<T>>(capacity).expect("AST list too large");
        let (layout, offset) =
            Layout::new::<BufferHeader<T>>().extend(elements).expect("AST list too large");
        (layout.pad_to_align(), offset)
    }

    pub(crate) fn new() -> Self {
        Self(None)
    }

    /// Move a fully initialized slice into one exact-sized allocation.
    ///
    /// SAFETY: the caller must relinquish ownership of all source elements
    /// immediately after this returns, without running their destructors.
    unsafe fn from_raw_parts_move(source: *const T, len: usize) -> Self {
        if len == 0 {
            return Self::new();
        }
        let (layout, offset) = Self::layout(len);
        let storage = alloc_slice_bytes(layout);
        let header = storage.cast::<BufferHeader<T>>();
        // SAFETY: layout reserves aligned, disjoint storage for the header and
        // len elements. The caller owns the initialized source slice and transfers
        // it exactly once; the allocation cannot overlap that live source.
        unsafe {
            header.as_ptr().write(BufferHeader { len, cap: len, _marker: PhantomData });
            ptr::copy_nonoverlapping(source, storage.as_ptr().add(offset).cast::<T>(), len);
        }
        Self(Some(header))
    }

    pub(crate) fn from_vec(mut values: Vec<T>) -> Self {
        // SAFETY: values uniquely owns its initialized prefix. No fallible work
        // occurs between transferring ownership and clearing the source length.
        let result = unsafe { Self::from_raw_parts_move(values.as_ptr(), values.len()) };
        // SAFETY: every element now belongs to result; only Vec storage is freed.
        unsafe { values.set_len(0) };
        result
    }

    pub(crate) fn from_smallvec<A: smallvec::Array<Item = T>>(
        mut values: smallvec::SmallVec<A>,
    ) -> Self {
        // SAFETY: values uniquely owns the initialized prefix being transferred.
        let result = unsafe { Self::from_raw_parts_move(values.as_ptr(), values.len()) };
        // SAFETY: result now owns all elements, including inline and ZST elements.
        unsafe { values.set_len(0) };
        result
    }

    pub(crate) fn from_thin_vec(mut values: thin_vec::ThinVec<T>) -> Self {
        // SAFETY: values uniquely owns the initialized prefix being transferred.
        let result = unsafe { Self::from_raw_parts_move(values.as_ptr(), values.len()) };
        // SAFETY: result now owns every element; release source storage only.
        unsafe { values.set_len(0) };
        result
    }

    pub(crate) fn from_scratch(values: &mut Vec<T>, start: usize) -> Self {
        assert!(start <= values.len());
        // SAFETY: start..len is initialized and exclusively borrowed. The copy
        // transfers that tail while keeping the reusable source allocation live.
        let result =
            unsafe { Self::from_raw_parts_move(values.as_ptr().add(start), values.len() - start) };
        // SAFETY: only the untouched prefix remains owned by values.
        unsafe { values.set_len(start) };
        result
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut values = Self::new();
        values.reserve_exact(capacity);
        values
    }

    pub(crate) fn from_elem(element: T, count: usize) -> Self
    where
        T: Clone,
    {
        let mut values = Self::with_capacity(count);
        if count != 0 {
            for _ in 1..count {
                values.push(element.clone());
            }
            values.push(element);
        }
        values
    }

    #[expect(
        dead_code,
        reason = "retains the spill-state query for arena-backed AST list consumers"
    )]
    pub(crate) fn spilled(&self) -> bool {
        self.0.is_some()
    }

    #[expect(dead_code, reason = "retains the empty-list query used by AST list integrations")]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn len(&self) -> usize {
        self.0.map_or(0, |header| {
            // SAFETY: every stored pointer is aligned to an initialized, live header;
            // this shared borrow reads only that header, never moved-out element slots.
            unsafe { (*header.as_ptr()).len }
        })
    }

    fn capacity(&self) -> usize {
        self.0.map_or(0, |header| {
            // SAFETY: the pointer addresses an aligned, initialized header within our
            // live allocation. Shared access reads only the header's capacity field.
            unsafe { (*header.as_ptr()).cap }
        })
    }

    fn data_ptr(&self) -> *mut T {
        match self.0 {
            Some(header) => {
                let offset = Self::layout(0).1;
                // SAFETY: layout composition aligns this offset for T and places it
                // within the allocation, or at its end for ZSTs. This creates a raw
                // storage pointer only, with no reference to uninitialized elements.
                unsafe { header.as_ptr().cast::<u8>().add(offset).cast::<T>() }
            }
            None => NonNull::<T>::dangling().as_ptr(),
        }
    }

    // Internal callers maintain the initialized-prefix invariant, including ownership
    // transfers to raw-pointer operations and IntoIter. No element reference is formed.
    fn update_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity());
        if let Some(header) = self.0 {
            // SAFETY: &mut self uniquely owns this aligned, initialized header; the
            // field write stays in bounds and does not borrow any element storage.
            unsafe { (*header.as_ptr()).len = len };
        } else {
            debug_assert_eq!(len, 0);
        }
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        // SAFETY: the pointer is nonnull and aligned even when empty or zero-sized.
        // Exactly len elements are initialized in the checked allocation, and the
        // shared self borrow prevents mutation for the returned slice's lifetime.
        unsafe { slice::from_raw_parts(self.data_ptr(), self.len()) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: the pointer is nonnull and aligned, and len describes only the
        // initialized prefix within the checked layout. &mut self grants exclusive
        // access for the returned slice's lifetime, including empty/ZST slices.
        unsafe { slice::from_raw_parts_mut(self.data_ptr(), self.len()) }
    }

    /// Ensure an absolute capacity, rather than reserving an additional count.
    pub(crate) fn reserve_exact(&mut self, capacity: usize) {
        let old_capacity = self.capacity();
        if capacity <= old_capacity {
            return;
        }
        let (new_layout, offset) = Self::layout(capacity);
        let len = self.len();
        let slot = current_slot();
        if let Some(header) = self.0 {
            let (old_layout, _) = Self::layout(old_capacity);
            // SAFETY: header uniquely owns a live slice-family allocation described
            // by old_layout. The new layout has the same alignment and no smaller
            // size, and there are no live header or element references during growth.
            let grew = unsafe {
                try_grow_in_chunk_in_place_in(header.cast(), old_layout, new_layout, slot)
            };
            if grew || old_layout == new_layout {
                // Equal layouts also cover ZST growth away from the active chunk tip.
                // SAFETY: growth preserved the aligned, initialized header and its
                // unique ownership; this in-bounds field write touches no elements.
                unsafe { (*header.as_ptr()).cap = capacity };
                return;
            }
        }

        let data = alloc_slice_bytes_in(new_layout, slot);
        let header = data.cast::<BufferHeader<T>>();
        // SAFETY: new_layout provides correctly aligned, disjoint storage for the
        // header and capacity elements. The old len-element prefix is initialized
        // and uniquely owned; copying transfers ownership without creating element
        // references. For ZSTs both regions contain zero bytes and remain aligned.
        unsafe {
            header.as_ptr().write(BufferHeader { len, cap: capacity, _marker: PhantomData });
            ptr::copy_nonoverlapping(self.data_ptr(), data.as_ptr().add(offset).cast::<T>(), len);
        }
        let old = self.0.replace(header);
        if let Some(old) = old {
            // SAFETY: old is the uniquely owned, aligned original allocation with
            // its exact layout. All initialized elements were moved to the new
            // storage, and no references or owning pointers to old storage remain.
            unsafe { dealloc_slice_bytes(old.cast(), Self::layout(old_capacity).0) };
        }
    }

    fn reserve_one(&mut self) {
        let len = self.len();
        if len == self.capacity() {
            let needed = len.checked_add(1).expect("AST list too large");
            self.reserve_exact(needed.max(len.saturating_mul(2)).max(4));
        }
    }

    pub(crate) fn push(&mut self, value: T) {
        self.reserve_one();
        let len = self.len();
        // SAFETY: reserve_one ensures an aligned, uniquely owned slot at len within
        // capacity. That slot is outside the initialized prefix and written once.
        unsafe { self.data_ptr().add(len).write(value) };
        self.update_len(len + 1);
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        let len = self.len().checked_sub(1)?;
        self.update_len(len);
        // SAFETY: the aligned last slot was initialized and in bounds. It is now
        // excluded from the prefix, and unique ownership transfers to the result.
        Some(unsafe { self.data_ptr().add(len).read() })
    }

    pub(crate) fn insert(&mut self, index: usize, value: T) {
        let len = self.len();
        assert!(index <= len);
        self.reserve_one();
        // SAFETY: the aligned storage is uniquely owned and has len + 1 slots.
        // The initialized tail shifts within capacity using an overlap-safe copy;
        // writing value replaces the duplicated slot, leaving one owner per value.
        unsafe {
            let at = self.data_ptr().add(index);
            ptr::copy(at, at.add(1), len - index);
            at.write(value);
        }
        self.update_len(len + 1);
    }

    pub(crate) fn remove(&mut self, index: usize) -> T {
        let len = self.len();
        assert!(index < len);
        // SAFETY: index is aligned, initialized, uniquely owned and in bounds.
        // Moving it out and overlap-copying the initialized tail keeps every live
        // slot in bounds; reducing len excludes the duplicated final slot.
        unsafe {
            let at = self.data_ptr().add(index);
            let value = at.read();
            ptr::copy(at.add(1), at, len - index - 1);
            self.update_len(len - 1);
            value
        }
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        let old_len = self.len();
        if len < old_len {
            self.update_len(len);
            // SAFETY: the aligned tail is initialized and within the allocation.
            // Exclusive ownership transfers to slice drop after excluding it from
            // len, so unwinding cannot cause a second drop of the same elements.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                    self.data_ptr().add(len),
                    old_len - len,
                ));
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.truncate(0);
    }

    pub(crate) fn extend<I: IntoIterator<Item = T>>(&mut self, values: I) {
        let values = values.into_iter();
        let (lower, upper) = values.size_hint();
        if upper == Some(lower) {
            let needed = self.len().checked_add(lower).expect("AST list too large");
            self.reserve_exact(needed);
        }
        for value in values {
            // Each push checks capacity even if an iterator supplies an incorrect hint.
            self.push(value);
        }
    }

    #[expect(
        dead_code,
        reason = "retains the range replacement operation for arena-backed AST list consumers"
    )]
    pub(crate) fn splice<R, I>(&mut self, range: R, replace_with: I) -> alloc::vec::IntoIter<T>
    where
        R: RangeBounds<usize>,
        I: IntoIterator<Item = T>,
    {
        let len = self.len();
        let start = match range.start_bound() {
            Bound::Included(&index) => index,
            Bound::Excluded(&index) => index.checked_add(1).expect("AST list range overflow"),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&index) => index.checked_add(1).expect("AST list range overflow"),
            Bound::Excluded(&index) => index,
            Bound::Unbounded => len,
        };
        assert!(start <= end && end <= len, "AST list range out of bounds");
        // Finish user iteration and all fallible allocation before moving any slots.
        let replacement: Vec<T> = replace_with.into_iter().collect();
        let new_len =
            (len - (end - start)).checked_add(replacement.len()).expect("AST list too large");
        let mut removed = Vec::with_capacity(end - start);
        self.reserve_exact(new_len);
        let tail_start = start + replacement.len();
        let data = self.data_ptr();
        self.update_len(start);
        // SAFETY: storage is aligned and uniquely owned; the old prefix and tail
        // are initialized, and max(len, new_len) fits capacity. Removed values move
        // once into a preallocated Vec. Overlap-safe copy moves the tail to its
        // final in-bounds position, and replacement writes fill the gap. No user
        // code or allocation runs while slots are moved out of the live prefix.
        unsafe {
            for index in start..end {
                removed.push(data.add(index).read());
            }
            ptr::copy(data.add(end), data.add(tail_start), len - end);
            for (index, value) in replacement.into_iter().enumerate() {
                data.add(start + index).write(value);
            }
        }
        self.update_len(new_len);
        removed.into_iter()
    }
}

impl<T: Clone> Clone for ArenaBuffer<T> {
    fn clone(&self) -> Self {
        let mut values = Self::with_capacity(self.len());
        values.extend(self.as_slice().iter().cloned());
        values
    }
}

impl<T: fmt::Debug> fmt::Debug for ArenaBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_slice().fmt(f)
    }
}

impl<T> Drop for ArenaBuffer<T> {
    fn drop(&mut self) {
        let Some(header) = self.0 else { return };
        with_drop_batch(|| {
            let _storage = Storage(header.cast(), Self::layout(self.capacity()).0);
            // SAFETY: the aligned allocation uniquely owns exactly the initialized
            // len-element prefix. The guard releases its exact allocation even if
            // slice drop unwinds, and no element references outlive this owner.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.data_ptr(), self.len()));
            }
        });
    }
}

impl<T> FromIterator<T> for ArenaBuffer<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        let mut buffer = Self::new();
        buffer.extend(values);
        buffer
    }
}

// SAFETY: the allocation is uniquely owned, aligned and bounds-checked; moving the
// owner transfers only T: Send elements. Slice-family reclamation supports transfer.
unsafe impl<T: Send> Send for ArenaBuffer<T> {}
// SAFETY: shared access exposes only the initialized prefix as shared T: Sync
// elements and reads immutable header fields. Mutation requires exclusive access.
unsafe impl<T: Sync> Sync for ArenaBuffer<T> {}

// SAFETY: len and capacity describe aligned owned storage, and pointers address
// storage directly even while the blanket FlatMapInPlace algorithm has moved out
// elements. Its unsafe set_len contract restores the initialized-prefix invariant.
unsafe impl<T> FlatMapInPlaceVec for ArenaBuffer<T> {
    type Elem = T;

    fn len(&self) -> usize {
        ArenaBuffer::len(self)
    }

    // SAFETY: the caller must ensure len <= capacity and that exactly the new
    // prefix is initialized and uniquely owned. Updating the aligned header stays
    // within its live allocation and does not form references to moved-out slots.
    unsafe fn set_len(&mut self, len: usize) {
        self.update_len(len);
    }

    fn as_ptr(&self) -> *const T {
        self.data_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut T {
        self.data_ptr()
    }

    fn insert(&mut self, index: usize, value: T) {
        self.insert(index, value);
    }
}

pub struct IntoIter<T> {
    values: ArenaBuffer<T>,
    front: usize,
    back: usize,
}

impl<T> IntoIterator for ArenaBuffer<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(mut self) -> IntoIter<T> {
        let back = self.len();
        // The iterator takes ownership of all initialized slots outside buffer len.
        self.update_len(0);
        IntoIter { values: self, front: 0, back }
    }
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        let index = self.front;
        self.front += 1;
        // SAFETY: index is an aligned initialized slot in the iterator's remaining
        // in-bounds range, uniquely owned and removed from that range before read.
        Some(unsafe { self.values.data_ptr().add(index).read() })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.back - self.front;
        (len, Some(len))
    }
}

impl<T> DoubleEndedIterator for IntoIter<T> {
    fn next_back(&mut self) -> Option<T> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        // SAFETY: back now identifies an aligned initialized slot within the owned
        // allocation, excluded from the remaining range before its unique move.
        Some(unsafe { self.values.data_ptr().add(self.back).read() })
    }
}

impl<T> ExactSizeIterator for IntoIter<T> {}
impl<T> core::iter::FusedIterator for IntoIter<T> {}

impl<T> Drop for IntoIter<T> {
    fn drop(&mut self) {
        if self.front == self.back {
            return;
        }
        with_drop_batch(|| {
            // Keep storage destruction inside the same batch, including unwinding.
            let values = core::mem::replace(&mut self.values, ArenaBuffer::new());
            // SAFETY: front..back is the aligned, initialized, uniquely owned remainder
            // within the allocation. Buffer len is zero, so its subsequent drop only
            // frees storage, including when destruction of a remaining element unwinds.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                    values.data_ptr().add(self.front),
                    self.back - self.front,
                ));
            }
        });
    }
}
