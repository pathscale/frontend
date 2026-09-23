//! The arena, a fast but limited type of allocator.
//!
//! Arenas are a type of allocator that destroy the objects within, all at
//! once, once the arena itself is destroyed. They do not support deallocation
//! of individual objects while the arena itself is still alive. The benefit
//! of an arena is very fast allocation; just a pointer bump.
//!
//! This crate implements several kinds of arena.

// tidy-alphabetical-start
// tidy-alphabetical-end

#![allow(clippy::mut_from_ref)] // Arena allocators are one place where this pattern is fine.
#![deny(unsafe_op_in_unsafe_fn)]
#![doc(test(no_crate_inject, attr(deny(warnings))))]
#![no_std]

// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `crates/libc-wrapper`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
#[macro_use]
extern crate alloc;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::cell::{Cell, RefCell};
use core::mem::{self, MaybeUninit};
use core::ptr::{self, NonNull};
use core::{cmp, hint, slice};

use smallvec::SmallVec;

/// This calls the passed function while ensuring it won't be inlined into the caller.
#[inline(never)]
#[cold]
fn outline<F: FnOnce() -> R, R>(f: F) -> R {
    f()
}

/// A chunk of raw bytes for `DroplessArena`.
///
/// Only `DroplessArena` uses this now, so it is bytes only and its `Drop` is not generic:
/// it frees memory and runs no element destructors, which dropck has nothing to object to.
/// `TypedArena` keeps its elements in `Vec`s instead (see there).
struct ArenaChunk {
    /// The raw storage for the arena chunk.
    storage: NonNull<[MaybeUninit<u8>]>,
}

impl Drop for ArenaChunk {
    fn drop(&mut self) {
        unsafe { drop(Box::from_raw(self.storage.as_mut())) }
    }
}

impl ArenaChunk {
    #[inline]
    fn new(capacity: usize) -> ArenaChunk {
        ArenaChunk { storage: NonNull::from(Box::leak(Box::new_uninit_slice(capacity))) }
    }

    // Returns a pointer to the first allocated byte.
    #[inline]
    fn start(&mut self) -> *mut u8 {
        self.storage.as_ptr() as *mut u8
    }

    // Returns a pointer to the end of the allocated space.
    #[inline]
    fn end(&mut self) -> *mut u8 {
        // SAFETY: `storage.len()` bytes are allocated from `start`.
        unsafe { self.start().add(self.storage.len()) }
    }
}

// The arenas start with PAGE-sized chunks, and then each new chunk is twice as
// big as its predecessor, up until we reach HUGE_PAGE-sized chunks, whereupon
// we stop growing. This scales well, from arenas that are barely used up to
// arenas that are used for 100s of MiBs. Note also that the chosen sizes match
// the usual sizes of pages and huge pages on Linux.
const PAGE: usize = 4096;
const HUGE_PAGE: usize = 2 * 1024 * 1024;

/// An arena that can hold objects of only one type.
///
/// # Why the chunks are `Vec`s
///
/// Upstream stores raw chunks and drops the elements in `impl<#[may_dangle] T> Drop`. The
/// compiler's arena is borrowed for the same `'tcx` its element types carry
/// (`WorkerLocal<Arena<'tcx>>` behind `&'tcx`), and dropck accepts that only if the arena's
/// destructor is known not to touch those borrows. `#[may_dangle]` (dropck_eyepatch) is
/// unstable, so this type has no `Drop` impl at all, the way the `typed-arena` crate does it:
/// each chunk is a `Vec<T>` whose capacity is reserved up front and which is only ever pushed to
/// within that capacity, so its buffer never reallocates and element addresses never move.
/// Dropping the arena drops the `Vec`s, and `alloc::vec::Vec` carries the eyepatch, so dropck
/// reasons about `T` exactly as it would for a `Vec<T>` local.
pub struct TypedArena<T> {
    /// The chunks, oldest first. Only the last one has spare capacity.
    chunks: RefCell<Vec<Vec<T>>>,
}

