//! Legacy diagnostic replay and query records.
//!
//! Compiler walks now run serially on the caller, including in parallel sessions, and emit in
//! their existing order. They open no parallel replay or borrowed worker context. The public
//! replay API remains for compatibility; its context-bearing events stay on the caller.
//! Owned file parsing creates its diagnostic context inside the task and returns DiagInner
//! records with its AST. Owned body parsing retains the established clean-transaction retry.
//! Neither path transports a DcxRef, QueryFrame or OrderedReplay to nagoya.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them.
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::{self, Vec};

use core::cell::RefCell;
use core::marker::PhantomData;
use core::mem;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use eko::thread_local;
use parking_lot::Mutex;

use crate::rustc_data_structures::fx::{FxIndexMap, FxIndexSet};
use crate::rustc_data_structures::sync::is_dyn_thread_safe;
use crate::rustc_errors::{DelayedDiagInner, DiagCtxt, DiagInner, ErrorGuaranteed, StashKey};
use crate::rustc_span::Span;
use crate::rustc_span::fatal_error::{FatalError, catch_fatal_errors};

/// One clock for `DiagCtxt` creation, item scope opening and stage starts. Only its order
/// matters.
///
/// **Ordering data, and process-wide because what it orders is.** The question it answers is
/// "did this `DiagCtxt` exist before that stage started", asked on whichever thread emits, about
/// a context and a stage that may each have been made on any thread. Nothing the stage could
/// carry answers it: a stage cannot list the contexts older than itself, and a context cannot
/// name the stages younger than itself. One counter both sides draw from is the least that does.
/// It is not a knob and not a cache; nothing reads it but the comparisons below.
static CLOCK: AtomicU64 = AtomicU64::new(1);

/// The next value of [`CLOCK`].
pub(super) fn tick() -> u64 {
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

/// Where one item's or one query's diagnostics accumulate while it runs.
///
/// **Arc-owned, with its lock inside.** The owning thread appends to it, and an inner
/// loop's [`OrderedReplay`] appends to it too, from whichever thread concludes the inner stage.
/// So it is shared across workers by ownership, not borrowed from the item's stack.
type Collection = Arc<Mutex<Vec<Event>>>;

#[derive(Copy, Clone, PartialEq, Eq)]
enum ScopeKind {
    /// A par item, opened by [`OrderedReplay::collect`].
    Item,
    /// A query executing inside an item, opened by [`QueryFrame::open`].
    Query,
}

/// A scope open on this thread: an item, or a query running inside one.
struct Scope {
    kind: ScopeKind,
    /// Contexts with a lower clock value are captured. An item's opening time; for a query
    /// frame, its root.
    opened: u64,
    /// The start of the outermost stage in this scope's chain of enclosing items and queries.
    /// What a query frame opened on top of this scope captures (see the module docs).
    root: u64,
    /// `None` until something is pushed: a query frame allocates nothing unless its query
    /// actually emits or consumes a query that did, and an item scope nothing unless its item
    /// does, or starts a stage (whose replay targets this scope). It was created up front for an
    /// item, an `Arc` and a lock per item, and nearly every item emits nothing.
    collection: Option<Collection>,
}

impl Scope {
    fn captures(&self, created: u64) -> bool {
        created < self.opened
    }

    /// This scope's collection, created on first use.
    fn collection(&mut self) -> Collection {
        Arc::clone(self.collection.get_or_insert_with(Collection::default))
    }
}

thread_local! {
    /// The scopes open on this thread, innermost last. A worker that picks up another item
    /// while its own is waiting opens that item's scope on top, and closes it again when the
    /// item returns, so the innermost scope is always what this thread is running: the query
    /// it is executing, or else the item.
    ///
    /// **Thread state, because what it records is.** An emission happens deep inside whatever
    /// the item calls (`DiagCtxtInner::emit_diagnostic`, reached through a `DiagCtxt` the item
    /// did not make), and the only thing that knows which item or query is running there is
    /// the thread running it, exactly as with the `ImplicitCtxt`. Carrying it as a value would
    /// mean threading a collection handle through every call that can emit.
    static SCOPES: RefCell<Vec<Scope>> = const { RefCell::new(Vec::new()) };
}

/// Which `DiagCtxt` an event belongs to, so it can be replayed into that context.
///
/// A caller-local pointer retained by the compatibility replay. Its owner must finish replay
/// while the diagnostic context remains alive. This type is neither Send nor Sync and never
/// appears in the input or output of an owned task; parse tasks return DiagInner records.
#[derive(Copy, Clone)]
pub(super) struct DcxRef(*const DiagCtxt);

impl DcxRef {
    pub(super) fn of(dcx: &DiagCtxt) -> Self {
        DcxRef(dcx as *const DiagCtxt)
    }
}

/// One thing an item did to a `DiagCtxt` that must happen in serial order.
pub(super) enum EventKind {
    /// A diagnostic that passed every check made at collection, waiting to be printed.
    Print(DiagInner),
    /// A delayed bug. Its order in `delayed_bugs` is the order `flush_delayed` reports it in.
    /// Recorded at replay only if no error has been counted by then, as `emit_diagnostic` would
    /// have done; with an error counted, `flush_delayed` never reports anything anyway.
    DelayedBug(DelayedDiagInner, ErrorGuaranteed),
    /// A stash or a steal. The operation itself happened live, because the item needs its
    /// effect at once (a steal returns the stolen diagnostic). Only its *order* is forwarded,
    /// because `emit_stashed_diagnostics` walks the stash in an order the operations decide.
    Stash(StashOp),
}

/// One entry in an item's or a query's diagnostic output.
#[expect(
    clippy::large_enum_variant,
    reason = "diagnostic events keep owned payloads inline to avoid one heap allocation per event"
)]
pub(super) enum Event {
    /// Something done to a context, and which context.
    Dcx {
        dcx: DcxRef,
        /// The context's clock value, so a replay into an enclosing scope can decide whether
        /// that scope captures this context too.
        created: u64,
        kind: EventKind,
    },
    /// The running code consumed a query whose output has diagnostics: they come out here if
    /// nothing earlier in serial order has printed them. A reference, never a copy.
    Consumed(Arc<QueryDiagnostics>),
}

