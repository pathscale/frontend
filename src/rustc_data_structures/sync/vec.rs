// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::rustc_index::Idx;

/// Number of chunks the storage is allowed to grow into. Chunk `k` holds
/// `FIRST_CHUNK << k` elements, so `NUM_CHUNKS` chunks hold
/// `FIRST_CHUNK * (2^NUM_CHUNKS - 1)` elements in total - far past anything a
/// compilation session can index, while costing one pointer per chunk in the struct.
const NUM_CHUNKS: usize = 32;

/// Elements in chunk 0. Every later chunk doubles.
const FIRST_CHUNK: usize = 8;

/// Split a flat index into `(chunk, offset within chunk)`.
///
/// Chunk `k` starts at flat index `FIRST_CHUNK * (2^k - 1)`, so
/// `index / FIRST_CHUNK + 1` lands in `[2^k, 2^(k+1))` exactly when `index` is in chunk
/// `k`, and the chunk number is that value's floor-log2.
#[inline]
fn chunk_of(index: usize) -> (usize, usize) {
    let scaled = index / FIRST_CHUNK + 1;
    let chunk = (usize::BITS - 1 - scaled.leading_zeros()) as usize;
    let prior = FIRST_CHUNK * ((1usize << chunk) - 1);
    (chunk, index - prior)
}

#[inline]
fn chunk_len(chunk: usize) -> usize {
    FIRST_CHUNK << chunk
}

fn alloc_chunk<T>(len: usize) -> *mut MaybeUninit<T> {
    let mut v: Vec<MaybeUninit<T>> = Vec::with_capacity(len);
    v.resize_with(len, MaybeUninit::uninit);
    Box::into_raw(v.into_boxed_slice()).cast::<MaybeUninit<T>>()
}

/// An append-only vector of `Copy` values whose read path takes no lock.
///
/// This replaces `elsa::sync::LockFreeFrozenVec`, which was the last reason this crate
/// pulled in `elsa` - a `std` crate. The contract is the one `elsa` documented and the one
/// `AppendOnlyIndexVec` below relies on: an element, once pushed, is never removed, never
/// overwritten and never moved, so an index handed out by `push` stays valid and stays
/// meaningful for the life of the collection. Storage is a fixed array of chunk pointers
/// rather than one growable buffer precisely so that growing cannot move what is already
/// there.
///
/// Writers serialise on a `parking_lot::Mutex`; readers touch it not at all.
pub struct LockFreeAppendOnlyVec<T: Copy> {
    /// `chunks[k]` is null until chunk `k` is first needed. Chunks are filled in order, so
    /// a null entry implies every later entry is null too.
    chunks: [AtomicPtr<MaybeUninit<T>>; NUM_CHUNKS],
    /// Number of slots that have been fully written. Published with `Release` by the
    /// writer, read with `Acquire`, and that pairing is what makes the read path sound.
    len: AtomicUsize,
    /// Held across a `push`. Readers never take it.
    writers: parking_lot::Mutex<()>,
}

// SAFETY: the only way to observe a `T` through a shared reference is `get`, which copies
// the value out; no reference into the storage ever escapes, so there is no aliasing to
// reason about. Moving a `T` between threads therefore happens on `push`/`get`, which is
// exactly what `T: Send` licenses. The chunk pointers and the length are atomics, and the
// non-atomic element writes are ordered against readers by the `Release`/`Acquire` pair on
// `len` (see `push`). `T: Sync` is deliberately not required, because no shared reference
// to a `T` is ever created.
unsafe impl<T: Copy + Send> Send for LockFreeAppendOnlyVec<T> {}
unsafe impl<T: Copy + Send> Sync for LockFreeAppendOnlyVec<T> {}

impl<T: Copy> Default for LockFreeAppendOnlyVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> LockFreeAppendOnlyVec<T> {
    #[allow(clippy::declare_interior_mutable_const)]
    const NULL: AtomicPtr<MaybeUninit<T>> = AtomicPtr::new(ptr::null_mut());

    pub const fn new() -> Self {
        Self {
            chunks: [Self::NULL; NUM_CHUNKS],
            len: AtomicUsize::new(0),
            writers: parking_lot::Mutex::new(()),
        }
    }

