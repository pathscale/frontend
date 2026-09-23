// Each test pins a property this arena promises and can break: references survive chunk growth,
// every element drops exactly once, the iterator paths are reentrant and leave nothing half
// built, and `DroplessArena` hands back aligned bytes that hold what was written.
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::alloc::Layout;
use core::cell::Cell;

use super::{DroplessArena, TypedArena};

/// Enough elements to force several chunk growths: the first chunk holds one page of them.
const PAST_SEVERAL_CHUNKS: usize = 20_000;

struct Counted<'a> {
    value: usize,
    drops: &'a Cell<usize>,
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn a_reference_survives_every_later_growth() {
    let arena = TypedArena::default();
    let early: Vec<(*const usize, &usize)> = (0..PAST_SEVERAL_CHUNKS)
        .map(|value| {
            let slot = &*arena.alloc(value);
            (core::ptr::from_ref(slot), slot)
        })
        .collect();
    // Grow again after every reference above was taken.
    let late = arena.alloc_from_iter(0..PAST_SEVERAL_CHUNKS);
    assert_eq!(late.len(), PAST_SEVERAL_CHUNKS);
    assert!(
        early
            .iter()
            .enumerate()
            .all(|(value, (address, slot))| **slot == value && core::ptr::eq(*address, *slot)),
        "an element moved or changed when the arena grew"
    );
    assert!(late.iter().copied().eq(0..PAST_SEVERAL_CHUNKS));
}

#[test]
fn every_element_drops_exactly_once_when_the_arena_does() {
    let drops = Cell::new(0);
    {
        let arena = TypedArena::default();
        (0..PAST_SEVERAL_CHUNKS).for_each(|value| {
            arena.alloc(Counted { value, drops: &drops });
        });
        let slice = arena.alloc_from_iter((0..100).map(|value| Counted { value, drops: &drops }));
        assert_eq!(slice.last().map(|counted| counted.value), Some(99));
        assert_eq!(drops.get(), 0, "nothing may drop while the arena is alive");
    }
    assert_eq!(drops.get(), PAST_SEVERAL_CHUNKS + 100);
}

#[test]
fn a_slice_larger_than_the_chunk_left_is_contiguous_and_in_order() {
    let arena = TypedArena::default();
    arena.alloc(String::from("fills part of the first chunk"));
    let words = arena.alloc_from_iter((0..PAST_SEVERAL_CHUNKS).map(|n| n.to_string()));
    assert!(
        words.iter().map(String::as_str).eq((0..PAST_SEVERAL_CHUNKS)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str))
    );
}

#[test]
fn an_iterator_may_allocate_into_the_arena_it_is_filling() {
    let arena: TypedArena<usize> = TypedArena::default();
    // The iterator borrows the arena and allocates in it while `alloc_from_iter` runs, which
    // would be a `RefCell` double borrow if the chunks were held during collection.
    let outer = arena.alloc_from_iter((0..64).map(|n| *arena.alloc(n * 10) + 1));
    assert!(outer.iter().copied().eq((0..64).map(|n| n * 10 + 1)));
}

#[test]
fn a_failing_iterator_allocates_nothing_and_drops_what_it_made() {
    let drops = Cell::new(0);
    let arena = TypedArena::default();
    let failed =
        arena.try_alloc_from_iter((0..10).map(|value| {
            if value == 7 { Err(value) } else { Ok(Counted { value, drops: &drops }) }
        }));
    assert!(matches!(failed, Err(7)));
    assert_eq!(drops.get(), 7, "the seven made before the error are dropped, once each");
    let next = arena.alloc(Counted { value: 99, drops: &drops });
    assert_eq!(next.value, 99);
    drop(arena);
    assert_eq!(drops.get(), 8, "only the one real allocation remained in the arena");
}

#[test]
fn dropless_allocations_are_aligned_and_hold_their_values() {
    let arena = DroplessArena::default();
    let bytes: Vec<&mut u8> = (0..PAST_SEVERAL_CHUNKS).map(|n| arena.alloc(n as u8)).collect();
    let wide: Vec<&mut u128> = (0..PAST_SEVERAL_CHUNKS).map(|n| arena.alloc(n as u128)).collect();
    assert!(bytes.iter().enumerate().all(|(n, byte)| **byte == n as u8));
    assert!(wide.iter().enumerate().all(|(n, word)| **word == n as u128
        && core::ptr::from_ref::<u128>(word).addr() % align_of::<u128>() == 0));
    for align in [1, 2, 8, 16, 64, 4096] {
        let raw = arena.alloc_raw(Layout::from_size_align(24, align).unwrap());
        assert_eq!(raw.addr() % align, 0, "alloc_raw ignored alignment {align}");
    }
}

#[test]
fn dropless_slices_and_strings_are_copies_of_their_input() {
    let arena = DroplessArena::default();
    let source: Vec<u32> = (0..PAST_SEVERAL_CHUNKS as u32).collect();
    let copied = arena.alloc_slice(&source);
    assert_eq!(copied, source.as_slice());
    assert!(!core::ptr::eq(copied.as_ptr(), source.as_ptr()));
    let text = "a string that has to come back byte for byte";
    assert_eq!(arena.alloc_str(text), text);
    let from_iter = arena.alloc_from_iter((0..1000_u64).map(|n| n * n));
    assert!(from_iter.iter().copied().eq((0..1000_u64).map(|n| n * n)));
}