impl Event {
    /// Apply the event, as if it were happening now outside every item.
    fn apply(self) {
        match self {
            Event::Dcx { dcx, kind, .. } => apply_to(dcx, kind),
            Event::Consumed(record) => record.emit(),
        }
    }
}

fn apply_to(dcx: DcxRef, kind: EventKind) {
    // SAFETY: see `DcxRef`.
    let dcx = unsafe { &*dcx.0 };
    dcx.inner.borrow_mut().replay(kind);
}

/// Where the current emission should go, if a scope on this thread captures it.
pub(super) struct Capture {
    dcx: DcxRef,
    created: u64,
    collection: Collection,
}

impl Capture {
    pub(super) fn push(self, kind: EventKind) {
        self.collection.lock().push(Event::Dcx { dcx: self.dcx, created: self.created, kind });
    }
}

/// The capture for an emission into `dcx` (created at clock value `created`) on this thread,
/// or `None` if it should take effect at once. Always `None` in a serial run, where no scope
/// is ever opened.
///
/// The innermost scope that captures the context takes it, looking through query frames down
/// to the item they run in and no further: below that item is whatever this thread was doing
/// before it picked the item up, which is not where the item's emissions belong.
pub(super) fn capture(dcx: DcxRef, created: u64) -> Option<Capture> {
    SCOPES.with(|scopes| {
        let mut scopes = scopes.borrow_mut();
        for scope in scopes.iter_mut().rev() {
            if scope.captures(created) {
                return Some(Capture { dcx, created, collection: scope.collection() });
            }
            if scope.kind == ScopeKind::Item {
                return None;
            }
        }
        None
    })
}

/// What one par item emitted, owned, in its program order.
///
/// Move it into the item's result slot and hand it to an [`OrderedReplay`]. Dropping it
/// unreplayed loses the item's diagnostics; the replay is the only way out.
#[must_use = "an item's diagnostics are lost unless they are replayed"]
#[derive(Default)]
pub struct ItemDiagnostics {
    events: Vec<Event>,
}

