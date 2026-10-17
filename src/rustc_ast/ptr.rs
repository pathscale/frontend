//! `P<T>`, the owning pointer for boxed AST nodes.
//!
//! A one-word owning handle with the parts of `Box` the AST uses: `Deref`, `DerefMut`, a deep
//! `Clone`, and `into_inner` in place of moving out with `*p`.
//!
//! Nodes are bump-allocated into 64 KiB chunks, one current chunk per thread, instead of one
//! global allocation each. A chunk is aligned to its size, so a node finds its chunk by masking
//! its address. The chunk header counts issued allocations, reserved allocation counts, and
//! one pin while it is a thread's current chunk. The current chunk reserves counts in batches,
//! consuming them locally for new nodes. Dropping a node returns its count locally or batches
//! its release; retiring a chunk releases its pin and unused reservations. The chunk goes back
//! to the global allocator when the count reaches zero. Nothing ties a node to a session or a
//! thread: a node can be dropped anywhere, and retired memory is held only while some allocation
//! of the chunk is alive.

use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use core::fmt;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};

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

static CHUNK_ALLOCS: AtomicU64 = AtomicU64::new(0);
static CHUNK_BATCH_RESERVES: AtomicU64 = AtomicU64::new(0);
static CHUNK_ATOMIC_RELEASES: AtomicU64 = AtomicU64::new(0);
static CHUNK_DEALLOCS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArenaMetrics {
    pub chunk_allocs: u64,
    pub chunk_batch_reserves: u64,
    pub chunk_atomic_releases: u64,
    pub chunk_deallocs: u64,
}

#[inline]
pub fn arena_metrics() -> ArenaMetrics {
    ArenaMetrics {
        chunk_allocs: CHUNK_ALLOCS.load(Ordering::Relaxed),
        chunk_batch_reserves: CHUNK_BATCH_RESERVES.load(Ordering::Relaxed),
        chunk_atomic_releases: CHUNK_ATOMIC_RELEASES.load(Ordering::Relaxed),
        chunk_deallocs: CHUNK_DEALLOCS.load(Ordering::Relaxed),
    }
}

#[inline]
pub fn reset_arena_metrics() {
    CHUNK_ALLOCS.store(0, Ordering::Relaxed);
    CHUNK_BATCH_RESERVES.store(0, Ordering::Relaxed);
    CHUNK_ATOMIC_RELEASES.store(0, Ordering::Relaxed);
    CHUNK_DEALLOCS.store(0, Ordering::Relaxed);
}

#[repr(C, align(64))]
struct ChunkHeader {
    /// Issued allocations, unused reservations, and the current-chunk pin.
    live: AtomicUsize,
}

fn chunk_layout() -> Layout {
    // SAFETY: CHUNK_BYTES is a nonzero power of two, and the size is the same value,
    // within isize::MAX. Constructing a layout accesses no memory or references.
    unsafe { Layout::from_size_align_unchecked(CHUNK_BYTES, CHUNK_BYTES) }
}

/// A fresh chunk with a count of one.
#[cold]
fn new_chunk() -> NonNull<ChunkHeader> {
    // SAFETY: chunk_layout has a valid nonzero size and alignment. Allocation creates fresh,
    // uniquely owned uninitialized bytes; no references are formed until initialization.
    let raw = unsafe { alloc(chunk_layout()) }.cast::<ChunkHeader>();
    let Some(chunk) = NonNull::new(raw) else { handle_alloc_error(chunk_layout()) };
    // SAFETY: the fresh chunk is aligned to CHUNK_BYTES, has room for the header, and is
    // uniquely owned. This initializes the header before any reference to it is formed.
    unsafe { chunk.as_ptr().write(ChunkHeader { live: AtomicUsize::new(1) }) };
    CHUNK_ALLOCS.fetch_add(1, Ordering::Relaxed);
    chunk
}

/// Returns one issued count locally, batches it, or frees it immediately.
///
/// # Safety
///
/// The caller owns one count of a live chunk, and gives it up.
#[inline]
// SAFETY: the caller transfers one count of an initialized, aligned live chunk header.
// That count keeps the header in bounds and alive until Current or release_counts consumes it.
unsafe fn release_from_chunk(chunk: NonNull<ChunkHeader>) {
    if CURRENT.with(Current::new, |cur| cur.release_from_chunk(chunk)).is_none() {
        // SAFETY: TLS did not accept the count; the caller's count still keeps the initialized,
        // aligned header alive, and no references to allocation contents are accessed.
        unsafe { release_counts(chunk, 1) };
    }
}

