//! Inline list elements with owned, chunk-backed spill storage.

use core::alloc::Layout;
use core::ptr::NonNull;
use core::{fmt, mem, ptr, slice};
use smallvec::SmallVec;

use crate::rustc_ast::ptr::{alloc_list_bytes, dealloc_list_bytes};
use crate::rustc_data_structures::flat_map_in_place::FlatMapInPlaceVec;

struct Spill<T> {
    data: NonNull<T>,
    len: usize,
    capacity: usize,
}

impl<T> Spill<T> {
    fn layout(capacity: usize) -> Layout {
        Layout::array::<T>(capacity).expect("AST list too large")
    }

    fn new(capacity: usize) -> Self {
        Self { data: alloc_list_bytes(Self::layout(capacity)).cast(), len: 0, capacity }
    }

    fn reserve_exact(&mut self, capacity: usize) {
        if capacity <= self.capacity {
            return;
        }
        let data = alloc_list_bytes(Self::layout(capacity)).cast::<T>();
        // SAFETY: disjoint owned buffers; only the initialized prefix moves.
        unsafe {
            ptr::copy_nonoverlapping(self.data.as_ptr(), data.as_ptr(), self.len);
            dealloc_list_bytes(self.data.cast(), Self::layout(self.capacity));
        }
        self.data = data;
        self.capacity = capacity;
    }
    fn extend(&mut self, values: impl Iterator<Item = T>) {
        for value in values {
            if self.len == self.capacity {
                let needed = self.len.checked_add(1).expect("AST list too large");
                self.reserve_exact(needed.max(self.len.saturating_mul(2)));
            }
            // SAFETY: capacity is checked for every element, independently of size hints.
            unsafe { self.data.as_ptr().add(self.len).write(value) };
            self.len += 1;
        }
    }
}

impl<T> Drop for Spill<T> {
    fn drop(&mut self) {
        // Release the buffer even if an element's destructor unwinds.
        struct Storage(NonNull<u8>, Layout);
        impl Drop for Storage {
            fn drop(&mut self) {
                // SAFETY: this guard owns the original allocation.
                unsafe { dealloc_list_bytes(self.0, self.1) };
            }
        }
        let _storage = Storage(self.data.cast(), Self::layout(self.capacity));
        // SAFETY: the initialized prefix belongs to this buffer.
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.data.as_ptr(), self.len)) };
    }
}

// SAFETY: the buffer owns its elements, and AST chunk counts are atomic.
unsafe impl<T: Send> Send for Spill<T> {}
unsafe impl<T: Sync> Sync for Spill<T> {}

pub(crate) struct ArenaBuffer<T> {
    inline: SmallVec<[T; 4]>,
    spill: Option<Spill<T>>,
}

impl<T> ArenaBuffer<T> {
    pub(crate) fn new() -> Self {
        Self { inline: SmallVec::new(), spill: None }
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut values = Self::new();
        values.reserve_exact(capacity);
        values
    }

    pub(crate) fn spilled(&self) -> bool {
        self.spill.is_some()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        match &self.spill {
            // SAFETY: the buffer holds len initialized elements.
            Some(spill) => unsafe { slice::from_raw_parts(spill.data.as_ptr(), spill.len) },
            None => self.inline.as_slice(),
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        match &mut self.spill {
            // SAFETY: the buffer is uniquely borrowed and holds len initialized elements.
            Some(spill) => unsafe { slice::from_raw_parts_mut(spill.data.as_ptr(), spill.len) },
            None => self.inline.as_mut_slice(),
        }
    }

    fn capacity(&self) -> usize {
        self.spill.as_ref().map_or(self.inline.capacity(), |spill| spill.capacity)
    }

    fn reserve_exact(&mut self, capacity: usize) {
        if capacity <= self.capacity() {
            return;
        }
        match &mut self.spill {
            Some(spill) => spill.reserve_exact(capacity),
            None => {
                let mut spill: Spill<T> = Spill::new(capacity);
                for value in mem::take(&mut self.inline) {
                    // SAFETY: capacity exceeds the inline length; each slot is written once.
                    unsafe { spill.data.as_ptr().add(spill.len).write(value) };
                    spill.len += 1;
                }
                self.spill = Some(spill);
            }
        }
    }

    fn reserve_one(&mut self) {
        let len = self.as_slice().len();
        if len == self.capacity() {
            let needed = len.checked_add(1).expect("AST list too large");
            self.reserve_exact(needed.max(len.saturating_mul(2)));
        }
    }

    pub(crate) fn push(&mut self, value: T) {
        self.reserve_one();
        match &mut self.spill {
            Some(spill) => {
                // SAFETY: reserve_one left an uninitialized slot after the prefix.
                unsafe { spill.data.as_ptr().add(spill.len).write(value) };
                spill.len += 1;
            }
            None => self.inline.push(value),
        }
    }

    pub(crate) fn insert(&mut self, index: usize, value: T) {
        assert!(index <= self.as_slice().len());
        self.reserve_one();
        match &mut self.spill {
            Some(spill) => {
                // SAFETY: the shifted range stays within capacity; copy supports overlap.
                unsafe {
                    let at = spill.data.as_ptr().add(index);
                    ptr::copy(at, at.add(1), spill.len - index);
                    at.write(value);
                }
                spill.len += 1;
            }
            None => self.inline.insert(index, value),
        }
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        match &mut self.spill {
            Some(spill) if spill.len != 0 => {
                spill.len -= 1;
                // SAFETY: the last initialized slot moves out and is excluded from len.
                Some(unsafe { spill.data.as_ptr().add(spill.len).read() })
            }
            Some(_) => None,
            None => self.inline.pop(),
        }
    }

    pub(crate) fn remove(&mut self, index: usize) -> T {
        assert!(index < self.as_slice().len());
        match &mut self.spill {
            Some(spill) => {
                // SAFETY: index is initialized; move it out and shift the tail over it.
                unsafe {
                    let at = spill.data.as_ptr().add(index);
                    let value = at.read();
                    ptr::copy(at.add(1), at, spill.len - index - 1);
                    spill.len -= 1;
                    value
                }
            }
            None => self.inline.remove(index),
        }
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        match &mut self.spill {
            Some(spill) if len < spill.len => {
                let old_len = mem::replace(&mut spill.len, len);
                // SAFETY: exclude the owned tail before dropping it, including on unwind.
                unsafe {
                    ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                        spill.data.as_ptr().add(len),
                        old_len - len,
                    ));
                }
            }
            Some(_) => {}
            None => self.inline.truncate(len),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.truncate(0);
    }

