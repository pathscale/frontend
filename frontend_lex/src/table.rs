//! A pass's own spelling-to-atom table.
//!
//! Every identifier, lifetime, literal and doc comment spelling a pass meets is looked up here.
//! The first sight of a spelling makes it an atom, numbered in order of first occurrence; every
//! later sight finds that atom. The table is plain memory owned by the pass: no atomics, no
//! lock, no sharing, and nothing reaches the shared interner until the pass resolves its atoms,
//! once each, in order.
//!
//! Open addressing with linear probing, at most half full. An entry keeps the spelling's offset
//! and length in the source, the high half of its hash as a tag (so a probe rarely reads a
//! spelling that does not match), and its atom.

use alloc::boxed::Box;
use alloc::vec;

use frontend_symbol::symbol_hash;

#[derive(Clone, Copy)]
struct Entry {
    /// High half of the hash; 0 marks an empty entry (a real tag of 0 is stored as 1).
    tag: u32,
    lo: u32,
    len: u32,
    atom: u32,
}

const EMPTY: Entry = Entry { tag: 0, lo: 0, len: 0, atom: 0 };

pub(crate) struct Table {
    entries: Box<[Entry]>,
    mask: usize,
    len: usize,
}

/// A lookup's result: found, or the hash to insert with.
pub(crate) enum Probe {
    Hit(u32),
    Miss { hash: u64 },
}

#[inline]
fn tag_of(hash: u64) -> u32 {
    ((hash >> 32) as u32).max(1)
}

impl Table {
    /// Sized for `source_len` bytes of source: about one distinct spelling per 128 bytes before
    /// it grows (sizing it for twice that measured no faster, and is twice the memory).
    pub(crate) fn for_source(source_len: usize) -> Table {
        let slots = (source_len / 64).clamp(256, 1 << 24).next_power_of_two();
        Table { entries: vec![EMPTY; slots].into_boxed_slice(), mask: slots - 1, len: 0 }
    }

    /// Sized for `atoms` distinct spellings, at most half full.
    pub(crate) fn with_capacity(atoms: usize) -> Table {
        let slots = (atoms * 2).max(256).next_power_of_two();
        Table { entries: vec![EMPTY; slots].into_boxed_slice(), mask: slots - 1, len: 0 }
    }

    /// The atom of `src[lo..lo + len]`, if this pass has met that spelling.
    #[inline]
    pub(crate) fn find(&self, src: &[u8], lo: usize, len: usize) -> Probe {
        let bytes = &src[lo..lo + len];
        let hash = symbol_hash(bytes);
        let tag = tag_of(hash);
        let mut slot = slot_of(tag, self.mask);
        loop {
            let entry = &self.entries[slot];
            if entry.tag == 0 {
                return Probe::Miss { hash };
            }
            if entry.tag == tag
                && entry.len as usize == len
                && same(&src[entry.lo as usize..entry.lo as usize + len], bytes)
            {
                return Probe::Hit(entry.atom);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// [`Table::find`] given the spelling's hash, which the caller kept.
    #[inline]
    pub(crate) fn find_hashed(&self, src: &[u8], lo: usize, len: usize, hash: u64) -> Option<u32> {
        let bytes = &src[lo..lo + len];
        let tag = tag_of(hash);
        let mut slot = slot_of(tag, self.mask);
        loop {
            let entry = &self.entries[slot];
            if entry.tag == 0 {
                return None;
            }
            if entry.tag == tag
                && entry.len as usize == len
                && same(&src[entry.lo as usize..entry.lo as usize + len], bytes)
            {
                return Some(entry.atom);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// Records `atom` for `src[lo..lo + len]`, which [`Table::find`] just missed with `hash`.
    #[inline]
    pub(crate) fn insert(&mut self, hash: u64, lo: usize, len: usize, atom: u32) {
        if (self.len + 1) * 2 > self.entries.len() {
            self.grow();
        }
        let tag = tag_of(hash);
        let mut slot = slot_of(tag, self.mask);
        while self.entries[slot].tag != 0 {
            slot = (slot + 1) & self.mask;
        }
        self.entries[slot] = Entry { tag, lo: lo as u32, len: len as u32, atom };
        self.len += 1;
    }

    #[cold]
    fn grow(&mut self) {
        // Slots come from tags alone (`slot_of`), so entries move without their spellings.
        let slots = self.entries.len() * 2;
        let mut fresh = vec![EMPTY; slots].into_boxed_slice();
        let mask = slots - 1;
        for entry in self.entries.iter().filter(|e| e.tag != 0) {
            let mut slot = slot_of(entry.tag, mask);
            while fresh[slot].tag != 0 {
                slot = (slot + 1) & mask;
            }
            fresh[slot] = *entry;
        }
        self.entries = fresh;
        self.mask = mask;
    }
}

/// The slot a tag's probe starts at. Every table operation derives it from the tag alone, so
/// growth (which keeps no full hash) places an entry where a later lookup starts.
#[inline]
fn slot_of(tag: u32, mask: usize) -> usize {
    (u64::from(tag).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32) as usize & mask
}

/// Whether `a == b` for spellings of equal length, inline: identifiers are short, and a call to
/// `memcmp` costs more than comparing them. Up to 16 bytes, two overlapping word loads per side.
#[inline(always)]
fn same(a: &[u8], b: &[u8]) -> bool {
    debug_assert_eq!(a.len(), b.len());
    let len = a.len();
    let word = |s: &[u8], at: usize| u64::from_le_bytes(s[at..at + 8].try_into().unwrap());
    let half = |s: &[u8], at: usize| u32::from_le_bytes(s[at..at + 4].try_into().unwrap());
    match len {
        8..=16 => word(a, 0) == word(b, 0) && word(a, len - 8) == word(b, len - 8),
        4..=7 => half(a, 0) == half(b, 0) && half(a, len - 4) == half(b, len - 4),
        1..=3 => a[0] == b[0] && a[len / 2] == b[len / 2] && a[len - 1] == b[len - 1],
        0 => true,
        _ => a == b,
    }
}
