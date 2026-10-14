//! `P<T>`, the owning pointer for boxed AST nodes.
//!
//! A one-word owning handle with the parts of `Box` the AST uses: `Deref`, `DerefMut`, a deep
//! `Clone`, and `into_inner` in place of moving out with `*p`.
//!
//! Nodes are bump-allocated into 64 KiB chunks, one current chunk per thread, instead of one
//! global allocation each. A chunk is aligned to its size, so a node finds its chunk by masking
//! its address. The chunk header counts issued allocations, reserved allocation counts, and
//! one pin while it is a thread's current chunk. The current chunk reserves counts in batches,
//! consuming them locally for new nodes. Dropping a node runs its destructor and decrements
//! the count; retiring a chunk releases its pin and unused reservations. The chunk goes back
//! to the global allocator when the count reaches zero. Nothing ties a node to a session or a
//! thread: a node can be dropped anywhere, and retired memory is held only while some allocation
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
/// Ownership counts acquired together by a chunk's sole allocating thread.
const COUNT_BATCH: usize = 256;

#[repr(C, align(64))]
struct ChunkHeader {
    /// Issued allocations, unused reservations, and the current-chunk pin.
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

/// Drops one count of the chunk, freeing it on the last.
///
/// # Safety
///
/// The caller owns one count of a live chunk, and gives it up.
#[inline]
unsafe fn release(chunk: NonNull<ChunkHeader>) {
    // SAFETY: the caller gives up its single count.
    unsafe { release_counts(chunk, 1) };
}

/// Gives up a pin and unused reservations, or one issued allocation count.
///
/// # Safety
///
/// The caller owns all counts of this live chunk; counts is nonzero.
#[inline]
unsafe fn release_counts(chunk: NonNull<ChunkHeader>, counts: usize) {
    // The same discipline as Arc: every count holder's writes happen before the free.
    if unsafe { chunk.as_ref() }.live.fetch_sub(counts, Ordering::Release) == counts {
        fence(Ordering::Acquire);
        // SAFETY: the last counts are gone, so nothing points into the chunk.
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

/// A thread's current chunk, next free byte, and unissued ownership counts.
struct Current {
    chunk: Option<NonNull<ChunkHeader>>,
    next: usize,
    reserved: usize,
}

impl Current {
    fn new() -> Self {
        Self { chunk: None, next: CHUNK_BYTES, reserved: 0 }
    }

    fn retire(&mut self) {
        if let Some(chunk) = self.chunk.take() {
            let counts = 1 + core::mem::take(&mut self.reserved);
            // SAFETY: this value owns the pin and all unissued reservation counts.
            unsafe { release_counts(chunk, counts) };
        }
    }
}

impl Drop for Current {
    fn drop(&mut self) {
        self.retire();
    }
}

static CURRENT: eko::thread::ThreadLocal<Current> = eko::thread::ThreadLocal::new();

/// An owned parsing job's allocation epoch. Returned nodes retain their own chunk counts.
#[cfg(feature = "parallel")]
pub(crate) struct ParseArena(Option<Current>);

#[cfg(feature = "parallel")]
impl ParseArena {
    pub(crate) fn enter() -> Self {
        Self(CURRENT.with(
            Current::new,
            |current| core::mem::replace(current, Current::new()),
        ))
    }
}

#[cfg(feature = "parallel")]
impl Drop for ParseArena {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            let retired = CURRENT.with(
                Current::new,
                |current| core::mem::replace(current, previous),
            );
            drop(retired);
        }
    }
}

/// Room for one node of layout, transferring one reserved count to it.
#[inline]
fn alloc_node(layout: Layout) -> NonNull<u8> {
    let slot = CURRENT.with(
        Current::new,
        |cur| {
            let align = layout.align();
            let mut offset = (cur.next + align - 1) & !(align - 1);
            let chunk = match cur.chunk {
                Some(chunk) if offset + layout.size() < CHUNK_BYTES => chunk,
                _ => {
                    cur.retire();
                    let chunk = new_chunk();
                    cur.chunk = Some(chunk);
                    offset = HEADER_BYTES;
                    chunk
                }
            };
            cur.next = offset + layout.size();
            if cur.reserved == 0 {
                // SAFETY: the current-chunk pin keeps the header alive. Only this
                // thread reserves counts; other threads may release issued counts.
                let count = unsafe { chunk.as_ref() }
                    .live
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                        let available = (isize::MAX as usize).saturating_sub(count);
                        (available != 0).then(|| count + available.min(COUNT_BATCH))
                    })
                    .expect("AST chunk count overflow");
                cur.reserved = ((isize::MAX as usize) - count).min(COUNT_BATCH);
            }
            // One reserved count becomes the returned allocation's owned count.
            cur.reserved -= 1;
            // SAFETY: offset + size is within the chunk, and strictly below its end, so even a
            // zero-sized node masks back to this chunk.
            unsafe { NonNull::new_unchecked(chunk.as_ptr().cast::<u8>().add(offset)) }
        },
    );
    // Out of thread-local keys: a chunk of its own, whose one count is the node's.
    slot.unwrap_or_else(|| {
        let chunk = new_chunk();
        // SAFETY: the header is HEADER_BYTES, inside the chunk.
        unsafe { NonNull::new_unchecked(chunk.as_ptr().cast::<u8>().add(HEADER_BYTES)) }
    })
}

