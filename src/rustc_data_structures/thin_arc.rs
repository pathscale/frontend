//! `ThinArcSlice<T>`: a reference-counted, immutable slice behind one thin pointer.
//!
//! One allocation holds the count, the length and the elements, so building one from an
//! exact-length iterator costs one allocation, where `Arc<Vec<T>>` costs two and `Arc<[T]>`
//! doubles the handle to 16 bytes. Small slices share the AST's 64 KiB chunks;
//! larger or over-aligned slices use the global allocator. Every empty slice shares one
//! static header and never allocates or touches a count.

use crate::rustc_ast::ptr::{alloc_list_bytes, dealloc_list_bytes};
use alloc::vec::Vec;
use core::alloc::Layout;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering, fence};
use core::{fmt, mem, ops, slice};

use crate::rustc_serialize::{Decodable, Decoder, Encodable, Encoder};

#[repr(C)]
struct Header {
    count: AtomicUsize,
    len: usize,
}

struct Storage {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Drop for Storage {
    fn drop(&mut self) {
        // SAFETY: this guard owns storage after its initialized elements are gone.
        unsafe { dealloc_list_bytes(self.ptr, self.layout) };
    }
}

struct Initialization<T> {
    _storage: Storage,
    data: *mut T,
    initialized: usize,
}

impl<T> Drop for Initialization<T> {
    fn drop(&mut self) {
        // SAFETY: only this prefix was initialized, and the guard owns every element.
        // The storage field is dropped after the elements, including during unwind.
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.data, self.initialized));
        }
    }
}

/// The shared header of every empty slice. Its count is never read or written.
static EMPTY: Header = Header { count: AtomicUsize::new(1), len: 0 };

pub struct ThinArcSlice<T> {
    ptr: NonNull<Header>,
    _owns: PhantomData<T>,
}

// SAFETY: the same bounds as `Arc<[T]>`: the elements are shared across threads and the
// count is atomic.
unsafe impl<T: Send + Sync> Send for ThinArcSlice<T> {}
unsafe impl<T: Send + Sync> Sync for ThinArcSlice<T> {}

impl<T> ThinArcSlice<T> {
    #[inline]
    fn layout(len: usize) -> (Layout, usize) {
        let (layout, offset) = Layout::new::<Header>()
            .extend(Layout::array::<T>(len).expect("ThinArcSlice too large"))
            .expect("ThinArcSlice too large");
        (layout.pad_to_align(), offset)
    }

    #[inline]
    pub fn empty() -> Self {
        ThinArcSlice { ptr: NonNull::from(&EMPTY), _owns: PhantomData }
    }

    #[inline]
    fn header(&self) -> &Header {
        // SAFETY: `ptr` is the static header or a live allocation this handle counts.
        unsafe { self.ptr.as_ref() }
    }

    #[inline]
    fn data(&self) -> *mut T {
        if self.header().len == 0 {
            return NonNull::dangling().as_ptr();
        }
        let (_, offset) = Self::layout(self.header().len);
        // SAFETY: a non-empty header heads an allocation laid out by `layout`.
        unsafe { self.ptr.as_ptr().cast::<u8>().add(offset).cast::<T>() }
    }

    /// Builds a slice in one allocation from an iterator that reports its exact length.
    pub fn from_exact<I: ExactSizeIterator<Item = T>>(mut iter: I) -> Self {
        let len = iter.len();
        if len == 0 {
            return Self::empty();
        }
        let (layout, offset) = Self::layout(len);
        let storage = alloc_list_bytes(layout);
        let raw = storage.as_ptr();
        let ptr = storage.cast::<Header>();
        // SAFETY: `ptr` is fresh and aligned for `Header`; elements go at `offset`.
        let data = unsafe { raw.add(offset).cast::<T>() };
        let mut initialization = Initialization {
            _storage: Storage { ptr: storage, layout },
            data,
            initialized: 0,
        };
        unsafe { ptr.as_ptr().write(Header { count: AtomicUsize::new(1), len }) };
        for i in 0..len {
            let item = iter.next().expect("ExactSizeIterator yielded fewer items than its len");
            // SAFETY: each slot is in the allocation and written exactly once.
            unsafe { data.add(i).write(item) };
            initialization.initialized += 1;
        }
        // The completed handle takes ownership of the initialized elements and storage.
        mem::forget(initialization);
        ThinArcSlice { ptr, _owns: PhantomData }
    }

    /// The slice, mutable, when this handle is the only one. `None` when shared or empty.
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut [T]> {
        let len = self.header().len;
        if len == 0 || self.header().count.load(Ordering::Acquire) != 1 {
            return None;
        }
        // SAFETY: the count is one, so no other handle can read these elements.
        Some(unsafe { slice::from_raw_parts_mut(self.data(), len) })
    }

