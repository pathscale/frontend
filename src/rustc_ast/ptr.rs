//! `P<T>`, the owning pointer for boxed AST nodes.
//!
//! A one-word owning handle with the parts of `Box` the AST uses: `Deref`, `DerefMut`, a deep
//! `Clone`, and `into_inner` in place of moving out with `*p`.
//!
//! Nodes are bump-allocated into 64 KiB chunks, one current chunk per thread, instead of one
//! global allocation each. A chunk is aligned to its size, so a node finds its chunk by masking
//! its address. The chunk header counts the nodes alive in it, plus one while it is a thread's
//! current chunk; dropping a node runs its destructor and decrements the count, and the chunk
//! goes back to the global allocator when the count reaches zero. Nothing ties a node to a
//! session or a thread: a node can be dropped anywhere, and memory is held only while some node
//! of the chunk is alive.

use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use core::fmt;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering, fence};

use crate::rustc_data_structures::stable_hash::{StableHash, StableHashCtxt, StableHasher};
use crate::rustc_serialize::{Decodable, Decoder, Encodable, Encoder};

/// Bytes in a chunk, and its alignment.
const CHUNK_BYTES: usize = 64 * 1024;
/// Where nodes start in a chunk; also the largest node alignment supported.
const HEADER_BYTES: usize = 64;
/// The largest node, so a chunk always fits many.
const MAX_NODE_BYTES: usize = 1024;

#[repr(C, align(64))]
struct ChunkHeader {
    /// Nodes alive in the chunk, plus one while it is a thread's current chunk.
    live: AtomicUsize,
}

fn chunk_layout() -> Layout {
    // SAFETY: a power-of-two alignment, and the size is a multiple of it.
    unsafe { Layout::from_size_align_unchecked(CHUNK_BYTES, CHUNK_BYTES) }
}

/// A fresh chunk with a count of one.
#[cold]
fn new_chunk() -> NonNull<ChunkHeader> {
    // SAFETY: the layout has a nonzero size.
    let raw = unsafe { alloc(chunk_layout()) }.cast::<ChunkHeader>();
    let Some(chunk) = NonNull::new(raw) else { handle_alloc_error(chunk_layout()) };
    // SAFETY: freshly allocated, aligned for the header.
    unsafe { chunk.as_ptr().write(ChunkHeader { live: AtomicUsize::new(1) }) };
    chunk
}

/// Drops one count of `chunk`, freeing it on the last.
///
/// # Safety
///
/// The caller owns one count of a live chunk, and gives it up.
#[inline]
unsafe fn release(chunk: NonNull<ChunkHeader>) {
    // The same discipline as `Arc`: every count holder's writes happen before the free.
    if unsafe { chunk.as_ref() }.live.fetch_sub(1, Ordering::Release) == 1 {
        fence(Ordering::Acquire);
        // SAFETY: the last count is gone, so nothing points into the chunk.
        unsafe { dealloc(chunk.as_ptr().cast(), chunk_layout()) };
    }
}

/// The chunk that holds the node at `node`.
#[inline]
fn chunk_of<T>(node: NonNull<T>) -> NonNull<ChunkHeader> {
    let addr = node.as_ptr() as usize & !(CHUNK_BYTES - 1);
    // SAFETY: a node is never in a chunk's first bytes, so the masked address is the nonzero
    // chunk start.
    unsafe { NonNull::new_unchecked(addr as *mut ChunkHeader) }
}

/// A thread's current chunk and the offset of its first free byte.
struct Current {
    chunk: Option<NonNull<ChunkHeader>>,
    next: usize,
}

impl Drop for Current {
    fn drop(&mut self) {
        if let Some(chunk) = self.chunk {
            // SAFETY: the current-chunk count is this value's.
            unsafe { release(chunk) }
        }
    }
}

static CURRENT: eko::thread::ThreadLocal<Current> = eko::thread::ThreadLocal::new();

