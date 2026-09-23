//! This module contains `StableHash` implementations for various data types
//! from `crate::rustc_middle::ty` in no particular order.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::ptr;

use crate::rustc_data_structures::fingerprint::Fingerprint;
use crate::rustc_data_structures::stable_hash::{StableHash, StableHashCtxt, StableHasher};
use tracing::trace;

use crate::rustc_middle::{mir, ty};

impl<'tcx, H, T> StableHash for &'tcx ty::list::RawList<H, T>
where
    T: StableHash,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        // An interned list is hashed once per hashing computation and its fingerprint reused for
        // every other path to it in the same computation. Upstream measured the memo at 74% on
        // `diesel-2.2.10` and ~4,000x on `deeply-nested-multi`: an interned type graph shares
        // sub-lists, and without it every shared node is re-hashed once per path.
        //
        // **The memo belongs to `hcx`, not to the thread.** It was a `thread_local!` keyed on
        // the address plus a process-wide generation counter to retire entries whose arena had
        // died; with sessions sharing nagoya workers that counter could no longer tell a live
        // entry from a dead one, and the map was mutable state every parallel stage touched.
        // See `StableHashCtxt::memoized_address_hash`. The address is identity for exactly as
        // long as the arena lives, which is longer than any one hashing computation.
        let addr = ptr::from_ref(*self).cast::<()>() as usize;
        let hash = match hcx.memoized_address_hash(addr) {
            Some(hash) => hash,
            None => {
                let mut list_hasher = StableHasher::new();
                self[..].stable_hash(hcx, &mut list_hasher);
                let hash: Fingerprint = list_hasher.finish();
                hcx.memoize_address_hash(addr, hash);
                hash
            }
        };

        hash.stable_hash(hcx, hasher);
    }
}

impl<'tcx> StableHash for ty::GenericArg<'tcx> {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.kind().stable_hash(hcx, hasher);
    }
}

// AllocIds get resolved to whatever they point to (to be stable)
impl StableHash for mir::interpret::AllocId {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        ty::tls::with_opt(|tcx| {
            trace!("hashing {:?}", *self);
            let tcx = tcx.expect("can't hash AllocIds during hir lowering");
            tcx.try_get_global_alloc(*self).stable_hash(hcx, hasher);
        });
    }
}

impl StableHash for mir::interpret::CtfeProvenance {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.into_parts().stable_hash(hcx, hasher);
    }
}
