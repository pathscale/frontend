//! The resolver frozen, for the error-suggestion searches, and the one stage they run in.
//!
//! # Why the searches can wait
//!
//! When a name does not resolve, the error for it carries suggestions: "a similar name exists"
//! (an edit-distance search over every name in scope), "consider importing" (a walk of every
//! module of the crate for items with that name), "try the variant's enum", "is a trait, not a
//! derive macro". Each unresolved name pays for its own full walk, so a file with hundreds of
//! them does hundreds of walks, one after another, inside the walk that found them. Measured
//! over this crate's own corpus, that search is most of the time `resolve_crate` takes.
//!
//! None of those searches needs anything the walk that found the failure has not already
//! finished writing. They read the module graph (children, visibilities, parents), which
//! import resolution completed; the `macro_rules` chains, which expansion completed; the
//! preludes and the built-in tables. So the walk records each failure as data (the path, the
//! scope, the few names that live only in the walk's ribs) and moves on, and the searches run
//! afterwards, all of them at once, as one stage over this frozen view: each item reads the
//! resolver through `&Resolver` and returns what it found as owned data.
//!
//! # What "frozen" means here, and how it is checked
//!
//! [`Resolver::run_frozen`] runs the stage with the resolver's existing read-only mode switched
//! on, the one import resolution uses for speculative lookups (`SpeculativeFlag`):
//!
//! - every borrow of a module's resolution table (`CmRefCell::borrow_checked`) is then a plain
//!   shared reference, not a `RefCell` borrow, so no item touches a borrow counter another item
//!   is touching;
//! - every write through the resolver's conditionally mutable cells (`CmCell::set_checked`,
//!   `CmRefCell::borrow_mut_checked`, `CmRefCell::take`) and every `cm_mut` asserts that the
//!   mode is off. A search that turned out to write resolver state would stop the build at that
//!   assertion rather than race. That assertion is the verifier of "the search is read-only".
//!
//! What the mode does not cover is state the resolver keeps in plain `Cell`s and `RefCell`s
//! (`CacheCell`, `CacheRefCell`): the lazily built tables of external crates, and the path
//! compression of `macro_rules` scope chains. The first is why the stage exists only when
//! [`Resolver::frozen_view_is_complete`]; the second is done by the caller, serially, before a
//! failure that walks a `macro_rules` chain is recorded (`freeze_macro_rules_chain`).
//!
//! # When the view is not complete
//!
//! External crates are materialised on first lookup: a module of `std` becomes a resolver
//! module the first time a path or a search reaches it, a crate named by `--extern` is loaded
//! the first time the extern prelude is searched. There is no bound on what a search could
//! reach, short of the whole of every dependency, so it cannot be forced up front, and a stage
//! reading it would need a lock around those tables. With no external crate and an empty extern
//! prelude (this crate's default: a session names no sysroot, see `AGENTS.md`) there is nothing
//! to materialise and the view is complete. Otherwise the callers do not defer at all: each
//! failure's search runs where the failure is found, exactly as it did before, so external
//! modules are materialised in the order they always were.

use alloc::vec::Vec;

use crate::rustc_data_structures::sync::run_stage;
use crate::rustc_resolve::{CmResolver, Resolver};

impl<'ra, 'tcx> Resolver<'ra, 'tcx> {
    /// Whether everything an error-suggestion search can read is already built: no external
    /// crate is loaded and the extern prelude is empty, so the module graph, the macro tables and
    /// the preludes are all local and all finished. See the module header.
    ///
    /// Stable for the whole of resolution: a crate is only ever loaded through the extern
    /// prelude, which an empty prelude never reaches.
    pub(crate) fn frozen_view_is_complete(&self) -> bool {
        self.extern_prelude.is_empty() && self.cstore().iter_crate_data().next().is_none()
    }

    /// Run `search` over every query, with the resolver frozen, as one stage, and hand back the
    /// answers in query order.
    ///
    /// The stage runs its items on the calling thread, in order, when the session is serial, and
    /// on the pool when it is parallel (`rustc_data_structures::sync::stage`); the answers are
    /// the same either way, because an item reads only the frozen resolver and its own query.
    /// Nothing is emitted from inside an item: the callers turn the answers into diagnostics
    /// afterwards, in query order, which is the order the walk found the failures in.
    ///
    /// Only for a complete view ([`frozen_view_is_complete`](Self::frozen_view_is_complete));
    /// the callers finish failures in place otherwise.
    pub(crate) fn run_frozen<Q, A>(
        &mut self,
        queries: &[Q],
        search: impl Fn(&Resolver<'ra, 'tcx>, &Q) -> A,
    ) -> Vec<A> {
        debug_assert!(self.frozen_view_is_complete());
        if queries.is_empty() {
            return Vec::new();
        }
        // SAFETY: `SpeculativeFlag::set` asks that no untracked borrow outlive the switch back
        // off, and no tracked borrow be live across the switch on. Untracked borrows are only
        // taken by the items of the stage below, inside it, and `run_stage` returns only once
        // every item has settled and been dropped. A tracked borrow a caller up the stack might
        // hold (a flush can happen in the middle of the late walk) is a shared `Ref` whose
        // count nobody changes while its frame is paused under this call; the items' untracked
        // reads alias it as shared reads, never as writes, because every write asserts the flag.
        unsafe { self.speculative_flag.set(true) };
        let this: &Resolver<'ra, 'tcx> = self;
        let answers = run_stage((this, queries), queries.len(), |input, index| {
            let (r, queries) = *input;
            search(r, &queries[index])
        });
        // SAFETY: as above; the stage has returned, so every untracked borrow is gone.
        unsafe { self.speculative_flag.set(false) };
        answers
    }

    /// Called right before this resolver emits (or stashes) a diagnostic of its own while late
    /// resolution may have errors waiting for their suggestion search: those errors come first
    /// in the output, so their searches run now and the waiting ones that emit in place are
    /// emitted before the new one. A no-op, and one load, when nothing waiting emits in place.
    ///
    /// Every emission site that late resolution can reach calls this; the ones on paths that
    /// only hold a `CmResolver` go through [`CmResolver::before_emit`].
    pub(crate) fn before_emit(&mut self) {
        if self.late_failures.has_waiting_emitters() {
            self.flush_late_failures();
        }
    }
}

impl<'r, 'ra, 'tcx> CmResolver<'r, 'ra, 'tcx> {
    /// [`Resolver::before_emit`], for emission sites inside name lookup, which run on a
    /// conditionally mutable resolver. Late resolution always looks names up through a mutable
    /// one (`cm_mut`), so a failure can only be waiting when this is `Mut`; a read-only lookup
    /// (speculative, or a suggestion search) never finalizes, and never reaches an emission
    /// site with anything waiting.
    pub(crate) fn before_emit(&mut self) {
        if let crate::rustc_resolve::ref_mut::RefOrMut::Mut(r) = self {
            r.before_emit();
        } else {
            debug_assert!(!self.late_failures.has_waiting_emitters());
        }
    }
}
