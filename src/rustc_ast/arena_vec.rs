//! One-word AST lists with elements stored directly in the AST chunks.

use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::{Bound, Deref, DerefMut, RangeBounds};

use crate::rustc_ast::arena_buffer::ArenaBuffer;
pub use crate::rustc_ast::arena_buffer::IntoIter;
use thin_vec::ThinVec;

use crate::rustc_data_structures::flat_map_in_place::FlatMapInPlace;
use crate::rustc_data_structures::stable_hash::{StableHash, StableHashCtxt, StableHasher};
use crate::rustc_serialize::{Decodable, Decoder, Encodable, Encoder};

/// Empty lists have no storage. Each allocated list owns one contiguous buffer.
/// Buffers share AST chunks; only buffers too large for a chunk allocate globally.
#[repr(transparent)]
#[derive(Clone)]
pub struct ArenaVec<T>(ArenaBuffer<T>);

impl<T> ArenaVec<T> {
    pub fn new() -> Self {
        Self(ArenaBuffer::new())
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self(ArenaBuffer::with_capacity(capacity))
    }

    pub fn from_elem(element: T, count: usize) -> Self
    where
        T: Clone,
    {
        Self(ArenaBuffer::from_elem(element, count))
    }

    pub fn as_slice(&self) -> &[T] {
        self.0.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.0.as_mut_slice()
    }

    pub fn push(&mut self, value: T) {
        self.0.push(value);
    }

    pub fn insert(&mut self, index: usize, value: T) {
        self.0.insert(index, value);
    }

    pub fn pop(&mut self) -> Option<T> {
        self.0.pop()
    }

    pub fn remove(&mut self, index: usize) -> T {
        self.0.remove(index)
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Replace a range, returning ownership of its removed elements in order.
    pub fn splice<R, I>(&mut self, range: R, replace_with: I) -> alloc::vec::IntoIter<T>
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

        // Finish fallible iteration and allocations before moving existing elements.
        let replacement: Vec<T> = replace_with.into_iter().collect();
        let final_len = (len - (end - start))
            .checked_add(replacement.len())
            .expect("AST list too large");
        let mut tail = Vec::with_capacity(len - end);
        let mut removed = Vec::with_capacity(end - start);
        self.0.reserve_exact(final_len);

        // Pop into temporary owners so no slot is read or dropped twice, including ZSTs.
        for _ in end..len {
            tail.push(self.0.pop().expect("AST list tail element"));
        }
        for _ in start..end {
            removed.push(self.0.pop().expect("AST list removed element"));
        }
        self.0.extend(replacement);
        self.0.extend(tail.into_iter().rev());
        removed.reverse();
        removed.into_iter()
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.flat_map_in_place(|value| if keep(&value) { Some(value) } else { None });
    }

    pub fn flat_map_in_place<F, I>(&mut self, f: F)
    where
        F: FnMut(T) -> I,
        I: IntoIterator<Item = T>,
    {
        self.0.flat_map_in_place(f);
    }
}

impl<T> Default for ArenaVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for ArenaVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> DerefMut for ArenaVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T> Extend<T> for ArenaVec<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, values: I) {
        self.0.extend(values);
    }
}

impl<T> FromIterator<T> for ArenaVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        Self(values.into_iter().collect())
    }
}

impl<T> From<ThinVec<T>> for ArenaVec<T> {
    fn from(values: ThinVec<T>) -> Self {
        values.into_iter().collect()
    }
}

impl<T> From<Vec<T>> for ArenaVec<T> {
    fn from(values: Vec<T>) -> Self {
        values.into_iter().collect()
    }
}

impl<T, const N: usize> From<[T; N]> for ArenaVec<T> {
    fn from(values: [T; N]) -> Self {
        values.into_iter().collect()
    }
}

impl<T> IntoIterator for ArenaVec<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a ArenaVec<T> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut ArenaVec<T> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<T: fmt::Debug> fmt::Debug for ArenaVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_slice().fmt(f)
    }
}

impl<T: PartialEq> PartialEq for ArenaVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for ArenaVec<T> {}

impl<T: Hash> Hash for ArenaVec<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl<T: StableHash> StableHash for ArenaVec<T> {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.as_slice().stable_hash(hcx, hasher);
    }
}

impl<E: Encoder, T: Encodable<E>> Encodable<E> for ArenaVec<T> {
    fn encode(&self, encoder: &mut E) {
        self.as_slice().encode(encoder);
    }
}

impl<D: Decoder, T: Decodable<D>> Decodable<D> for ArenaVec<T> {
    fn decode(decoder: &mut D) -> Self {
        Vec::<T>::decode(decoder).into()
    }
}

const _: () = assert!(core::mem::size_of::<ArenaVec<u32>>() == core::mem::size_of::<usize>());