impl<T> Default for TypedArena<T> {
    /// Creates a new `TypedArena`.
    fn default() -> TypedArena<T> {
        // No chunk yet: the first allocation grows.
        TypedArena { chunks: RefCell::new(Vec::new()) }
    }
}

impl<T> TypedArena<T> {
    /// Allocates an object in the `TypedArena`, returning a reference to it.
    #[inline]
    pub fn alloc(&self, object: T) -> &mut T {
        assert!(size_of::<T>() != 0);

        let mut chunks = self.chunks.borrow_mut();
        if !Self::has_room(&chunks, 1) {
            Self::grow(&mut chunks, 1);
        }
        let chunk = chunks.last_mut().unwrap();
        // Within capacity (checked above), so this never reallocates and earlier elements stay
        // where they are.
        chunk.push(object);
        let len = chunk.len();
        // SAFETY: the element at `len - 1` was just written. The buffer is never reallocated
        // (pushes stay within capacity) and never freed before the arena, and no other
        // reference to this slot is ever handed out, so the `&mut` is unique for `&self`'s
        // lifetime.
        unsafe { &mut *chunk.as_mut_ptr().add(len - 1) }
    }

    /// Whether the last chunk can take `additional` more elements without reallocating.
    #[inline]
    fn has_room(chunks: &[Vec<T>], additional: usize) -> bool {
        chunks.last().is_some_and(|chunk| chunk.capacity() - chunk.len() >= additional)
    }

    /// Allocates the elements of this iterator into a contiguous slice in the `TypedArena`.
    ///
    /// Note: for reasons of reentrancy and panic safety we collect into a `SmallVec<[_; 8]>` before
    /// storing the elements in the arena.
    #[inline]
    pub fn alloc_from_iter<I: IntoIterator<Item = T>>(&self, iter: I) -> &mut [T] {
        self.try_alloc_from_iter(iter.into_iter().map(Ok::<T, core::convert::Infallible>))
            .unwrap_or_else(|never| match never {})
    }

    /// Allocates the elements of this iterator into a contiguous slice in the `TypedArena`.
    ///
    /// Note: for reasons of reentrancy and panic safety we collect into a `SmallVec<[_; 8]>` before
    /// storing the elements in the arena.
    #[inline]
    pub fn try_alloc_from_iter<E>(
        &self,
        iter: impl IntoIterator<Item = Result<T, E>>,
    ) -> Result<&mut [T], E> {
        // These arenas are reentrant: `iter` may hold a reference to `self` and allocate in it
        // while it runs. So the elements are collected first, before the chunks are borrowed,
        // which also means nothing is half-initialized if the iterator panics.
        assert!(size_of::<T>() != 0);

        let vec: Result<SmallVec<[T; 8]>, E> = iter.into_iter().collect();
        let vec = vec?;
        if vec.is_empty() {
            return Ok(&mut []);
        }
        let len = vec.len();

        let mut chunks = self.chunks.borrow_mut();
        // The slice must be contiguous, so if the current chunk lacks the room, start a new one
        // sized to fit; `grow` guarantees at least `len`.
        if !Self::has_room(&chunks, len) {
            Self::grow(&mut chunks, len);
        }
        let chunk = chunks.last_mut().unwrap();
        let start = chunk.len();
        // Within capacity (checked above), so `extend` never reallocates.
        chunk.extend(vec);
        // SAFETY: `start..start + len` was just written, in one buffer that is never reallocated
        // or freed before the arena, and no other reference to these slots is handed out.
        Ok(unsafe { slice::from_raw_parts_mut(chunk.as_mut_ptr().add(start), len) })
    }