/// Room for one node of `layout`, with a count taken on its chunk for the node.
#[inline]
fn alloc_node(layout: Layout) -> NonNull<u8> {
    let slot = CURRENT.with(
        || Current { chunk: None, next: CHUNK_BYTES },
        |cur| {
            let align = layout.align();
            let mut offset = (cur.next + align - 1) & !(align - 1);
            let chunk = match cur.chunk {
                Some(chunk) if offset + layout.size() < CHUNK_BYTES => chunk,
                old => {
                    if let Some(old) = old {
                        // SAFETY: the current-chunk count, given up as the chunk is retired.
                        unsafe { release(old) }
                    }
                    let chunk = new_chunk();
                    cur.chunk = Some(chunk);
                    offset = HEADER_BYTES;
                    chunk
                }
            };
            cur.next = offset + layout.size();
            // SAFETY: the current chunk is alive while it holds the current-chunk count.
            unsafe { chunk.as_ref() }.live.fetch_add(1, Ordering::Relaxed);
            // SAFETY: `offset + size` is within the chunk, and strictly below its end, so even a
            // zero-sized node masks back to this chunk.
            unsafe { NonNull::new_unchecked(chunk.as_ptr().cast::<u8>().add(offset)) }
        },
    );
    // Out of thread-local keys: a chunk of its own, whose one count is the node's.
    slot.unwrap_or_else(|| {
        let chunk = new_chunk();
        // SAFETY: the header is `HEADER_BYTES`, inside the chunk.
        unsafe { NonNull::new_unchecked(chunk.as_ptr().cast::<u8>().add(HEADER_BYTES)) }
    })
}

pub struct P<T> {
    ptr: NonNull<T>,
    _owns: PhantomData<T>,
}

// SAFETY: `P` owns its node as `Box` does; the chunk count is atomic.
unsafe impl<T: Send> Send for P<T> {}
// SAFETY: as `Box`.
unsafe impl<T: Sync> Sync for P<T> {}

impl<T> P<T> {
    #[inline]
    pub fn new(value: T) -> P<T> {
        const {
            assert!(size_of::<T>() <= MAX_NODE_BYTES && align_of::<T>() <= HEADER_BYTES);
        }
        let slot = alloc_node(Layout::new::<T>()).cast::<T>();
        // SAFETY: a fresh slot sized and aligned for `T`.
        unsafe { slot.as_ptr().write(value) };
        P { ptr: slot, _owns: PhantomData }
    }

    /// Moves the node out, which `*p` does for a `Box`.
    #[inline]
    pub fn into_inner(self) -> T {
        let this = core::mem::ManuallyDrop::new(self);
        // SAFETY: the node is initialized and owned; `this` is not dropped, so it is read once.
        let value = unsafe { this.ptr.as_ptr().read() };
        // SAFETY: the node's count.
        unsafe { release(chunk_of(this.ptr)) };
        value
    }

    #[inline]
    pub fn map(self, f: impl FnOnce(T) -> T) -> P<T> {
        P::new(f(self.into_inner()))
    }
}

impl<T> Deref for P<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: an owned, initialized node.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for P<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: an owned, initialized node, borrowed uniquely through `self`.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for P<T> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: the node is owned and initialized, dropped once; then its count goes.
        unsafe {
            ptr::drop_in_place(self.ptr.as_ptr());
            release(chunk_of(self.ptr));
        }
    }
}

impl<T: Clone> Clone for P<T> {
    #[inline]
    fn clone(&self) -> P<T> {
        P::new((**self).clone())
    }
}

impl<T: fmt::Debug> fmt::Debug for P<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for P<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> From<T> for P<T> {
    #[inline]
    fn from(value: T) -> P<T> {
        P::new(value)
    }
}

impl<T> AsRef<T> for P<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> AsMut<T> for P<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: PartialEq> PartialEq for P<T> {
    fn eq(&self, other: &P<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for P<T> {}

impl<T: core::hash::Hash> core::hash::Hash for P<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl<S: Encoder, T: Encodable<S>> Encodable<S> for P<T> {
    fn encode(&self, s: &mut S) {
        (**self).encode(s)
    }
}

impl<D: Decoder, T: Decodable<D>> Decodable<D> for P<T> {
    fn decode(d: &mut D) -> P<T> {
        P::new(Decodable::decode(d))
    }
}

impl<T: StableHash> StableHash for P<T> {
    #[inline]
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        (**self).stable_hash(hcx, hasher);
    }
}
