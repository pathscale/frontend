//! Diagnostics as owned output: collected per par item and per query, forwarded, replayed in
//! serial order.
//!
//! # The problem
//!
//! A caller of this frontend reads diagnostics as a list, in emission order, and that list is
//! the answer: `frontend_facts::check_source` hands it back verbatim. When the compiler's
//! internal stages (`rustc_data_structures::sync::stages`: per body, per module, per check)
//! run their items on several workers, two bodies that each produce an error race to the one
//! shared emitter, and the list comes out in whichever order the threads reached the lock. The
//! same program would give different answers on different runs, and a different answer from a
//! serial run. Neither is acceptable.
//!
//! # The shape: data forward, not a shared sink
//!
//! There is no shared emission buffer and no reorder logic. A par item's diagnostics are part of
//! its *result*, and a query's diagnostics are part of *its* result:
//!
//! 1. The stage runs each item inside [`OrderedReplay::collect`] (through the item hook
//!    `rustc_interface::util::install_parallel_context` registers). While it runs, whatever the
//!    item emits into a `DiagCtxt` that already existed when the item began is *moved* into an
//!    owned [`ItemDiagnostics`] instead of reaching the emitter.
//! 2. The hook hands that value to the stage's replay as the item's diagnostic output.
//! 3. An [`OrderedReplay`] records the items' outputs as they finish, in any order, and when the
//!    stage concludes ([`OrderedReplay::finish`], called in stage order on the thread that
//!    started the stage) forwards them in item order.
//!
//! Forwarding is a move. A diagnostic is built once, carried as data (spans are indices into the
//! frozen source map, not copies of source text), and rendered to text once, by the emitter at
//! the final sink. Nothing is cloned and nothing is rendered twice.
//!
//! **Order is the serial order by construction.** A serial run emits item 0's diagnostics, then
//! item 1's, and so on, each item's in its own program order; forwarding in item order
//! reproduces exactly that. Nested loops compose without a special case: an inner loop's
//! replay forwards into the *enclosing item's* collection, which is itself forwarded in the
//! outer loop's order.
//!
//! # Queries: a query's diagnostics belong to the query, not to whoever ran it
//!
//! Items share the query system. A query is computed once, by whichever item asks for it first
//! *in time*, and every later request is a cache hit that emits nothing. In a serial run "first
//! in time" is "first in serial order", so the query's diagnostics come out at its first serial
//! consumer. In parallel it is whoever won the race: a query first asked for by item `j` may be
//! computed by item `m > j`. Collected into `m`'s output, its diagnostics would come out at `m`'s
//! position, or not at all when `m` comes after an item that ended in a fatal error (the replay
//! drops everything after that item, as a serial run never reaches it). Item `j`'s cache hit
//! would emit nothing, and the error would be lost. That was a real divergence (two `E0223`
//! errors from HIR type lowering, present serially and missing in parallel).
//!
//! So a query that runs inside an item scope gets a stream of its own ([`QueryFrame`]): what it
//! emits goes there, not into the item's collection. When the query finishes the stream is
//! frozen into an `Arc<`[`QueryDiagnostics`]`>`, the diagnostics part of the query's output,
//! stored beside the value (`rustc_middle::query::NodeDiagnostics`, keyed by the value's
//! `DepNodeIndex`). Every *consumption* of the query inside an item scope then appends a
//! reference to that record, `Consumed(record)`, to the consumer's collection: the run that
//! computed it, every cache hit, every thread that waited for it, every thread that found it
//! poisoned. A query that emitted nothing, and consumed nothing that did, has no record and costs
//! its consumers one branch.
//!
//! At replay a `Consumed(record)` emits the record's events in place, recursively, the first time
//! it is reached, and nothing after that: the record's events are moved out on first use
//! ([`QueryDiagnostics`] says why that is the "emitted" mark). Since replay walks everything in
//! serial order, the first `Consumed` it reaches is the first serial consumer's, which is where
//! a serial run printed the query's diagnostics. Dropped items drop only their references; the
//! record is still there for the consumer that counts. Forwarding into an enclosing collection
//! moves the reference, never the events, so nested stages cost nothing extra; the events are
//! moved once, by the outermost replay.
//!
//! # What happens at collection, and what waits for the replay
//!
//! Emission is split in two (see `DiagCtxtInner::emit_diagnostic`):
//!
//! - **Counted at collection**: the error guarantee `emit` hands back, `err_guars` and
//!   `lint_err_guars` behind `has_errors` and `err_count`, taint, `treat_err_as_bug`. These are
//!   what a running item can observe about its own emissions. An item that emits an error and
//!   then asks `has_errors()`, or calls `abort_if_errors()`, or taints its inference context
//!   with the guarantee, must see its own error at once, as it does in a serial run. Counting
//!   at replay would break that inside the item. Replay therefore never counts again, and that
//!   holds for query records too: an error inside a query is counted once, when the query emits
//!   it, however many consumers reference the record.
//! - **Decided at replay**: the duplicate check against `emitted_diagnostics`, `OnceNote` and
//!   `OnceHelp` filtering, the one-shot `recursion_depth_exceeding_limit` silencing, the printed
//!   error and warning counts, `has_printed`, and the emitter itself. These depend on what was
//!   printed *before*, so they are made in serial order, and the copy of a duplicate that
//!   survives is the one a serial run keeps.
//!
//! Delayed bugs and stash operations are forwarded as data too, for the same reason: their
//! final order is otherwise the race. See [`EventKind`].
//!
//! # A fatal error inside an item or a query
//!
//! A serial run that raises `FatalError` inside item `k` prints everything up to that point,
//! then abandons the loop: items after `k` never run. [`OrderedReplay::collect`] catches the
//! fatal error and returns it as data, alongside what the item emitted before raising it.
//! [`OrderedReplay::finish`] forwards items in order up to and including the first one that
//! ended in a fatal error, drops everything after it, and hands the fatal error back for the
//! shim to raise again. The output is the serial output. (Counts are the exception: items after
//! `k` may have run and counted errors. After a fatal error nothing prints a count.)
//!
//! A query that raises is closed on the way out like any other: its partial stream is frozen
//! into a record, referenced from the consumer that ran it, and kept in the query's poisoned
//! state, so a thread that later finds the query poisoned (or wakes from waiting on it) appends
//! the same reference before raising. Whichever of them comes first in serial order prints what
//! the query emitted before it failed.
//!
//! A panic that is not a fatal error (an internal compiler error) is not caught; it unwinds out
//! as before, and what that item collected is lost with it.
//!
//! # Which `DiagCtxt` a scope captures
//!
//! Only one that already existed when the scope began. Every `DiagCtxt`, every item scope and
//! every stage draws a number from one clock ([`tick`]); an item scope captures a context only
//! if the context's number is lower. A context created *inside* an item (a scratch parser, a
//! silent context for a speculative parse) keeps emitting straight to its own emitter, which is
//! what its creator expects to read back.
//!
//! A query frame is stricter: it captures only a context older than the outermost stage of its
//! chain (its *root*), because its record outlives the item that computed it and may be replayed
//! from another stage altogether. An emission a query frame does not capture falls through to
//! the item scope beneath it, as it did before queries had streams. In practice every capture in
//! a query frame is the session's own context: a query reaches a `DiagCtxt` only through its
//! `TyCtxt`, and one it creates for itself is newer than every scope.
//!
//! # Serial mode
//!
//! [`OrderedReplay::collect`] runs the closure directly and collects nothing when the compiler
//! is not in thread-safe mode, and [`QueryFrame::open`] opens nothing, so a serial run is
//! byte-identical to what it was: the emission path finds no scope open on the thread and
//! prints at once, statement for statement as before, and no query ever has a record.

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
    static SCOPES: RefCell<Vec<Scope>> = const { RefCell::new(Vec::new()) };
}

