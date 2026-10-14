//! One-word AST lists with inline elements and spill buffers in the AST chunks.

use alloc::vec::Vec;
use core::fmt;
use core::ops::{Deref, DerefMut};

use crate::rustc_ast::arena_buffer::ArenaBuffer;
pub use crate::rustc_ast::arena_buffer::IntoIter;
use thin_vec::ThinVec;

use crate::rustc_ast::ptr::P;
use crate::rustc_data_structures::flat_map_in_place::FlatMapInPlace;
use crate::rustc_serialize::{Decodable, Decoder, Encodable, Encoder};

/// Empty lists have no storage. Nonempty lists own an arena-allocated inline buffer.
/// Spill buffers share AST chunks; only buffers too large for a chunk allocate globally.
#[derive(Clone)]
pub struct ArenaVec<T>(Option<P<ArenaBuffer<T>>>);

impl<T> ArenaVec<T> {
    pub fn new() -> Self {
        Self(None)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            Self::new()
        } else {
            Self(Some(P::new(ArenaBuffer::with_capacity(capacity))))
        }
    }

    pub fn as_slice(&self) -> &[T] {
        match &self.0 {
            Some(values) => values.as_slice(),
            None => &[],
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        match &mut self.0 {
            Some(values) => values.as_mut_slice(),
            None => &mut [],
        }
    }

    pub fn push(&mut self, value: T) {
        self.0.get_or_insert_with(|| P::new(ArenaBuffer::new())).push(value);
    }

    pub fn insert(&mut self, index: usize, value: T) {
        assert!(index <= self.len());
        self.0.get_or_insert_with(|| P::new(ArenaBuffer::new())).insert(index, value);
    }

    pub fn pop(&mut self) -> Option<T> {
        self.0.as_mut().and_then(|values| values.pop())
    }

    pub fn remove(&mut self, index: usize) -> T {
        assert!(index < self.len());
        self.0.as_mut().unwrap().remove(index)
    }

    pub fn truncate(&mut self, len: usize) {
        if let Some(values) = &mut self.0 {
            values.truncate(len);
        }
    }

    pub fn clear(&mut self) {
        if let Some(values) = &mut self.0 {
            values.clear();
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.flat_map_in_place(|value| if keep(&value) { Some(value) } else { None });
    }

    pub fn flat_map_in_place<F, I>(&mut self, f: F)
    where
        F: FnMut(T) -> I,
        I: IntoIterator<Item = T>,
    {
        if let Some(values) = &mut self.0 {
            (**values).flat_map_in_place(f);
        }
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
        let mut values = values.into_iter().fuse();
        if let Some(first) = values.next() {
            self.0
                .get_or_insert_with(|| P::new(ArenaBuffer::new()))
                .extend(core::iter::once(first).chain(values));
        }
    }
}

impl<T> FromIterator<T> for ArenaVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        let values: ArenaBuffer<T> = values.into_iter().collect();
        if values.is_empty() { Self::new() } else { Self(Some(P::new(values))) }
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

impl<T> IntoIterator for ArenaVec<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.map_or_else(ArenaBuffer::new, P::into_inner).into_iter()
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

const _: () = assert!(size_of::<ArenaVec<u32>>() == size_of::<usize>());
