//! A file's buffers, allocated once at their final sizes and filled by many tasks at once.
//!
//! Each chunk's part knows, before any is written, exactly where its tokens, groups, newlines
//! and the rest go in the file: the counts of every chunk before it. So each part copies its
//! own patched output into its own ranges, on its own task, and the file is never copied whole
//! on one. The first touch of a fresh buffer's pages (most of a copy's cost at file sizes)
//! spreads across the pool with it.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

use frontend_symbol::Symbol;

use crate::scan::Chunk;
use crate::{Group, Token};

/// A slice of `len` elements written in disjoint ranges by different tasks, then read whole.
struct Slab<T> {
    ptr: *mut MaybeUninit<T>,
    len: usize,
}

// SAFETY: a `Slab` is a uniquely owned allocation; tasks write disjoint ranges through a shared
// reference (see `write`), and it is read only through `into_box`, which takes it by value.
unsafe impl<T: Send> Send for Slab<T> {}
// SAFETY: as above; shared access only ever writes disjoint ranges.
unsafe impl<T: Send> Sync for Slab<T> {}

impl<T: Copy> Slab<T> {
    fn new(len: usize) -> Self {
        let boxed: Box<[MaybeUninit<T>]> = Box::new_uninit_slice(len);
        Slab { ptr: Box::into_raw(boxed).cast::<MaybeUninit<T>>(), len }
    }

    /// Writes `items` at `at`.
    ///
    /// # Safety
    ///
    /// `at + items.len() <= len`, and no other write overlaps this range.
    unsafe fn write(&self, at: usize, items: impl ExactSizeIterator<Item = T>) {
        debug_assert!(at + items.len() <= self.len);
        for (i, item) in items.enumerate() {
            // SAFETY: in bounds, and this range is written by this caller alone.
            unsafe { self.ptr.add(at + i).write(MaybeUninit::new(item)) };
        }
    }

    /// Copies `items` to `at`.
    ///
    /// # Safety
    ///
    /// As [`Slab::write`].
    unsafe fn copy(&self, at: usize, items: &[T]) {
        debug_assert!(at + items.len() <= self.len);
        // SAFETY: in bounds, and this range is written by this caller alone; `MaybeUninit<T>`
        // has `T`'s layout.
        unsafe {
            core::ptr::copy_nonoverlapping(
                items.as_ptr(),
                self.ptr.add(at).cast::<T>(),
                items.len(),
            )
        };
    }

    /// The whole slice.
    ///
    /// # Safety
    ///
    /// Every element has been written, and the writes happen-before this call.
    unsafe fn into_box(self) -> Box<[T]> {
        let slice = core::ptr::slice_from_raw_parts_mut(self.ptr, self.len);
        core::mem::forget(self);
        // SAFETY: the allocation came from `Box::new_uninit_slice(len)` and is fully written.
        unsafe { Box::from_raw(slice).assume_init() }
    }
}

impl<T> Drop for Slab<T> {
    fn drop(&mut self) {
        let slice = core::ptr::slice_from_raw_parts_mut(self.ptr, self.len);
        // SAFETY: the allocation came from `Box::new_uninit_slice(len)`; `MaybeUninit` drops
        // nothing, so freeing a partly written slab is sound.
        drop(unsafe { Box::from_raw(slice) });
    }
}

/// Where one part's output goes: the count of each kind of element in every chunk before it.
#[derive(Clone, Copy, Default)]
pub(crate) struct Offsets {
    pub(crate) tokens: usize,
    pub(crate) groups: usize,
    item_cuts: usize,
    raw_idents: usize,
    suffixes: usize,
    newlines: usize,
    multibyte: usize,
    gallery: usize,
}

impl Offsets {
    /// Each chunk's offsets, and the file's totals.
    pub(crate) fn of(chunks: &[Chunk]) -> (Vec<Offsets>, Offsets) {
        let mut at = Offsets::default();
        let offsets = chunks
            .iter()
            .map(|c| {
                let here = at;
                at.tokens += c.tokens.len();
                at.groups += c.groups.len();
                at.item_cuts += c.item_cuts.len();
                at.raw_idents += c.raw_idents.len();
                at.suffixes += c.suffixes.len();
                at.newlines += c.newlines.len();
                at.multibyte += c.multibyte.len();
                at.gallery += c.gallery_candidates.len();
                here
            })
            .collect();
        (offsets, at)
    }
}