impl ItemDiagnostics {
    /// True if the item emitted nothing that needs replaying.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// What [`OrderedReplay::collect`] returns: the item's result, or the fatal error that ended
/// it, and what it emitted either way.
pub struct ItemRun<R> {
    pub result: Result<R, FatalError>,
    pub diagnostics: ItemDiagnostics,
}

// ---- queries -------------------------------------------------------------------------------

/// The diagnostics part of one query's output: what the query emitted while it ran inside an
/// item scope, including `Consumed` references to the queries it used, frozen when it finished
/// (or failed).
///
/// Shared by `Arc` between the query's stored output and every consumer's collection. It is
/// emitted once, at the first `Consumed` reference replay reaches, and that emission *moves* the
/// events out: **an empty record is an emitted one**, and every later reference emits nothing,
/// which is what a cache hit does in a serial run. A record is only ever created non-empty.
///
/// The "emitted" mark is per session state kept in the record itself. The lock around the
/// events makes "emitted once" hold whatever thread gets there; in practice only one thread
/// emits at a time anyway, because a record is only emitted by the outermost replay (on the
/// thread that concludes a top-level stage, in stage order) or by a consumer outside every item
/// (the session's own thread). Nested replays forward the reference and never emit.
pub struct QueryDiagnostics {
    events: Mutex<Vec<Event>>,
}

impl QueryDiagnostics {
    /// Take the events, leaving the record emitted.
    fn take(&self) -> Vec<Event> {
        mem::take(&mut *self.events.lock())
    }

    /// Whether the record has been emitted already, so a new reference to it would emit nothing.
    fn is_spent(&self) -> bool {
        self.events.lock().is_empty()
    }

    /// Emit the record's events now, in order, and the events of every record it references that
    /// has not been emitted yet, in place. Iterative, because query nesting can be deep.
    fn emit(&self) {
        let mut pending: Vec<vec::IntoIter<Event>> = Vec::new();
        pending.push(self.take().into_iter());
        while let Some(events) = pending.last_mut() {
            match events.next() {
                None => {
                    pending.pop();
                }
                Some(Event::Consumed(record)) => pending.push(record.take().into_iter()),
                Some(Event::Dcx { dcx, kind, .. }) => apply_to(dcx, kind),
            }
        }
    }
}

/// The diagnostics stream of one query executing inside an item scope.
///
/// Open it right before the query's provider runs and close it right after, on the same thread,
/// with the value (or on the way out of an unwind). While it is open, what the query emits goes
/// into its own stream instead of the item's. `rustc_query_impl::execution` keeps it in the
/// job's guard, so a query that raises is closed by the guard's drop.
#[must_use = "a query frame must be closed, or its query's diagnostics are lost"]
pub struct QueryFrame {
    /// Tied to the thread whose scope stack it sits on.
    _thread: PhantomData<*const ()>,
}

impl QueryFrame {
    /// Open a stream for the query this thread is about to execute, if it runs inside an item
    /// scope. `None` in a serial run and outside every item, where emissions go straight out as
    /// they always did.
    ///
    /// Costs a push onto the thread's scope stack and nothing else: the stream is allocated
    /// only when something is put in it.
    #[inline]
    pub fn open() -> Option<QueryFrame> {
        if !is_dyn_thread_safe() {
            return None;
        }
        SCOPES.with(|scopes| {
            let mut scopes = scopes.borrow_mut();
            let root = scopes.last()?.root;
            scopes.push(Scope { kind: ScopeKind::Query, opened: root, root, collection: None });
            Some(QueryFrame { _thread: PhantomData })
        })
    }

    /// The query is done, with a value or not. Freezes what it emitted into its record, puts a
    /// reference to the record into the collection of whatever ran the query (the enclosing
    /// query's stream, or the item's), and hands the record back for the query's stored output.
    /// `None` if the query emitted nothing and consumed nothing that did.
    pub fn close(self) -> Option<Arc<QueryDiagnostics>> {
        mem::forget(self);
        close_frame()
    }
}

impl Drop for QueryFrame {
    /// Not the normal path: the query system closes its frames explicitly. A frame dropped
    /// unclosed still closes, so the stack stays balanced and its consumer keeps the reference.
    fn drop(&mut self) {
        drop(close_frame());
    }
}

#[expect(
    clippy::arc_with_non_send_sync,
    reason = "query diagnostic records are forwarded only through caller-thread scope frames"
)]
fn close_frame() -> Option<Arc<QueryDiagnostics>> {
    SCOPES.with(|scopes| {
        let mut scopes = scopes.borrow_mut();
        let frame = scopes.pop();
        debug_assert!(
            frame.as_ref().is_some_and(|frame| frame.kind == ScopeKind::Query),
            "a query frame closed over a scope it did not open"
        );
        // Moved out. An inner `OrderedReplay` that forwarded into this frame has finished by
        // now (its par call returned inside the query), so nobody else holds the collection.
        let events = mem::take(&mut *frame?.collection?.lock());
        if events.is_empty() {
            return None;
        }
        let record = Arc::new(QueryDiagnostics { events: Mutex::new(events) });
        // The query's first consumer is whoever ran it. A frame is only opened on top of
        // another scope, so there is one.
        if let Some(below) = scopes.last_mut() {
            below.collection().lock().push(Event::Consumed(Arc::clone(&record)));
        }
        Some(record)
    })
}