/// Which `DiagCtxt` an event belongs to, so it can be replayed into that context.
///
/// A raw pointer, because the event travels through worker threads and result slots that do
/// not carry the context's lifetime. **Sound because of when it is used.** An item's event is
/// only captured from a context created before the capturing item began, and it is replayed
/// before the par call that ran that item returns (the [`OrderedReplay`] contract); the par call
/// is made by code that borrows the session owning the context, so the context can neither move
/// nor drop in between. A query record's event can be replayed later, from another stage, so a
/// query frame captures only a context older than its chain's outermost stage, which in this
/// compiler is always the session's context (see the module docs, "Which `DiagCtxt` a scope
/// captures"): it lives as long as the `TyCtxt` whose query results hold the record.
#[derive(Copy, Clone)]
pub(super) struct DcxRef(*const DiagCtxt);

// SAFETY: a `DcxRef` is only dereferenced to take the context's own lock (`DiagCtxt::inner`,
// a real mutex in thread-safe mode, which is the only mode in which one is ever made), and the
// lifetime argument is on the type.
unsafe impl Send for DcxRef {}
unsafe impl Sync for DcxRef {}

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
/// # How a stage uses it
///
/// In thread-safe mode only; a serial stage needs none of this. Every item index must report,
/// or everything after it is held back and lost. The stage does this through its item hook
/// (`rustc_interface::util`, the diagnostics hook); the shape of it is:
///
/// ```ignore (sketch of the hook's part in a stage)
/// let replay = OrderedReplay::new(len);            // on the calling thread, before spawning
/// // for item `i`, on whichever worker runs it:
/// let run = replay.collect(|| for_each(item));
/// replay.ready(i, run.diagnostics, run.result.is_err());
/// slots[i] = run.result.ok();                      // the item's own result, if any
/// // after every item has finished, still on the calling thread:
/// if let Err(fatal) = replay.finish() {
///     fatal.raise();                               // leave the par call as a serial run would
/// }
/// ```
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
    /// It was a fresh `tick` per item, which is one `fetch_add` on the one clock every worker
    /// shares, per item. The stage's own time captures exactly the same contexts. An item scope
    /// captures a context older than itself, and the only contexts created between the stage's
    /// start and an item's start are ones the item cannot reach: the stage's function and input
    /// outlive the stage's scope (`sync::stages` is shaped like `std::thread::scope`), so they
    /// cannot borrow anything the scope's body made after it began, and another item's scratch
    /// context is that item's local. Contexts the item makes for itself are newer than either.
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

    /// [`collect`](Self::collect) for a parallel stage's item hook, without asking the mode.
    ///
    /// The hook only ever runs inside a parallel stage, and every thread that runs one of its
    /// items has a parallel session latched (the scope's owner opened a parallel scope, and a
    /// helper latches the session's width before it runs anything), so the answer is always
    /// "thread-safe". Asking is a thread-local read, and this is on the path of every item.
    pub(crate) fn collect_in_stage<R>(&self, f: impl FnOnce() -> R) -> ItemRun<R> {
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
    pub fn finish(self) -> Result<(), FatalError> {
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
                report,
                NOT_REPORTED,
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
