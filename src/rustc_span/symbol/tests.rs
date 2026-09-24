// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;

use super::*;
use crate::rustc_span::create_default_session_globals_then;

#[test]
fn interner_tests() {
    let i = Interner::prefill(&[], &[]);
    // first one is zero:
    assert_eq!(i.intern_str("dog"), Symbol::new(0));
    // re-use gets the same entry, even with a `ByteSymbol`
    assert_eq!(i.intern_byte_str(b"dog"), ByteSymbol::new(0));
    // different string gets a different #:
    assert_eq!(i.intern_byte_str(b"cat"), ByteSymbol::new(1));
    assert_eq!(i.intern_str("cat"), Symbol::new(1));
    // dog is still at zero
    assert_eq!(i.intern_str("dog"), Symbol::new(0));
}

#[test]
fn interner_get() {
    let i = Interner::prefill(&["chicken"], &["cow"]);
    let dog_idx = i.intern_str("dog"); // 2
    let cat_idx = i.intern_str("cat"); // 3
    assert_eq!(i.get_str(Symbol::new(0)), "chicken");
    assert_eq!(i.get_str(Symbol::new(1)), "cow");
    assert_eq!(i.get_str(cat_idx), "cat");
    assert_eq!(i.get_str(dog_idx), "dog");
}

#[test]
fn without_first_quote_test() {
    create_default_session_globals_then(|| {
        let i = Ident::from_str("'break");
        assert_eq!(i.without_first_quote().name, kw::Break);
    });
}

/// Many distinct strings: indices in interning order, stable on re-interning, through several
/// growths of `SymbolIndices`.
#[test]
fn interner_grows_and_keeps_indices() {
    let i = Interner::prefill(&["chicken"], &["cow"]);
    let names: Vec<String> = (0..5000).map(|n| format!("name_{n}")).collect();
    for (n, name) in names.iter().enumerate() {
        assert_eq!(i.intern_str(name), Symbol::new(2 + n as u32));
    }
    for (n, name) in names.iter().enumerate().rev() {
        assert_eq!(i.intern_str(name), Symbol::new(2 + n as u32));
        assert_eq!(i.get_str(Symbol::new(2 + n as u32)), name.as_str());
    }
    assert_eq!(i.intern_str("chicken"), Symbol::new(0));
    assert_eq!(i.intern_str("cow"), Symbol::new(1));
    assert_eq!(i.intern_str(""), Symbol::new(2 + names.len() as u32));
}

/// Intern `s` the way `Interner::intern_inner` does, with `lock` standing in for the interner's
/// lock: a lock-free lookup, then the locked `find_or_insert`.
fn intern_shared(
    strs: &SymbolStrs,
    indices: &SymbolIndices,
    lock: &parking_lot::Mutex<()>,
    s: &'static [u8],
) -> u32 {
    let hash = symbol_hash(s);
    match indices.find(s, hash, strs) {
        Ok(local) => local,
        Err(miss) => {
            let _held = lock.lock();
            // SAFETY: `lock` is held, so this is the one writer.
            unsafe { indices.find_or_insert(s, hash, strs, miss, || s) }
        }
    }
}

/// A lookup's miss goes stale before its insert: the string was inserted by another writer in
/// between, in the same table and then across a growth. The insert must find it, not add it.
#[test]
fn symbol_indices_stale_miss_finds_the_later_insert() {
    let strs = SymbolStrs::new();
    let indices = SymbolIndices::new();
    let lock = parking_lot::Mutex::new(());

    // Same table.
    let a: &'static [u8] = b"alpha";
    intern_shared(&strs, &indices, &lock, b"seed");
    let stale = indices.find(a, symbol_hash(a), &strs).err().expect("absent");
    let first = intern_shared(&strs, &indices, &lock, a);
    // SAFETY: single-threaded test, the one writer.
    let again = unsafe { indices.find_or_insert(a, symbol_hash(a), &strs, stale, || a) };
    assert_eq!(again, first);

    // Across growth: the miss names a table that is no longer current.
    let b: &'static [u8] = b"beta";
    let stale = indices.find(b, symbol_hash(b), &strs).err().expect("absent");
    let first = intern_shared(&strs, &indices, &lock, b);
    for n in 0..4 * SYMBOL_INDICES_FIRST_SLOTS {
        let s: &'static [u8] = leaked(format!("filler_{n}"));
        intern_shared(&strs, &indices, &lock, s);
    }
    // SAFETY: as above.
    let again = unsafe { indices.find_or_insert(b, symbol_hash(b), &strs, stale, || b) };
    assert_eq!(again, first);

    // Nothing was added twice: every local index names a distinct string.
    let len = strs.len.load(Ordering::Acquire);
    assert_eq!(len, 3 + 4 * SYMBOL_INDICES_FIRST_SLOTS);
    for local in 0..len {
        let s = strs.get(local);
        assert_eq!(indices.find(s, symbol_hash(s), &strs).ok(), Some(local as u32));
    }
}

/// Several workers intern overlapping sets of strings in different orders, concurrently, while
/// the table grows under them. Every string gets exactly one index, every worker agrees on it,
/// and the index names that string.
#[test]
fn symbol_indices_concurrent_intern_agrees() {
    const WORKERS: usize = 8;
    const NAMES: usize = 3000;
    let names: Vec<&'static [u8]> =
        (0..NAMES).map(|n| leaked(format!("ident_{}", n % (NAMES / 2)))).collect();
    let distinct = NAMES / 2;

    let strs = SymbolStrs::new();
    let indices = SymbolIndices::new();
    let lock = parking_lot::Mutex::new(());
    let seen: Vec<Vec<AtomicU32>> =
        (0..WORKERS).map(|_| (0..NAMES).map(|_| AtomicU32::new(u32::MAX)).collect()).collect();

    // `eko::thread::scope`, as `vec_cache`'s concurrent test: the test is the one place that
    // needs real concurrency to exercise the lock-free path.
    eko::thread::scope(|s| {
        for w in 0..WORKERS {
            let (strs, indices, lock, names, seen) = (&strs, &indices, &lock, &names, &seen);
            s.spawn(move || {
                // Worker `w` walks the names from a different start, half of them backwards.
                for k in 0..NAMES {
                    let n = if w % 2 == 0 { (k + w * 397) % NAMES } else { NAMES - 1 - k };
                    let local = intern_shared(strs, indices, lock, names[n]);
                    seen[w][n].store(local, Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(strs.len.load(Ordering::Acquire), distinct);
    for n in 0..NAMES {
        let local = seen[0][n].load(Ordering::Relaxed);
        for w in 1..WORKERS {
            assert_eq!(seen[w][n].load(Ordering::Relaxed), local, "worker {w} on name {n}");
        }
        assert_eq!(strs.get(local as usize), names[n]);
        // The duplicate of a name (`n` and `n + distinct`) shares its index.
        assert_eq!(seen[0][n % distinct].load(Ordering::Relaxed), local);
    }
}

/// A test string that lives as long as the tables that hold it.
fn leaked(s: String) -> &'static [u8] {
    let s: &'static str = s.leak();
    s.as_bytes()
}