    /// The slice, mutable, copying it first when it is shared.
    pub fn make_mut(&mut self) -> &mut [T]
    where
        T: Clone,
    {
        if self.get_mut().is_none() && !self.is_empty() {
            *self = Self::from_exact(self.iter().cloned());
        }
        let len = self.header().len;
        // SAFETY: empty, or unique after the copy above.
        unsafe { slice::from_raw_parts_mut(self.data(), len) }
    }
}

impl<T> ops::Deref for ThinArcSlice<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: `len` initialized elements live at `data` (or none, at a dangling pointer).
        unsafe { slice::from_raw_parts(self.data(), self.header().len) }
    }
}

impl<T> Clone for ThinArcSlice<T> {
    #[inline]
    fn clone(&self) -> Self {
        if self.header().len != 0 {
            self.header()
                .count
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < isize::MAX as usize).then(|| count + 1)
                })
                .expect("ThinArcSlice count overflow");
        }
        ThinArcSlice { ptr: self.ptr, _owns: PhantomData }
    }
}

impl<T> Drop for ThinArcSlice<T> {
    #[inline]
    fn drop(&mut self) {
        let len = self.header().len;
        if len == 0 || self.header().count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        let (layout, _) = Self::layout(len);
        let _storage = Storage { ptr: self.ptr.cast(), layout };
        // SAFETY: the last handle; the elements are initialized and dropped once.
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.data(), len));
        }
    }
}

impl<T> Default for ThinArcSlice<T> {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl<T> From<Vec<T>> for ThinArcSlice<T> {
    #[inline]
    fn from(vec: Vec<T>) -> Self {
        Self::from_exact(vec.into_iter())
    }
}

impl<T> FromIterator<T> for ThinArcSlice<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<T>>())
    }
}

impl<T: fmt::Debug> fmt::Debug for ThinArcSlice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: PartialEq> PartialEq for ThinArcSlice<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr || **self == **other
    }
}

impl<T: Eq> Eq for ThinArcSlice<T> {}

impl<T: Hash> Hash for ThinArcSlice<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl<E: Encoder, T: Encodable<E>> Encodable<E> for ThinArcSlice<T> {
    fn encode(&self, s: &mut E) {
        (**self).encode(s);
    }
}

impl<D: Decoder, T: Decodable<D>> Decodable<D> for ThinArcSlice<T> {
    fn decode(d: &mut D) -> Self {
        let vec: Vec<T> = Decodable::decode(d);
        vec.into()
    }
}

const _: () = assert!(mem::size_of::<ThinArcSlice<u64>>() == mem::size_of::<usize>());

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    #[test]
    fn build_clone_drop() {
        let a: ThinArcSlice<Arc<u32>> = ThinArcSlice::from_exact((0..5).map(Arc::new));
        let b = a.clone();
        assert_eq!(a.len(), 5);
        assert_eq!(*b[4], 4);
        let probe = a[0].clone();
        drop(a);
        assert_eq!(Arc::strong_count(&probe), 2);
        drop(b);
        assert_eq!(Arc::strong_count(&probe), 1);
    }

    #[test]
    fn empty_shares_static() {
        let e: ThinArcSlice<u8> = ThinArcSlice::from_exact(core::iter::empty());
        let f = e.clone();
        assert!(e.is_empty() && f.is_empty());
        assert_eq!(e, ThinArcSlice::default());
    }

    #[test]
    fn small_slice_survives_chunk_turnover() {
        let a = ThinArcSlice::from_exact((0..64).map(Arc::new));
        let b = a.clone();
        let probe = a[0].clone();
        for _ in 0..1024 {
            let transient = ThinArcSlice::from_exact(0..64u32);
            assert_eq!(transient[63], 63);
        }
        drop(a);
        assert_eq!(*b[63], 63);
        assert_eq!(Arc::strong_count(&probe), 2);
        drop(b);
        assert_eq!(Arc::strong_count(&probe), 1);
    }

    #[test]
    fn large_and_overaligned_slices() {
        #[repr(align(128))]
        struct Aligned(u8);

        let large = ThinArcSlice::from_exact(0..4096u32);
        let shared = large.clone();
        drop(large);
        assert_eq!(shared[4095], 4095);

        let mut aligned = ThinArcSlice::from_exact(core::iter::once(Aligned(7)));
        assert_eq!(aligned.as_ptr() as usize % 128, 0);
        aligned.get_mut().unwrap()[0].0 = 9;
        assert_eq!(aligned[0].0, 9);
    }

    #[test]
    fn make_mut_copies_when_shared() {
        let mut a: ThinArcSlice<u32> = vec![1, 2, 3].into();
        let b = a.clone();
        a.make_mut()[0] = 9;
        assert_eq!(&*a, &[9, 2, 3]);
        assert_eq!(&*b, &[1, 2, 3]);
        let mut c = a.clone();
        drop(a);
        assert!(c.get_mut().is_some());
    }
}
