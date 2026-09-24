//! The diagnostics part of a query's output, stored beside its value.
//!
//! A query that runs inside a par item in a parallel session has its diagnostics collected into
//! a record of its own (`rustc_errors::QueryDiagnostics`, see `rustc_errors::item_scope` for the
//! design and why). The record is part of the query's output, like the value: whoever consumes
//! the value consumes the diagnostics too, and replay prints them at the first consumer in
//! serial order. The value lives in the query's cache under its `DepNodeIndex`; the record lives
//! here under the same index, because that index is what every consumer holds, on the execute
//! path and on the cache-hit path alike.
//!
//! Almost no query has a record, and a cache hit is the hottest path in the compiler. So the
//! question "does node `i` have one" is answered by one bit per node, read without a lock, and
//! only a set bit goes to the map. This is not a cache: nothing here is recomputable, and every
//! entry is written once, by the one execution of its query, before the value is published.
//!
//! In a serial session no query has a record and nothing here is touched after the mode check.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them.
use alloc::boxed::Box;
use alloc::sync::Arc;

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::rustc_data_structures::sharded::ShardedHashMap;
use crate::rustc_data_structures::sync::is_dyn_thread_safe;
use crate::rustc_errors::{QueryDiagnostics, consume_query_diagnostics};
use crate::rustc_middle::dep_graph::DepNodeIndex;

/// Bits per page: one page covers this many consecutive `DepNodeIndex`es.
const PAGE_BITS: usize = 1 << 20;
const WORD_BITS: usize = usize::BITS as usize;
/// Words per page. A page is 128 KiB, allocated the first time a node in its range gets a record.
const PAGE_WORDS: usize = PAGE_BITS / WORD_BITS;
/// Pages to cover every `u32` index. Computed in `u64` so a 32-bit target does not overflow.
const PAGES: usize = ((1u64 << 32) / PAGE_BITS as u64) as usize;

/// Query diagnostics records by the `DepNodeIndex` of the value they belong to.
pub struct NodeDiagnostics {
    /// One bit per node: set once its record is in `records`. `PAGES` page pointers, each null
    /// until a node in its range gets a record.
    present: Box<[AtomicPtr<AtomicUsize>]>,
    records: ShardedHashMap<DepNodeIndex, Arc<QueryDiagnostics>>,
}

impl Default for NodeDiagnostics {
    fn default() -> Self {
        NodeDiagnostics {
            present: (0..PAGES).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
            records: ShardedHashMap::default(),
        }
    }
}

impl NodeDiagnostics {
    /// Store the record of the query whose value is about to be published under `index`.
    ///
    /// Call it before the value goes into the cache: a thread that finds the value must find
    /// the bit. The bit is set with `Release` after the map insert, and the cache publishes the
    /// value after that, so a reader that has the value sees both.
    pub fn insert(&self, index: DepNodeIndex, record: Arc<QueryDiagnostics>) {
        self.records.insert_unique(index, record);
        let bit = index.as_usize();
        let word = &self.page_or_new(bit / PAGE_BITS)[(bit % PAGE_BITS) / WORD_BITS];
        word.fetch_or(1 << (bit % WORD_BITS), Ordering::Release);
    }

    /// The value under `index` was consumed without executing its query: a cache hit, or a
    /// wait for another thread's execution that ended with the value. If the query's output has
    /// diagnostics, they are consumed with it (`rustc_errors::consume_query_diagnostics`).
    ///
    /// A mode check and a bit test for the queries that have none, which is nearly all.
    #[inline]
    pub fn consume(&self, index: DepNodeIndex) {
        if !is_dyn_thread_safe() || !self.has(index) {
            return;
        }
        self.consume_record(index);
    }

    #[cold]
    #[inline(never)]
    fn consume_record(&self, index: DepNodeIndex) {
        if let Some(record) = self.records.get(&index) {
            consume_query_diagnostics(record);
        }
    }

    #[inline]
    fn has(&self, index: DepNodeIndex) -> bool {
        let bit = index.as_usize();
        let page = self.present[bit / PAGE_BITS].load(Ordering::Acquire);
        if page.is_null() {
            return false;
        }
        // SAFETY: a non-null page pointer came from `page_or_new`, which allocated `PAGE_WORDS`
        // words and never frees them while `self` lives; the offset is below `PAGE_WORDS`.
        let word = unsafe { &*page.add((bit % PAGE_BITS) / WORD_BITS) };
        word.load(Ordering::Acquire) & (1 << (bit % WORD_BITS)) != 0
    }

    /// Page `page`, allocated zeroed if nobody has yet.
    fn page_or_new(&self, page: usize) -> &[AtomicUsize] {
        let slot = &self.present[page];
        let mut current = slot.load(Ordering::Acquire);
        if current.is_null() {
            let fresh: Box<[AtomicUsize]> = (0..PAGE_WORDS).map(|_| AtomicUsize::new(0)).collect();
            let fresh = Box::into_raw(fresh).cast::<AtomicUsize>();
            match slot.compare_exchange(ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => current = fresh,
                Err(winner) => {
                    // SAFETY: `fresh` came from `Box::into_raw` of a `PAGE_WORDS` slice just
                    // above and was never shared: the exchange failed.
                    drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(fresh, PAGE_WORDS)) });
                    current = winner;
                }
            }
        }
        // SAFETY: as in `has`: `PAGE_WORDS` words, alive as long as `self`.
        unsafe { &*ptr::slice_from_raw_parts(current, PAGE_WORDS) }
    }
}

impl Drop for NodeDiagnostics {
    fn drop(&mut self) {
        for slot in self.present.iter_mut() {
            let page = *slot.get_mut();
            if !page.is_null() {
                // SAFETY: allocated by `page_or_new` as a `PAGE_WORDS` slice, and `&mut self`
                // means nobody reads it any more.
                drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(page, PAGE_WORDS)) });
            }
        }
    }
}