    pub(crate) fn extend<I: IntoIterator<Item = T>>(&mut self, values: I) {
        let values = values.into_iter();
        let (lower, upper) = values.size_hint();
        if upper == Some(lower) {
            let needed = self.as_slice().len().checked_add(lower).expect("AST list too large");
            let capacity = self.capacity();
            if needed > capacity {
                self.reserve_exact(needed.max(capacity.saturating_mul(2)));
            }
        }
        self.extend_reserved(values);
    }

    fn extend_reserved(&mut self, mut values: impl Iterator<Item = T>) {
        if let Some(spill) = &mut self.spill {
            spill.extend(values);
            return;
        }
        let available = self.inline.capacity() - self.inline.len();
        self.inline.extend(values.by_ref().take(available));
        if let Some(value) = values.next() {
            // The inline buffer is full. This push creates the chunk-backed spill.
            self.push(value);
            self.spill.as_mut().expect("AST list exceeds inline capacity").extend(values);
        }
    }
}

impl<T: Clone> Clone for ArenaBuffer<T> {
    fn clone(&self) -> Self {
        self.as_slice().iter().cloned().collect()
    }
}

impl<T: fmt::Debug> fmt::Debug for ArenaBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_slice().fmt(f)
    }
}

impl<T> FromIterator<T> for ArenaBuffer<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        let values = values.into_iter().fuse();
        let mut buffer = Self::new();
        let (lower, upper) = values.size_hint();
        if upper == Some(lower) {
            buffer.reserve_exact(lower);
        }
        buffer.extend_reserved(values);
        buffer
    }
}

// SAFETY: these operations expose precisely the initialized prefix and owned capacity.
unsafe impl<T> FlatMapInPlaceVec for ArenaBuffer<T> {
    type Elem = T;
    fn len(&self) -> usize {
        self.spill.as_ref().map_or(self.inline.len(), |spill| spill.len)
    }
    unsafe fn set_len(&mut self, len: usize) {
        match &mut self.spill {
            Some(spill) => {
                debug_assert!(len <= spill.capacity);
                spill.len = len;
            }
            // SAFETY: the caller guarantees initialization and capacity.
            None => unsafe { self.inline.set_len(len) },
        }
    }
    fn as_ptr(&self) -> *const T {
        // Flat mapping and owning iteration can have moved elements beyond the live prefix.
        // Use the storage pointer directly without creating references to those elements.
        match &self.spill {
            Some(spill) => spill.data.as_ptr(),
            None => self.inline.as_ptr(),
        }
    }
    fn as_mut_ptr(&mut self) -> *mut T {
        match &mut self.spill {
            Some(spill) => spill.data.as_ptr(),
            None => self.inline.as_mut_ptr(),
        }
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
        let back = self.as_slice().len();
        // SAFETY: the iterator takes ownership of every initialized element.
        unsafe { self.set_len(0) };
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
        // SAFETY: this initialized slot moves out once.
        Some(unsafe { self.values.as_ptr().add(index).read() })
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
        // SAFETY: this initialized slot moves out once.
        Some(unsafe { self.values.as_ptr().add(self.back).read() })
    }
}

impl<T> ExactSizeIterator for IntoIter<T> {}
impl<T> core::iter::FusedIterator for IntoIter<T> {}

impl<T> Drop for IntoIter<T> {
    fn drop(&mut self) {
        // SAFETY: the remaining range is owned by the iterator; values has length zero.
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                self.values.as_mut_ptr().add(self.front),
                self.back - self.front,
            ));
        }
    }
}