    /// Starts a new chunk that can hold at least `additional` elements.
    #[inline(never)]
    #[cold]
    fn grow(chunks: &mut Vec<Vec<T>>, additional: usize) {
        // We need the element size to convert chunk sizes (ranging from
        // PAGE to HUGE_PAGE bytes) to element counts.
        let elem_size = cmp::max(1, size_of::<T>());
        let mut new_cap;
        if let Some(last_chunk) = chunks.last() {
            // If the previous chunk's len is less than HUGE_PAGE
            // bytes, then this chunk will be least double the previous
            // chunk's size.
            new_cap = last_chunk.capacity().min(HUGE_PAGE / elem_size / 2);
            new_cap *= 2;
        } else {
            new_cap = PAGE / elem_size;
        }
        // Also ensure that this chunk can fit `additional`.
        new_cap = cmp::max(additional, new_cap);

        // A new chunk rather than a `reserve` on the old one: reallocating would move elements
        // that references already point at.
        chunks.push(Vec::with_capacity(new_cap));
    }
}

#[inline(always)]
fn align_down(val: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    val & !(align - 1)
}

#[inline(always)]
fn align_up(val: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (val + align - 1) & !(align - 1)
}

// Pointer alignment is common in compiler types, so keep `DroplessArena` aligned to them
// to optimize away alignment code.
const DROPLESS_ALIGNMENT: usize = align_of::<usize>();

/// An arena that can hold objects of multiple different types that impl `Copy`
/// and/or satisfy `!mem::needs_drop`.
pub struct DroplessArena {
    /// A pointer to the start of the free space.
    start: Cell<*mut u8>,

    /// A pointer to the end of free space.
    ///
    /// The allocation proceeds downwards from the end of the chunk towards the
    /// start. (This is slightly simpler and faster than allocating upwards,
    /// see <https://fitzgeraldnick.com/2019/11/01/always-bump-downwards.html>.)
    /// When this pointer crosses the start pointer, a new chunk is allocated.
    ///
    /// This is kept aligned to DROPLESS_ALIGNMENT.
    end: Cell<*mut u8>,

    /// A vector of arena chunks.
    chunks: RefCell<Vec<ArenaChunk>>,
}

unsafe impl Send for DroplessArena {}

impl Default for DroplessArena {
    #[inline]
    fn default() -> DroplessArena {
        DroplessArena {
            // We set both `start` and `end` to 0 so that the first call to
            // alloc() will trigger a grow().
            start: Cell::new(ptr::null_mut()),
            end: Cell::new(ptr::null_mut()),
            chunks: Default::default(),
        }
    }
}

impl DroplessArena {
    #[inline(never)]
    #[cold]
    fn grow(&self, layout: Layout) {
        // Add some padding so we can align `self.end` while
        // still fitting in a `layout` allocation.
        let additional = layout.size() + cmp::max(DROPLESS_ALIGNMENT, layout.align()) - 1;

        let mut chunks = self.chunks.borrow_mut();
        let mut new_cap;
        if let Some(last_chunk) = chunks.last_mut() {
            // If the previous chunk's len is less than HUGE_PAGE
            // bytes, then this chunk will be least double the previous
            // chunk's size.
            new_cap = last_chunk.storage.len().min(HUGE_PAGE / 2);
            new_cap *= 2;
        } else {
            new_cap = PAGE;
        }
        // Also ensure that this chunk can fit `additional`.
        new_cap = cmp::max(additional, new_cap);

        // `push` then `last_mut` rather than `Vec::push_mut`, which is unstable.
        chunks.push(ArenaChunk::new(align_up(new_cap, PAGE)));
        let chunk = chunks.last_mut().unwrap();
        self.start.set(chunk.start());

        // Align the end to DROPLESS_ALIGNMENT.
        let end = align_down(chunk.end().addr(), DROPLESS_ALIGNMENT);

        // Make sure we don't go past `start`. This should not happen since the allocation
        // should be at least DROPLESS_ALIGNMENT - 1 bytes.
        debug_assert!(chunk.start().addr() <= end);

        self.end.set(chunk.end().with_addr(end));
    }