/// Gives up a pin and unused reservations, or one issued allocation count.
///
/// # Safety
///
/// The caller owns `counts` counts of this live chunk; counts is nonzero.
#[inline]
// SAFETY: the caller transfers nonzero owned counts of a live, aligned, initialized header.
// Atomic access shares only the header; the final release uniquely owns the whole chunk.
unsafe fn release_counts(chunk: NonNull<ChunkHeader>, counts: usize) {
    // The same discipline as Arc: every count holder's writes happen before the free.
    // SAFETY: the owned counts keep this aligned, initialized header in bounds and alive;
    // shared access touches only its atomic counter, never the allocation payloads.
    CHUNK_ATOMIC_RELEASES.fetch_add(1, Ordering::Relaxed);
    if unsafe { chunk.as_ref() }.live.fetch_sub(counts, Ordering::Release) == counts {
        fence(Ordering::Acquire);
        // SAFETY: this is the original aligned allocation base and layout. The last counts
        // are gone after the acquire fence, so no live values or references need the bytes.
        CHUNK_DEALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { dealloc(chunk.as_ptr().cast(), chunk_layout()) };
    }
}

/// The chunk that holds the node at `node`.
#[inline]
fn chunk_of<T>(node: NonNull<T>) -> NonNull<ChunkHeader> {
    let addr = node.as_ptr() as usize & !(CHUNK_BYTES - 1);
    // SAFETY: callers pass only nodes from CHUNK_BYTES-aligned chunks, after HEADER_BYTES.
    // Masking yields the nonzero, aligned header address within that same allocation.
    // This constructs a pointer only; it does not access initialized data or create aliases.
    unsafe { NonNull::new_unchecked(addr as *mut ChunkHeader) }
}

/// A thread's current chunk, next free byte, and unissued ownership counts.
pub(crate) struct Current {
    chunk: Option<NonNull<ChunkHeader>>,
    next: usize,
    reserved: usize,
    drop_depth: usize,
    pending_chunk: Option<NonNull<ChunkHeader>>,
    pending_count: usize,
}

impl Current {
    fn new() -> Self {
        Self {
            chunk: None,
            next: CHUNK_BYTES,
            reserved: 0,
            drop_depth: 0,
            pending_chunk: None,
            pending_count: 0,
        }
    }

    fn flush_pending(&mut self) {
        if let Some(chunk) = self.pending_chunk.take() {
            let counts = core::mem::take(&mut self.pending_count);
            // SAFETY: pending counts were transferred from dead allocations in this live,
            // aligned chunk. They keep its initialized header alive until this release;
            // no references to those allocation contents remain.
            unsafe { release_counts(chunk, counts) };
        }
    }

    /// Takes ownership of one issued count without borrowing any allocation contents.
    fn release_from_chunk(&mut self, chunk: NonNull<ChunkHeader>) {
        if self.chunk == Some(chunk) && self.reserved < COUNT_BATCH {
            self.reserved += 1;
        } else if self.drop_depth != 0 {
            if self.pending_chunk != Some(chunk) {
                self.flush_pending();
                self.pending_chunk = Some(chunk);
            }
            // Each pending count is still included in live, which is at most isize::MAX.
            self.pending_count += 1;
        } else {
            // SAFETY: this method receives one owned count of the live, aligned chunk;
            // the header is initialized and its payload is no longer accessed by this owner.
            unsafe { release_counts(chunk, 1) };
        }
    }

