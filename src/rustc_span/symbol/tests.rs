// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;


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

/// A string interned from a source text keeps its bytes in that text, one held clone per text;
/// a copy made elsewhere or a string outside the text gets the same symbols as `intern`.
#[test]
fn interner_from_source() {
    let i = Interner::prefill(&["fn"], &[]);
    let src = alloc::sync::Arc::new(String::from("fn alpha beta alpha"));
    let alpha = &src[3..8];
    let a = Symbol::new(i.intern_inner(alpha.as_bytes(), Some(&src)));
    assert_eq!(a, Symbol::new(1));
    assert!(core::ptr::eq(i.get_str(a).as_ptr(), alpha.as_ptr()));
    // Predefined: found in the static table, nothing held.
    assert_eq!(Symbol::new(i.intern_inner(src[..2].as_bytes(), Some(&src))), Symbol::new(0));
    // The second `alpha` and a copy of it find the first.
    assert_eq!(Symbol::new(i.intern_inner(src[14..].as_bytes(), Some(&src))), a);
    assert_eq!(i.intern_str("alpha"), a);
    let b = Symbol::new(i.intern_inner(src[9..13].as_bytes(), Some(&src)));
    assert_eq!(i.get_str(b), "beta");
    assert_eq!(i.inner.lock().sources.len(), 1);
    assert_eq!(alloc::sync::Arc::strong_count(&src), 2);
    // Not a slice of `src`: copied, nothing more held.
    let other = String::from("gamma");
    let g = Symbol::new(i.intern_inner(other.as_bytes(), Some(&src)));
    assert!(!core::ptr::eq(i.get_str(g).as_ptr(), other.as_ptr()));
    assert_eq!(i.get_str(g), "gamma");
    assert_eq!(alloc::sync::Arc::strong_count(&src), 2);
    drop(i);
    assert_eq!(alloc::sync::Arc::strong_count(&src), 1);
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


/// A test string that lives as long as the tables that hold it.
fn leaked(s: String) -> &'static [u8] {
    let s: &'static str = s.leak();
    s.as_bytes()
}

#[test]
fn symbol_hash_bits_mark_exactly_non_ascii() {
    // Every length through two whole words and a tail of each size, with one non-ASCII byte
    // at every position (a two-byte char, so both of its bytes are high), and none.
    for len in 0..=24usize {
        let ascii: String = (0..len).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let (hash, bits) = symbol_hash_and_bits(ascii.as_bytes());
        assert_eq!(hash, symbol_hash(ascii.as_bytes()));
        assert_eq!(bits & 0x8080_8080_8080_8080, 0, "{ascii:?}");
        for at in 0..len.saturating_sub(1) {
            let mut s = String::from(&ascii[..at]);
            s.push('\u{e9}');
            s.push_str(&ascii[at + 2..]);
            assert_eq!(s.len(), len);
            let (hash, bits) = symbol_hash_and_bits(s.as_bytes());
            assert_eq!(hash, symbol_hash(s.as_bytes()));
            assert_ne!(bits & 0x8080_8080_8080_8080, 0, "{s:?}");
        }
    }
}

#[test]
fn intern_if_ascii_matches_intern() {
    let i = Interner::prefill(&["fn", "let"], &[]);
    assert_eq!(i.intern_if_ascii(b"fn", None), Some(i.intern_inner(b"fn", None)));
    assert_eq!(i.intern_if_ascii(b"dog", None), Some(i.intern_inner(b"dog", None)));
    assert_eq!(i.intern_if_ascii("caf\u{e9}".as_bytes(), None), None);
    assert_eq!(i.intern_if_ascii(b"", None), Some(i.intern_inner(b"", None)));
}