/// The file's buffers, at their final sizes, and how many parts have written theirs.
pub(crate) struct Output {
    tokens: Slab<Token>,
    groups: Slab<Group>,
    item_cuts: Slab<u32>,
    raw_idents: Slab<u32>,
    suffixes: Slab<(u32, Symbol)>,
    newlines: Slab<u32>,
    multibyte: Slab<(u32, u8)>,
    /// Gallery candidates, deduplicated when the file is finished.
    gallery: Slab<(u32, u32)>,
    parts: usize,
    written: AtomicUsize,
}

/// The file's buffers, filled.
pub(crate) struct Filled {
    pub(crate) tokens: Box<[Token]>,
    pub(crate) groups: Box<[Group]>,
    pub(crate) item_cuts: Box<[u32]>,
    pub(crate) raw_idents: Box<[u32]>,
    pub(crate) suffixes: Box<[(u32, Symbol)]>,
    pub(crate) newlines: Box<[u32]>,
    pub(crate) multibyte: Box<[(u32, u8)]>,
    pub(crate) gallery: Box<[(u32, u32)]>,
}

impl Output {
    pub(crate) fn new(totals: Offsets, parts: usize) -> Output {
        Output {
            tokens: Slab::new(totals.tokens),
            groups: Slab::new(totals.groups),
            item_cuts: Slab::new(totals.item_cuts),
            raw_idents: Slab::new(totals.raw_idents),
            suffixes: Slab::new(totals.suffixes),
            newlines: Slab::new(totals.newlines),
            multibyte: Slab::new(totals.multibyte),
            gallery: Slab::new(totals.gallery),
            parts,
            written: AtomicUsize::new(0),
        }
    }

    /// Copies one patched chunk to its ranges.
    ///
    /// # Safety
    ///
    /// `at` is this chunk's entry of [`Offsets::of`] over the chunks this output was sized
    /// for, `chunk` is that chunk, and each chunk is written once.
    pub(crate) unsafe fn write(&self, at: Offsets, chunk: &Chunk) {
        // SAFETY: the offsets are prefix sums of the same chunks' counts, so each chunk's
        // ranges are in bounds and disjoint from every other chunk's.
        unsafe {
            self.tokens.copy(at.tokens, &chunk.tokens);
            self.groups.copy(at.groups, &chunk.groups);
            self.item_cuts.copy(at.item_cuts, &chunk.item_cuts);
            self.raw_idents.copy(at.raw_idents, &chunk.raw_idents);
            self.suffixes.write(
                at.suffixes,
                chunk.suffixes.iter().map(|&(token, symbol)| (token, Symbol::new(symbol))),
            );
            self.newlines.copy(at.newlines, &chunk.newlines);
            self.multibyte.copy(at.multibyte, &chunk.multibyte);
            self.gallery.copy(at.gallery, &chunk.gallery_candidates);
        }
        self.written.fetch_add(1, Ordering::Release);
    }

    /// The filled buffers. Panics unless every part has been written.
    pub(crate) fn finish(self) -> Filled {
        assert_eq!(
            self.written.load(Ordering::Acquire),
            self.parts,
            "every part is patched before the file is finished"
        );
        // SAFETY: every part wrote its ranges, which together cover each slab (the offsets
        // are prefix sums), and the acquire load orders those writes before these reads.
        unsafe {
            Filled {
                tokens: self.tokens.into_box(),
                groups: self.groups.into_box(),
                item_cuts: self.item_cuts.into_box(),
                raw_idents: self.raw_idents.into_box(),
                suffixes: self.suffixes.into_box(),
                newlines: self.newlines.into_box(),
                multibyte: self.multibyte.into_box(),
                gallery: self.gallery.into_box(),
            }
        }
    }
}