    /// Append `val` and return the index it was written at.
    pub fn push(&self, val: T) -> usize {
        let _writer = self.writers.lock();

        // Only a writer stores to `len`, and we hold the writer lock, so this cannot be
        // stale: no other thread can have advanced it.
        let len = self.len.load(Ordering::Relaxed);
        let (chunk, offset) = chunk_of(len);
        assert!(chunk < NUM_CHUNKS, "LockFreeAppendOnlyVec grew past {NUM_CHUNKS} chunks");

        let mut ptr = self.chunks[chunk].load(Ordering::Relaxed);
        if ptr.is_null() {
            ptr = alloc_chunk::<T>(chunk_len(chunk));
            self.chunks[chunk].store(ptr, Ordering::Relaxed);
        }

        // SAFETY: `offset < chunk_len(chunk)` by construction of `chunk_of`, and `ptr`
        // addresses a live allocation of that many `MaybeUninit<T>`. Slot `offset` is
        // untouched: `len` names the first unwritten slot, writers are serialised by
        // `self.writers`, and no slot is ever written twice.
        unsafe {
            ptr.add(offset).write(MaybeUninit::new(val));
        }

        // The store that publishes everything above. A reader that observes `len > i` has
        // an Acquire load that synchronises-with this Release store, so the chunk pointer
        // and the element write for slot `i` are visible to it.
        self.len.store(len + 1, Ordering::Release);
        len
    }

    /// Elements published so far. Grows only; a reader may see a length a concurrent push
    /// has just passed, never one it has not reached.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<T> {
        // The length only grows, so reading it once and independently of the element is
        // sound: the worst case is a stale value and a spurious `None` for an element some
        // other thread has just pushed.
        if index >= self.len.load(Ordering::Acquire) {
            return None;
        }
        let (chunk, offset) = chunk_of(index);
        let ptr = self.chunks[chunk].load(Ordering::Relaxed);

        // SAFETY: `index < len` was established with an Acquire load, which
        // synchronises-with the Release store in the `push` that wrote slot `index`.
        // Everything that `push` did before that store is therefore visible here: the chunk
        // pointer is non-null and the slot is initialised. Nothing ever writes the slot
        // again, so no concurrent write races this read. `T: Copy` means the value is read
        // out by copy and the storage keeps its own.
        Some(unsafe { (*ptr.add(offset)).assume_init() })
    }
}

impl<T: Copy> Drop for LockFreeAppendOnlyVec<T> {
    fn drop(&mut self) {
        for chunk in 0..NUM_CHUNKS {
            let ptr = *self.chunks[chunk].get_mut();
            if ptr.is_null() {
                // Chunks are allocated in order, so the first null ends the live ones.
                break;
            }
            // SAFETY: `ptr` came from `Box::into_raw` on a boxed slice of exactly
            // `chunk_len(chunk)` `MaybeUninit<T>`, and this is the only place it is
            // reclaimed - `&mut self` means no reader or writer can be running. `T: Copy`
            // has no drop glue and `MaybeUninit<T>` has none either, so this is a free and
            // nothing is dropped that was never initialised.
            unsafe {
                drop(Box::from_raw(ptr::slice_from_raw_parts_mut(ptr, chunk_len(chunk))));
            }
        }
    }
}

#[derive(Default)]
pub struct AppendOnlyIndexVec<I: Idx, T: Copy> {
    vec: LockFreeAppendOnlyVec<T>,
    _marker: PhantomData<fn(&I)>,
}

impl<I: Idx, T: Copy> AppendOnlyIndexVec<I, T> {
    pub fn new() -> Self {
        Self { vec: LockFreeAppendOnlyVec::new(), _marker: PhantomData }
    }

    pub fn push(&self, val: T) -> I {
        let i = self.vec.push(val);
        I::new(i)
    }

    pub fn get(&self, i: I) -> Option<T> {
        let i = i.index();
        self.vec.get(i)
    }

    /// Elements published so far; see `LockFreeAppendOnlyVec::len`. Every index below it
    /// reads back `Some` from `get` on this thread.
    pub fn len(&self) -> usize {
        self.vec.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vec.is_empty()
    }
}

#[derive(Default)]
pub struct AppendOnlyVec<T: Copy> {
    vec: parking_lot::RwLock<Vec<T>>,
}

impl<T: Copy> AppendOnlyVec<T> {
    pub fn new() -> Self {
        Self { vec: Default::default() }
    }

    pub fn push(&self, val: T) -> usize {
        let mut v = self.vec.write();
        let n = v.len();
        v.push(val);
        n
    }

    pub fn get(&self, i: usize) -> Option<T> {
        self.vec.read().get(i).copied()
    }

    pub fn iter_enumerated(&self) -> impl Iterator<Item = (usize, T)> {
        (0..).map_while(|i| Some((i, self.get(i)?)))
    }

    pub fn iter(&self) -> impl Iterator<Item = T> {
        (0..).map_while(|i| self.get(i))
    }
}

impl<T: Copy + PartialEq> AppendOnlyVec<T> {
    pub fn contains(&self, val: T) -> bool {
        self.iter().any(|v| v == val)
    }
}

impl<A: Copy> FromIterator<A> for AppendOnlyVec<A> {
    fn from_iter<T: IntoIterator<Item = A>>(iter: T) -> Self {
        let this = Self::new();
        for val in iter {
            this.push(val);
        }
        this
    }
}