    #[inline]
    pub fn alloc_raw(&self, layout: Layout) -> *mut u8 {
        assert!(layout.size() != 0);

        // This loop executes once or twice: if allocation fails the first
        // time, the `grow` ensures it will succeed the second time.
        loop {
            let start = self.start.get().addr();
            let old_end = self.end.get();
            let end = old_end.addr();

            // Align allocated bytes so that `self.end` stays aligned to
            // DROPLESS_ALIGNMENT.
            let bytes = align_up(layout.size(), DROPLESS_ALIGNMENT);

            // Tell LLVM that `end` is aligned to DROPLESS_ALIGNMENT.
            unsafe { hint::assert_unchecked(end == align_down(end, DROPLESS_ALIGNMENT)) };

            if let Some(sub) = end.checked_sub(bytes) {
                let new_end = align_down(sub, layout.align());
                if start <= new_end {
                    let new_end = old_end.with_addr(new_end);
                    // `new_end` is aligned to DROPLESS_ALIGNMENT as `align_down`
                    // preserves alignment as both `end` and `bytes` are already
                    // aligned to DROPLESS_ALIGNMENT.
                    self.end.set(new_end);
                    return new_end;
                }
            }

            // No free space left. Allocate a new chunk to satisfy the request.
            // On failure the grow will panic or abort.
            self.grow(layout);
        }
    }

    #[inline]
    pub fn alloc<T>(&self, object: T) -> &mut T {
        assert!(!mem::needs_drop::<T>());
        assert!(size_of::<T>() != 0);

        let mem = self.alloc_raw(Layout::new::<T>()) as *mut T;

        unsafe {
            // Write into uninitialized memory.
            ptr::write(mem, object);
            &mut *mem
        }
    }

    /// Allocates a slice of objects that are copied into the `DroplessArena`, returning a mutable
    /// reference to it. Will panic if passed a zero-sized type.
    ///
    /// Panics:
    ///
    ///  - Zero-sized types
    ///  - Zero-length slices
    #[inline]
    pub fn alloc_slice<T>(&self, slice: &[T]) -> &mut [T]
    where
        T: Copy,
    {
        assert!(!mem::needs_drop::<T>());
        assert!(size_of::<T>() != 0);
        assert!(!slice.is_empty());

        let mem = self.alloc_raw(Layout::for_value::<[T]>(slice)) as *mut T;

        unsafe {
            mem.copy_from_nonoverlapping(slice.as_ptr(), slice.len());
            slice::from_raw_parts_mut(mem, slice.len())
        }
    }

    /// Allocates a string slice that is copied into the `DroplessArena`, returning a
    /// reference to it. Will panic if passed an empty string.
    ///
    /// Panics:
    ///
    ///  - Zero-length string
    #[inline]
    pub fn alloc_str(&self, string: &str) -> &str {
        let slice = self.alloc_slice(string.as_bytes());

        // SAFETY: the result has a copy of the same valid UTF-8 bytes.
        unsafe { core::str::from_utf8_unchecked(slice) }
    }

    /// # Safety
    ///
    /// The caller must ensure that `mem` is valid for writes up to `size_of::<T>() * len`, and that
    /// that memory stays allocated and not shared for the lifetime of `self`. This must hold even
    /// if `iter.next()` allocates onto `self`.
    #[inline]
    unsafe fn write_from_iter<T, I: Iterator<Item = T>>(
        &self,
        mut iter: I,
        len: usize,
        mem: *mut T,
    ) -> &mut [T] {
        let mut i = 0;
        // Use a manual loop since LLVM manages to optimize it better for
        // slice iterators
        loop {
            // SAFETY: The caller must ensure that `mem` is valid for writes up to
            // `size_of::<T>() * len`.
            unsafe {
                match iter.next() {
                    Some(value) if i < len => mem.add(i).write(value),
                    Some(_) | None => {
                        // We only return as many items as the iterator gave us, even
                        // though it was supposed to give us `len`
                        return slice::from_raw_parts_mut(mem, i);
                    }
                }
            }
            i += 1;
        }
    }

