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

use core::cell::RefCell;
use core::ptr;

use eko::thread::ThreadLocal;

use crate::rustc_data_structures::fingerprint::Fingerprint;
use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_data_structures::stable_hash::{
    self, StableHash, StableHashControls, StableHashCtxt, StableHasher,
};
use tracing::trace;

use crate::rustc_middle::{mir, ty};

impl<'tcx, H, T> StableHash for &'tcx ty::list::RawList<H, T>
where
    T: StableHash,
{
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        // Note: this cache makes an *enormous* performance difference on certain benchmarks. E.g.
        // without it, compiling `diesel-2.2.10` can be 74% slower, and compiling
        // `deeply-nested-multi` can be ~4,000x slower(!)
        //
        // The generation is the key's third component and is not optional. The address is only an
        // identity for as long as the arena that handed it out is alive, and this frontend is
        // a second `GlobalCtxt` in the same process reuses those addresses for different lists. See
        // `stable_hash::bump_address_cache_generation`.
        // `eko::thread::ThreadLocal` is a value with a `const fn new`, not a macro, so
        // the initialiser moves from the declaration down to the `with` call site.
        static CACHE: ThreadLocal<
            RefCell<(u64, FxHashMap<(*const (), StableHashControls), Fingerprint>)>,
        > = ThreadLocal::new();

        let cached = CACHE.with(
            || RefCell::new((stable_hash::address_cache_generation(), Default::default())),
            |cache| {
                let generation = stable_hash::address_cache_generation();
                {
                    let mut cache = cache.borrow_mut();
                    if cache.0 != generation {
                        cache.0 = generation;
                        cache.1.clear();
                    }
                }
                let key = (ptr::from_ref(*self).cast::<()>(), hcx.stable_hash_controls());
                if let Some(&hash) = cache.borrow().1.get(&key) {
                    return hash;
                }

                let mut hasher = StableHasher::new();
                self[..].stable_hash(hcx, &mut hasher);

                let hash: Fingerprint = hasher.finish();
                cache.borrow_mut().1.insert(key, hash);
                hash
            },
        );

        // Behaviour change: `ThreadLocal::with` answers `None` when the key could not be created,
        // which is a process out of thread-local slots - `std::thread_local!` had no such failure
        // mode. This cache is only ever a speed-up, so the fallback is to hash uncached rather
        // than to abort.
        let hash = match cached {
            Some(hash) => hash,
            None => {
                let mut hasher = StableHasher::new();
                self[..].stable_hash(hcx, &mut hasher);
                let hash: Fingerprint = hasher.finish();
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