/// This thread consumed a query whose stored output has diagnostics `record`: a cache hit, a
/// wait that ended with the value, or a wait or lookup that found the query poisoned.
///
/// Inside a scope, a reference goes into the current collection and the record is emitted at
/// replay, if nothing earlier in serial order has emitted it. Outside every scope (the
/// session's own thread, between or around stages) the record is emitted now, as the query's
/// diagnostics would have been had this been the first thing to run it. A record already
/// emitted is skipped: nothing can un-emit it, so the reference would only ever emit nothing.
pub fn consume_query_diagnostics(record: Arc<QueryDiagnostics>) {
    if record.is_spent() {
        return;
    }
    let collection = SCOPES.with(|scopes| scopes.borrow_mut().last_mut().map(Scope::collection));
    match collection {
        Some(collection) => collection.lock().push(Event::Consumed(record)),
        None => record.emit(),
    }
}

// ---- items ---------------------------------------------------------------------------------

/// A collection a replay forwards into, and what it captures.
struct Target {
    opened: u64,
    collection: Collection,
}

/// Records items' diagnostics as they finish and forwards them in item order when the stage
/// concludes.
///
/// Create it on the thread that makes the par call, before the items start, with the number of
/// items: it captures that thread's innermost scopes, if there are any, as where to forward. Then
/// run each item through [`collect`](Self::collect) and report it with [`ready`](Self::ready),
/// once per item, from any thread, in any order, and call [`finish`](Self::finish) after the last
/// one, before the par call returns.
///
/// Forwarding goes to the enclosing scope's collection when the par call is itself inside an
/// item (or inside a query inside one) and that scope captures the context, and straight to
/// the context otherwise. Either way it is a move of the item's events. A `Consumed` reference
/// always goes to the innermost enclosing scope when there is one, since it stands for whatever
/// the par call's caller is doing, and is emitted only by the outermost replay.
///
/// # Compatibility use
///
/// Every item index must report before finish. This replay opens caller-local collection frames
/// and forwards diagnostics in input order; the owned task executor does not use it.
///
/// # What an item costs it
///
/// Most items emit nothing. Such an item allocates nothing here (its scope's collection is
/// created on first use, as a query frame's is), reads no clock (its scope opens at the stage's
/// time, see `opened`), and reports with one store into its own entry of `reported`, which no
/// other item writes. Only an item that emitted something takes the shared `emitted` lock.
pub struct OrderedReplay {
    /// The scopes enclosing the par call, innermost first: any query frames, then the item
    /// they run in. Empty at the top level.
    enclosing: Vec<Target>,
    /// The start of the outermost stage of this chain; this stage's own start at the top level.
    /// Every item scope this replay opens carries it, for the query frames opened inside.
    root: u64,
    /// This stage's start on the clock: the `opened` of every item scope it opens.
    ///
    /// Compatibility collection frames share this creation time. Contexts created locally
    /// inside a frame continue to emit directly into their own diagnostic sinks.
    opened: u64,
    /// Per item: whether it has reported, and whether it ended in a fatal error. Sized once, when
    /// the stage starts, and written once per item, by the thread that ran it.
    reported: Box<[AtomicU8]>,
    /// What the items that emitted anything emitted, by index, in the order they finished. Sorted
    /// once, by `finish`.
    emitted: Mutex<Vec<(usize, ItemDiagnostics)>>,
}

/// `OrderedReplay::reported`: the item has not reported.
const NOT_REPORTED: u8 = 0;
/// It reported, and ended normally.
const REPORTED: u8 = 1;
/// It reported, and ended in a fatal error.
const REPORTED_FATAL: u8 = 2;

impl OrderedReplay {
    pub fn new(len: usize) -> Self {
        let (enclosing, root) = SCOPES.with(|scopes| {
            let mut scopes = scopes.borrow_mut();
            let root = scopes.last().map(|scope| scope.root);
            let mut enclosing = Vec::new();
            for scope in scopes.iter_mut().rev() {
                enclosing.push(Target { opened: scope.opened, collection: scope.collection() });
                if scope.kind == ScopeKind::Item {
                    break;
                }
            }
            (enclosing, root)
        });
        let opened = tick();
        OrderedReplay {
            enclosing,
            root: root.unwrap_or(opened),
            opened,
            reported: (0..len).map(|_| AtomicU8::new(NOT_REPORTED)).collect(),
            emitted: Mutex::new(Vec::new()),
        }
    }