    #[inline]
    pub fn alloc_from_iter<T, I: IntoIterator<Item = T>>(&self, iter: I) -> &mut [T] {
        assert!(!mem::needs_drop::<T>());
        assert!(size_of::<T>() != 0);

        // Warning: this function is reentrant: `iter` could hold a reference to `&self` and
        // allocate additional elements while we're iterating.
        let iter = iter.into_iter();

        let size_hint = iter.size_hint();

        match size_hint {
            (min, Some(max)) if min == max => {
                // We know the exact number of elements the iterator expects to produce here.
                let len = min;

                if len == 0 {
                    return &mut [];
                }

                let mem = self.alloc_raw(Layout::array::<T>(len).unwrap()) as *mut T;
                // SAFETY: `write_from_iter` doesn't touch `self`. It only touches the slice we just
                // reserved. If the iterator panics or doesn't output `len` elements, this will
                // leave some unallocated slots in the arena, which is fine because we do not call
                // `drop`.
                unsafe { self.write_from_iter(iter, len, mem) }
            }
            (_, _) => outline(move || self.try_alloc_from_iter(iter.map(Ok::<T, core::convert::Infallible>)).unwrap_or_else(|never| match never {})),
        }
    }

    #[inline]
    pub fn try_alloc_from_iter<T, E>(
        &self,
        iter: impl IntoIterator<Item = Result<T, E>>,
    ) -> Result<&mut [T], E> {
        // Despite the similarity with `alloc_from_iter`, we cannot reuse their fast case, as we
        // cannot know the minimum length of the iterator in this case.
        assert!(!mem::needs_drop::<T>());
        assert!(size_of::<T>() != 0);

        // Takes care of reentrancy.
        let vec: Result<SmallVec<[T; 8]>, E> = iter.into_iter().collect();
        let mut vec = vec?;
        if vec.is_empty() {
            return Ok(&mut []);
        }
        // Move the content to the arena by copying and then forgetting it.
        let len = vec.len();
        Ok(unsafe {
            let start_ptr = self.alloc_raw(Layout::for_value::<[T]>(vec.as_slice())) as *mut T;
            vec.as_ptr().copy_to_nonoverlapping(start_ptr, len);
            vec.set_len(0);
            slice::from_raw_parts_mut(start_ptr, len)
        })
    }
}