    fn retire(&mut self) {
        self.flush_pending();
        if let Some(chunk) = self.chunk.take() {
            let counts = 1 + core::mem::take(&mut self.reserved);
            // SAFETY: this value owns the pin and all unissued reservations of the aligned,
            // initialized header. Pending counts are flushed before the pin can disappear;
            // issued allocations retain separate counts and no payload is accessed here.
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

/// This thread's `Current`, found with one lookup. The slot is a `Box` owned by the thread-local
/// key, so its address is stable until the thread exits, and `ParseArena` swaps the contents in
/// place. The caller makes a short-lived `&mut` from the pointer for each access and never holds
/// one across code that can run another lookup (a drop, an allocation). `None` when the thread
/// is out of thread-local keys.
#[inline]
pub(crate) fn current_slot() -> Option<NonNull<Current>> {
    CURRENT.with(Current::new, |cur| NonNull::from(cur))
}

#[inline]
fn current_ptr() -> Option<NonNull<Current>> {
    current_slot()
}

/// Coalesces consecutive chunk releases through nested drops, including unwinding.
#[inline]
pub(crate) fn with_drop_batch<R>(f: impl FnOnce() -> R) -> R {
    struct Batch(Option<NonNull<Current>>);
    impl Drop for Batch {
        fn drop(&mut self) {
            if let Some(cur) = self.0 {
                // SAFETY: the pointer came from current_ptr on this thread, whose slot outlives
                // every drop on it. No other reference to it is live here: the borrow is
                // created now and ends before this function returns.
                let cur = unsafe { &mut *cur.as_ptr() };
                cur.drop_depth -= 1;
                if cur.drop_depth == 0 {
                    cur.flush_pending();
                }
            }
        }
    }

    let cur = current_ptr();
    if let Some(cur) = cur {
        // SAFETY: as in Batch::drop; this borrow ends at the end of the statement.
        let cur = unsafe { &mut *cur.as_ptr() };
        cur.drop_depth = cur.drop_depth.checked_add(1).expect("AST drop batch depth overflow");
    }
    let _batch = Batch(cur);
    // No borrow of the slot is live before f can recurse, allocate, or switch ParseArena.
    f()
}

/// An owned parsing job's allocation epoch. Returned nodes retain their own chunk counts.
#[cfg(feature = "parallel")]
pub(crate) struct ParseArena(Option<Current>);

#[cfg(feature = "parallel")]
impl ParseArena {
    pub(crate) fn enter() -> Self {
        Self(CURRENT.with(Current::new, |current| {
            current.flush_pending();
            let mut next = Current::new();
            // Batch scopes belong to the thread's call stack, not an allocation epoch.
            next.drop_depth = core::mem::take(&mut current.drop_depth);
            core::mem::replace(current, next)
        }))
    }
}

#[cfg(feature = "parallel")]
impl Drop for ParseArena {
    fn drop(&mut self) {
        if let Some(mut previous) = self.0.take() {
            let retired = CURRENT.with(Current::new, |current| {
                current.flush_pending();
                previous.drop_depth = core::mem::take(&mut current.drop_depth);
                core::mem::replace(current, previous)
            });
            drop(retired);
        }
    }
}

/// Room for one node of layout, transferring one reserved count to it.
#[inline]
fn alloc_node(layout: Layout) -> NonNull<u8> {
    alloc_node_in(layout, current_slot())
}

/// Allocate using a slot already looked up by a compound buffer operation.
#[inline]
fn alloc_node_in(layout: Layout, slot: Option<NonNull<Current>>) -> NonNull<u8> {
    if let Some(cur) = slot {
        // SAFETY: current_ptr returned this thread's stable slot. This short borrow ends
        // before any caller can allocate, drop a value, or otherwise look up the slot.
        let cur = unsafe { &mut *cur.as_ptr() };
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
            // SAFETY: the current-chunk pin keeps this aligned, initialized header in
            // bounds and alive. Shared access touches only the atomic counter, and only
            // this thread reserves counts; other threads may release issued counts.
            let live = unsafe { &chunk.as_ref().live };
            let mut count = live.load(Ordering::Relaxed);
            loop {
                let available = (isize::MAX as usize).saturating_sub(count);
                assert!(available != 0, "AST chunk count overflow");
                let reserved = count + available.min(COUNT_BATCH);
                match live.compare_exchange_weak(
                    count,
                    reserved,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(previous) => {
                        count = previous;
                        break;
                    }
                    Err(actual) => count = actual,
                }
            }
            cur.reserved = ((isize::MAX as usize) - count).min(COUNT_BATCH);
            CHUNK_BATCH_RESERVES.fetch_add(1, Ordering::Relaxed);
        }
        // One reserved count becomes the returned allocation's owned count.
        cur.reserved -= 1;
        // SAFETY: offset is rounded to the layout's alignment and lies after the header;
        // offset + size is strictly inside the live chunk. These fresh, uninitialized
        // bytes are uniquely allocated, and no reference is formed, even for a ZST.
        unsafe { NonNull::new_unchecked(chunk.as_ptr().cast::<u8>().add(offset)) }
    } else {
        // Out of thread-local keys: a chunk of its own, whose one count is the node's.
        let chunk = new_chunk();
        // SAFETY: HEADER_BYTES is aligned for every accepted layout, and that layout fits
        // strictly inside the fresh chunk after the initialized header. The returned payload
        // is uniquely owned, uninitialized storage and no reference is formed.
        unsafe { NonNull::new_unchecked(chunk.as_ptr().cast::<u8>().add(HEADER_BYTES)) }
    }
}

/// Allocates nonempty storage, sharing AST chunks for small layouts.
#[inline]
pub(crate) fn alloc_bytes(layout: Layout) -> NonNull<u8> {
    assert!(layout.size() != 0);
    if layout.size() <= MAX_NODE_BYTES && layout.align() <= HEADER_BYTES {
        alloc_node(layout)
    } else {
        // SAFETY: Layout guarantees valid size/alignment and size is nonzero. Allocation
        // returns fresh, uniquely owned uninitialized storage without forming references.
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
// SAFETY: the caller supplies the original nonzero layout and uniquely owned live storage.
// Chunk pointers have aligned, initialized headers; global pointers retain their allocation
// provenance. All payload values and references are gone before either release path runs.
pub(crate) unsafe fn dealloc_bytes(ptr: NonNull<u8>, layout: Layout) {
    if layout.size() <= MAX_NODE_BYTES && layout.align() <= HEADER_BYTES {
        // SAFETY: classification matches alloc_bytes; the owned count keeps the aligned,
        // initialized header alive, and no values or references remain in this payload.
        unsafe { release_from_chunk(chunk_of(ptr)) };
    } else {
        // SAFETY: the pointer is the original allocation base, aligned for this exact layout;
        // the caller gives up uniquely owned bytes after all payload values/references end.
        unsafe { dealloc(ptr.as_ptr(), layout) };
    }
}

/// List buffers use the same chunks up to the largest layout that fits after a header.
#[inline]
pub(crate) fn alloc_list_bytes(layout: Layout) -> NonNull<u8> {
    alloc_slice_bytes(layout)
}

#[inline]
fn fits_in_chunk(layout: Layout) -> bool {
    layout.size() < CHUNK_BYTES - HEADER_BYTES && layout.align() <= HEADER_BYTES
}

/// Allocates contiguous, nonempty slice storage using the list allocation classification.
#[inline]
pub(crate) fn alloc_slice_bytes(layout: Layout) -> NonNull<u8> {
    alloc_slice_bytes_in(layout, current_slot())
}

#[inline]
pub(crate) fn alloc_slice_bytes_in(layout: Layout, slot: Option<NonNull<Current>>) -> NonNull<u8> {
    assert!(layout.size() != 0);
    if fits_in_chunk(layout) {
        alloc_node_in(layout, slot)
    } else {
        // Oversized buffers do not use the thread's bump chunk.
        alloc_bytes(layout)
    }
}

/// Releases an initialized or uninitialized list buffer after its elements are gone.
///
/// # Safety
///
/// The pointer and layout must describe a live allocation from alloc_list_bytes.
/// All contained values must have been dropped and no references may remain.
#[inline]
// SAFETY: list and slice allocation classification is identical, so the caller's original
// nonzero layout describes uniquely owned, aligned storage with no live payload values.
pub(crate) unsafe fn dealloc_list_bytes(ptr: NonNull<u8>, layout: Layout) {
    // SAFETY: alloc_list_bytes delegates to alloc_slice_bytes with this exact layout;
    // the caller has relinquished all references and values in the allocation.
    unsafe { dealloc_slice_bytes(ptr, layout) };
}

/// Extends uniquely owned list/slice storage only at the active chunk's exact tip.
///
/// # Safety
///
/// `ptr` must be a live allocation from the list/slice family with nonzero `old_layout`.
/// `new_layout` must be nonzero, have the same alignment and a nondecreasing size.
/// No references to the allocation may remain during growth. On success, its layout becomes
/// `new_layout`; the added bytes are uninitialized. Failure leaves the old allocation intact.
#[inline]
// SAFETY: the caller supplies uniquely owned, aligned storage with the matching old layout.
// Classification is checked before deriving a header. Only unused bytes after the exact tip
// become owned, and no references to existing or uninitialized payload bytes are formed.
pub(crate) unsafe fn try_grow_in_chunk_in_place_in(
    ptr: NonNull<u8>,
    old_layout: Layout,
    new_layout: Layout,
    slot: Option<NonNull<Current>>,
) -> bool {
    if old_layout.size() == 0
        || new_layout.size() < old_layout.size()
        || new_layout.align() != old_layout.align()
        || !fits_in_chunk(old_layout)
        || !fits_in_chunk(new_layout)
    {
        return false;
    }
    let chunk = chunk_of(ptr);
    let Some(cur) = slot else { return false };
    // SAFETY: current_ptr returned this thread's stable slot. This short mutable borrow
    // performs only checks and a cursor update, with no allocation or callback.
    let cur = unsafe { &mut *cur.as_ptr() };
    if cur.chunk != Some(chunk) {
        return false;
    }
    let offset = ptr.as_ptr() as usize - chunk.as_ptr() as usize;
    if offset + old_layout.size() != cur.next || offset + new_layout.size() >= CHUNK_BYTES {
        return false;
    }
    cur.next = offset + new_layout.size();
    true
}

/// Releases list/slice storage, reclaiming the active chunk's tip when possible.
///
/// # Safety
///
/// `ptr` and the nonzero `layout` must describe a live list/slice allocation. All values
/// must have been dropped and all references relinquished; the caller gives up its count.
#[inline]
// SAFETY: the caller transfers uniquely owned storage with its matching layout. Only chunk
// allocations are masked; their initialized, aligned headers remain live through the owned
// count. Rewinding makes only the dead allocation's bytes available for later initialization.
pub(crate) unsafe fn dealloc_slice_bytes(ptr: NonNull<u8>, layout: Layout) {
    if fits_in_chunk(layout) {
        let chunk = chunk_of(ptr);
        let returned = CURRENT
            .with(Current::new, |cur| {
                if cur.chunk == Some(chunk) {
                    let offset = ptr.as_ptr() as usize - chunk.as_ptr() as usize;
                    if offset + layout.size() == cur.next {
                        cur.next = offset;
                    }
                }
                cur.release_from_chunk(chunk);
            })
            .is_some();
        if !returned {
            // SAFETY: TLS did not consume the allocation's count. It still keeps the aligned,
            // initialized header alive, and the caller no longer accesses the payload.
            unsafe { release_counts(chunk, 1) };
        }
    } else {
        // SAFETY: global slice storage used alloc_bytes with this exact layout; its uniquely
        // owned payload has no live values or references and is aligned for that layout.
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
        // SAFETY: alloc_node returned fresh, uniquely owned storage sized and aligned for T
        // within a live chunk. This initializes T before any references to it are formed.
        unsafe { slot.as_ptr().write(value) };
        P { ptr: slot, _owns: PhantomData }
    }

    /// Moves the node out, which `*p` does for a `Box`.
    #[inline]
    pub fn into_inner(self) -> T {
        let this = core::mem::ManuallyDrop::new(self);
        // SAFETY: this uniquely owns an initialized T at an aligned, in-bounds chunk slot.
        // ManuallyDrop prevents a second read/drop, and consuming self ends its borrows.
        let value = unsafe { this.ptr.as_ptr().read() };
        // SAFETY: the node's owned count keeps the aligned, initialized header alive. Its
        // value was moved out once, and no payload references survive the consumed handle.
        unsafe { release_from_chunk(chunk_of(this.ptr)) };
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
        // SAFETY: the owned count keeps this initialized, aligned T in bounds and alive.
        // Borrowing self permits shared access only, and the reference cannot outlive self.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for P<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the owned count keeps this initialized, aligned T in bounds and alive.
        // The unique borrow of its sole owning handle excludes all other payload references.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for P<T> {
    #[inline]
    fn drop(&mut self) {
        if !core::mem::needs_drop::<T>() {
            // SAFETY: this handle uniquely owns an initialized, aligned node in its chunk;
            // T has no destructor, and the owned count is relinquished exactly once.
            unsafe { release_from_chunk(chunk_of(self.ptr)) };
            return;
        }
        /// Gives up the node's count, then leaves the drop batch entered below, on normal
        /// return and on unwind. One thread-local lookup serves the whole drop.
        struct Finish(NonNull<ChunkHeader>, Option<NonNull<Current>>);
        impl Drop for Finish {
            fn drop(&mut self) {
                match self.1 {
                    Some(cur) => {
                        // SAFETY: the pointer came from current_ptr on this thread, whose slot
                        // outlives this drop. The borrow is created here, after the payload
                        // drop ended, and ends with this function. The guard owns the count of
                        // a live, initialized and aligned header; the payload is gone.
                        let cur = unsafe { &mut *cur.as_ptr() };
                        // The batch is still entered, so this may be deferred, as before.
                        cur.release_from_chunk(self.0);
                        cur.drop_depth -= 1;
                        if cur.drop_depth == 0 {
                            cur.flush_pending();
                        }
                    }
                    // SAFETY: no thread-local slot; the guard owns one count of a live header
                    // and the payload is gone.
                    None => unsafe { release_counts(self.0, 1) },
                }
            }
        }
        let cur = current_ptr();
        if let Some(cur) = cur {
            // SAFETY: as in Finish::drop; the borrow ends with this statement, before the
            // payload drop can recurse, allocate, or switch ParseArena.
            let cur = unsafe { &mut *cur.as_ptr() };
            cur.drop_depth = cur.drop_depth.checked_add(1).expect("AST drop batch depth overflow");
        }
        let _finish = Finish(chunk_of(self.ptr), cur);
        // SAFETY: self uniquely owns a properly aligned, initialized T within this live
        // chunk. It is dropped once, with its count held until normal return or unwind.
        // No `&mut Current` is held across this call.
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