    /// Run one par item with its diagnostics collected as owned data.
    ///
    /// Call it on the thread that runs the item, around exactly the item's work. Nesting is
    /// fine: an inner loop's items open their own scopes on top. In serial mode it just calls
    /// `f`.
    ///
    /// A `FatalError` raised inside `f` is caught and returned as `Err`, with what the item
    /// emitted before raising it; see the module docs for why and for what the caller must do
    /// with it. Any other panic unwinds through, closing the scope on the way.
    pub fn collect<R>(&self, f: impl FnOnce() -> R) -> ItemRun<R> {
        // Serial mode: nothing races, so nothing is collected and a fatal error unwinds exactly
        // as it always did. Stages only run items after the mode is chosen, so this cannot be
        // the uninitialised-mode panic.
        if !is_dyn_thread_safe() {
            return ItemRun { result: Ok(f()), diagnostics: ItemDiagnostics::default() };
        }
        self.collect_in_stage(f)
    }

    /// Open a caller-local collection frame for the compatibility collect API.
    fn collect_in_stage<R>(&self, f: impl FnOnce() -> R) -> ItemRun<R> {
        // No collection yet: one is made when the item first emits or consumes something with
        // diagnostics, or when a stage started inside it captures this scope as its target.
        let scope =
            Scope { kind: ScopeKind::Item, opened: self.opened, root: self.root, collection: None };
        SCOPES.with(|scopes| scopes.borrow_mut().push(scope));

        /// Closes the scope if `f` unwinds, a foreign panic included. The normal path closes it
        /// itself, because it needs what the scope collected.
        struct CloseOnUnwind;
        impl Drop for CloseOnUnwind {
            fn drop(&mut self) {
                SCOPES.with(|scopes| {
                    scopes.borrow_mut().pop();
                });
            }
        }
        let close = CloseOnUnwind;
        let result = catch_fatal_errors(f);
        mem::forget(close);
        let scope = SCOPES.with(|scopes| scopes.borrow_mut().pop());
        debug_assert!(
            scope.as_ref().is_some_and(|scope| scope.kind == ScopeKind::Item),
            "an item scope closed over a scope it did not open"
        );

        // Moved out, not copied. An inner `OrderedReplay` that targeted this scope has finished
        // by now (its par call returned inside `f`), so the lock is uncontended.
        let events = match scope.and_then(|scope| scope.collection) {
            Some(collection) => mem::take(&mut *collection.lock()),
            None => Vec::new(),
        };
        ItemRun { result, diagnostics: ItemDiagnostics { events } }
    }

    /// Item `index` is done. `fatal` is `run.result.is_err()` from its [`ItemRun`].
    ///
    /// Only records. Nothing is forwarded until [`finish`](Self::finish): a stage cannot know
    /// here whether an earlier stage of the same scope will stop on a fatal error, and if one
    /// does a serial run never reaches this stage at all, so anything forwarded early would be
    /// output the serial run does not have. `finish` is called in stage order, and only for
    /// stages a serial run reaches, which makes the order right by construction.
    pub fn ready(&self, index: usize, diagnostics: ItemDiagnostics, fatal: bool) {
        if !diagnostics.is_empty() {
            self.emitted.lock().push((index, diagnostics));
        }
        // `Release`, after the push: `finish` reads the flag, then the list. It runs after every
        // item has settled, which already orders it after this, so the pairing is belt and
        // braces rather than the proof.
        let report = if fatal { REPORTED_FATAL } else { REPORTED };
        self.reported[index].store(report, Ordering::Release);
    }

    /// Every item has reported. Forwards each item's diagnostics in index order, up to and
    /// including the first that ended in a fatal error, and drops the rest, which a serial run
    /// never reaches. Returns that fatal error to raise again; the stage raises it after every
    /// item has settled, so the unwind leaves the stage as it would have in a serial run.
    ///
    /// By reference, because the stage that owns the replay is shared with the threads that ran
    /// its items; call it once. A replay dropped without it is discarded: what its items emitted
    /// is dropped with it, which is the unwind path (an internal compiler error, or a fatal error
    /// in an earlier stage of the scope).
    pub fn finish(&self) -> Result<(), FatalError> {
        // A serial run never reaches the items after the first fatal one, so their diagnostics
        // are dropped with the rest of `emitted` when this returns early.
        let mut emitted = mem::take(&mut *self.emitted.lock());
        // Each index reports once, so the keys are distinct and an unstable sort is exact.
        emitted.sort_unstable_by_key(|(index, _)| *index);
        let mut emitted = emitted.into_iter().peekable();
        for (index, report) in self.reported.iter().enumerate() {
            let report = report.load(Ordering::Acquire);
            // An item that never reported holds back everything after it. After a fatal error
            // that is expected (a stage skips the items a serial run would never reach), so only
            // a gap before any stop is a bug.
            debug_assert_ne!(
                report, NOT_REPORTED,
                "item {index} never reported to its OrderedReplay"
            );
            if report == NOT_REPORTED {
                break;
            }
            if let Some((_, diagnostics)) = emitted.next_if(|(emitter, _)| *emitter == index) {
                self.forward(diagnostics);
            }
            if report == REPORTED_FATAL {
                return Err(FatalError);
            }
        }
        Ok(())
    }