/// Declares an `Arena` that can allocate values of a variety of `Copy`, `needs_drop` and
/// `!needs_drop` types.
///
/// The declared arena actually contains a single [`DroplessArena`], plus a separate
/// [`TypedArena`] for each of the types listed in the body of the macro invocation.
///
/// Any type that is `Copy` can be allocated in the arena without needing to be listed
/// explicitly. Those values will be stored in the [`DroplessArena`].
///
/// Types that are `!Copy` can only be allocated if they are listed in the macro invocation.
/// For types that are `!Copy + needs_drop`, values will be stored in the corresponding
/// [`TypedArena`] and will be dropped when the arena is dropped.
///
/// As an optimization, types that are `!Copy + !needs_drop` will actually be stored in the
/// [`DroplessArena`], and the corresponding [`TypedArena`] will remain empty. This makes
/// better use of the dropless arena's storage blocks, while the overhead of having a few
/// unused typed-arenas is negligible.
#[macro_export]
macro_rules! declare_arena {
    (
    // Each of these entries becomes a `$name: TypedArena<$ty>` field in the arena.
    // This allows values of non-copy type $ty to be allocated in the arena.
    // The field names must be distinct, but have no further significance.
    $(
        $name:ident: $ty:ty,
    )*
) => {
    #[derive(Default)]
    pub struct Arena<'tcx> {
        pub dropless: $crate::DroplessArena,
        $($name: $crate::TypedArena<$ty>,)*
    }

    // `$crate::`, not `rustc_arena::`. Upstream this macro is only ever invoked from a crate that
    // has `rustc_arena` as a dependency, so the bare name resolved through the extern prelude.
    // Here the caller reaches it through a re-export, and only `$crate` names the defining crate
    // from wherever the expansion lands.
    pub trait ArenaAllocatable<'tcx, C = $crate::IsNotCopy>: Sized {
        #[allow(clippy::mut_from_ref)]
        fn allocate_on(self, arena: &'tcx Arena<'tcx>) -> &'tcx mut Self;
        #[allow(clippy::mut_from_ref)]
        fn allocate_from_iter(
            arena: &'tcx Arena<'tcx>,
            iter: impl ::core::iter::IntoIterator<Item = Self>,
        ) -> &'tcx mut [Self];
    }

    // Any type that impls `Copy` can be arena-allocated in the `DroplessArena`.
    impl<'tcx, T: Copy> ArenaAllocatable<'tcx, $crate::IsCopy> for T {
        #[inline]
        #[allow(clippy::mut_from_ref)]
        fn allocate_on(self, arena: &'tcx Arena<'tcx>) -> &'tcx mut Self {
            arena.dropless.alloc(self)
        }
        #[inline]
        #[allow(clippy::mut_from_ref)]
        fn allocate_from_iter(
            arena: &'tcx Arena<'tcx>,
            iter: impl ::core::iter::IntoIterator<Item = Self>,
        ) -> &'tcx mut [Self] {
            arena.dropless.alloc_from_iter(iter)
        }
    }
    $(
        impl<'tcx> ArenaAllocatable<'tcx, $crate::IsNotCopy> for $ty {
            #[inline]
            fn allocate_on(self, arena: &'tcx Arena<'tcx>) -> &'tcx mut Self {
                if !::core::mem::needs_drop::<Self>() {
                    arena.dropless.alloc(self)
                } else {
                    arena.$name.alloc(self)
                }
            }

            #[inline]
            #[allow(clippy::mut_from_ref)]
            fn allocate_from_iter(
                arena: &'tcx Arena<'tcx>,
                iter: impl ::core::iter::IntoIterator<Item = Self>,
            ) -> &'tcx mut [Self] {
                if !::core::mem::needs_drop::<Self>() {
                    arena.dropless.alloc_from_iter(iter)
                } else {
                    arena.$name.alloc_from_iter(iter)
                }
            }
        }
    )*

    impl<'tcx> Arena<'tcx> {
        #[inline]
        #[allow(clippy::mut_from_ref)]
        pub fn alloc<T: ArenaAllocatable<'tcx, C>, C>(&'tcx self, value: T) -> &mut T {
            value.allocate_on(self)
        }

        // Any type that impls `Copy` can have slices be arena-allocated in the `DroplessArena`.
        #[inline]
        #[allow(clippy::mut_from_ref)]
        pub fn alloc_slice<T: ::core::marker::Copy>(&self, value: &[T]) -> &mut [T] {
            if value.is_empty() {
                return &mut [];
            }
            self.dropless.alloc_slice(value)
        }

        #[inline]
        pub fn alloc_str(&self, string: &str) -> &str {
            if string.is_empty() {
                return "";
            }
            self.dropless.alloc_str(string)
        }

        #[allow(clippy::mut_from_ref)]
        pub fn alloc_from_iter<T: ArenaAllocatable<'tcx, C>, C>(
            &'tcx self,
            iter: impl ::core::iter::IntoIterator<Item = T>,
        ) -> &mut [T] {
            T::allocate_from_iter(self, iter)
        }
    }
}
}

// Marker types that let us give different behaviour for arenas allocating
// `Copy` types vs `!Copy` types.
pub struct IsCopy;
pub struct IsNotCopy;

#[cfg(test)]
mod tests;