/// Allocates nonempty storage, sharing AST chunks for small layouts.
#[inline]
pub(crate) fn alloc_bytes(layout: Layout) -> NonNull<u8> {
    assert!(layout.size() != 0);
    if layout.size() <= MAX_NODE_BYTES && layout.align() <= HEADER_BYTES {
        alloc_node(layout)
    } else {
        // SAFETY: the layout has a nonzero size.
        let raw = unsafe { alloc(layout) };
        NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout))
    }
}

/// Releases storage returned by `alloc_bytes`.
///
/// # Safety
///
/// `ptr` must be a live allocation from `alloc_bytes(layout)`, with all values
/// in it already dropped. The caller gives up ownership of that allocation.
#[inline]
pub(crate) unsafe fn dealloc_bytes(ptr: NonNull<u8>, layout: Layout) {
    if layout.size() <= MAX_NODE_BYTES && layout.align() <= HEADER_BYTES {
        // SAFETY: the allocation owns one count of its chunk.
        unsafe { release(chunk_of(ptr)) };
    } else {
        // SAFETY: the pointer and layout describe the original global allocation.
        unsafe { dealloc(ptr.as_ptr(), layout) };
    }
}

/// List buffers use the same chunks up to the largest layout that fits after a header.
#[inline]
pub(crate) fn alloc_list_bytes(layout: Layout) -> NonNull<u8> {
    assert!(layout.size() != 0);
    if layout.size() < CHUNK_BYTES - HEADER_BYTES && layout.align() <= HEADER_BYTES {
        alloc_node(layout)
    } else {
        alloc_bytes(layout)
    }
}

/// Releases an initialized or uninitialized list buffer after its elements are gone.
///
/// # Safety
///
/// The pointer and layout must describe a live allocation from alloc_list_bytes.
#[inline]
pub(crate) unsafe fn dealloc_list_bytes(ptr: NonNull<u8>, layout: Layout) {
    if layout.size() < CHUNK_BYTES - HEADER_BYTES && layout.align() <= HEADER_BYTES {
        // SAFETY: the allocation owns one count of its chunk.
        unsafe { release(chunk_of(ptr)) };
    } else {
        // SAFETY: the allocation used alloc_bytes with this layout.
        unsafe { dealloc_bytes(ptr, layout) };
    }
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
        struct Count(NonNull<ChunkHeader>);
        impl Drop for Count {
            fn drop(&mut self) {
                // SAFETY: this guard owns the node's chunk count.
                unsafe { release(self.0) };
            }
        }
        let _count = Count(chunk_of(self.ptr));
        // SAFETY: the node is owned and initialized, dropped once; then its count goes.
        unsafe { ptr::drop_in_place(self.ptr.as_ptr()) };
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