    fn forward(&self, diagnostics: ItemDiagnostics) {
        for event in diagnostics.events {
            let target = match &event {
                // Stays a reference: resolved by the outermost replay.
                Event::Consumed(_) => self.enclosing.first(),
                Event::Dcx { created, .. } => {
                    self.enclosing.iter().find(|target| *created < target.opened)
                }
            };
            match target {
                Some(target) => target.collection.lock().push(event),
                None => event.apply(),
            }
        }
    }
}

/// One change to the stash, recorded so the serial order of the stash can be rebuilt.
pub(super) enum StashOp {
    Insert(StashKey, Span),
    Remove(StashKey, Span),
}

/// The record needed to emit stashed diagnostics in the order a serial run would.
///
/// `emit_stashed_diagnostics` walks the stash in `IndexMap` order: stash keys in order of their
/// first insertion, spans in insertion order under each, with `swap_remove` on every steal
/// moving the last span into the stolen one's slot. That order is a function of the sequence of
/// stash and steal operations, and when items run in parallel that sequence is the race. So
/// the operations are logged in serial order (items' operations arrive through the replay) and
/// replayed onto the shape the stash had when the log began. Only the *order* comes from the
/// log; which entries exist, and their contents, come from the live stash.
pub(super) struct StashReplay {
    /// The stash's shape when the first item-captured operation happened. Everything in it was
    /// put there before any item ran in parallel, so its order is already the serial one.
    base: FxIndexMap<StashKey, FxIndexSet<Span>>,
    /// Every operation since, in serial order.
    log: Vec<StashOp>,
}

impl StashReplay {
    /// Start a log from the stash's current shape. Called before the operation that starts it
    /// is applied, so the base does not include it.
    pub(super) fn begin<V>(stash: &FxIndexMap<StashKey, FxIndexMap<Span, V>>) -> Self {
        let base = stash
            .iter()
            .map(|(key, spans)| (*key, spans.keys().copied().collect::<FxIndexSet<Span>>()))
            .collect();
        StashReplay { base, log: Vec::new() }
    }

    pub(super) fn record(&mut self, op: StashOp) {
        self.log.push(op);
    }

    /// The live stash's entries, in the order a serial run would have left them.
    ///
    /// An entry the log never placed is not emitted: it was stashed by an item after one that
    /// ended in a fatal error, which a serial run never ran. An entry the log placed but the
    /// live stash no longer holds is skipped, because whoever stole it emitted its replacement.
    ///
    /// **Not fixed here: a steal that races its stash.** When stealer and stasher are different
    /// items, whether the steal finds anything depends on which ran first, and the stealing code
    /// then does different things; reordering output cannot undo a different decision. The race
    /// does not arise in this compiler today: stashes are made by the parser and name
    /// resolution, before any par loop, and each is stolen during type checking by the one body
    /// containing its span; the one stash made during type checking, `MaybeFruTypo`, is stolen
    /// by the body that stashed it.
    pub(super) fn in_serial_order<V>(
        self,
        mut stash: FxIndexMap<StashKey, FxIndexMap<Span, V>>,
    ) -> Vec<V> {
        let mut shape = self.base;
        for op in self.log {
            match op {
                StashOp::Insert(key, span) => {
                    shape.entry(key).or_default().insert(span);
                }
                StashOp::Remove(key, span) => {
                    if let Some(spans) = shape.get_mut(&key) {
                        spans.swap_remove(&span);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (key, spans) in shape {
            for span in spans {
                if let Some(entry) = stash.get_mut(&key).and_then(|live| live.swap_remove(&span)) {
                    out.push(entry);
                }
            }
        }
        out
    }
}
