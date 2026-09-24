//! Stages: a function over frozen input, fanned out per index, each item's output written once
//! into its own slot and awaitable on its own. The compiler's one parallel primitive.
//!
//! # The shape
//!
//! ```ignore (illustrative)
//! // One stage, all of it: every item's output, moved out in input order.
//! let facts: Vec<Fact> = sync::run_stage(owners, owners.len(), |owners, i| fact(tcx, owners[i]));
//!
//! // Stages that feed each other, in a scope.
//! let checked = sync::stages(|scope| {
//!     let lowered = scope.stage(files, files.len(), |files, i| lower(&files[i]));
//!     // Item `i` of the second stage waits for `lowered[i]` only, not for the whole first stage.
//!     let input = lowered.clone();
//!     scope.stage(input, lowered.len(), |lowered, i| lowered.wait(i).map(check))
//! });
//! ```
//!
//! - **Zero copy.** A stage's input is one frozen value the stage owns (an arena slice
//!   `&'tcx [T]`, an `Arc<[T]>`, a previous stage's `Arc<Slots<_>>`) and each item is told its
//!   index and reads in place. Outputs are moved into preallocated slots, once, and read back by
//!   reference or moved out; nothing on the path clones an input or an output.
//! - **Data forward.** A stage's [`Slots`] is frozen once filled and is the natural input of the
//!   next stage. An item's diagnostics are part of its output too (see "Diagnostics" below).
//! - **Reactive.** Starting a stage returns at once. Its items run on the pool while the scope
//!   goes on, and a later stage's item waits for exactly the slots it reads, through
//!   [`Slots::wait`], which also *runs* the item it waits for when nobody has started it yet.
//!   There is no barrier between stages; there is one at the end of the scope, because that is
//!   where the borrows the items rely on end.
//!
//! **Serial when parallelism is off.** In a serial session, and in a build without the `parallel`
//! feature, a stage runs all of its items on the calling thread, in index order, at the moment it
//! is started, and a panic or fatal error in one propagates at once. That is exactly the loop the
//! serial compiler ran, and the rest of the API behaves the same over it: every slot is settled
//! before a later stage asks.
//!
//! # Borrowing, and why there is a scope
//!
//! Items borrow: their functions capture `TyCtxt<'tcx>`, their inputs are arena slices, and they
//! read the compiler's thread-locals (`SessionGlobals`, the `ImplicitCtxt`), which live in the
//! stack frames of the session. [`stages`] is shaped like `std::thread::scope`: the body gets a
//! `&'scope StageScope<'scope, 'env>` for a `'scope` it cannot name a local for, a stage's input,
//! function and output must outlive `'scope`, and [`stages`] does not return, or unwind, until
//! every item of every stage has settled and every pool helper has let go of the scope. So
//! everything an item borrows outlives every use, and a call site needs no `unsafe`.
//!
//! **What an item runs in is typed and borrowed for `'scope` too.** A parallel scope opens inside
//! `rustc_middle::ty::tls::ItemContext::capture`, which hands it the caller's `SessionGlobals`
//! and `ImplicitCtxt` as references, and every stage keeps a copy (an `ItemContext<'scope>`) and
//! installs it around its items with the real functions (`set_session_globals_then`,
//! `tls::enter_context`). Each stage owns its diagnostics replay (`rustc_errors::OrderedReplay`)
//! and runs every item through it. This is the one module in `rustc_data_structures` that names
//! the compiler above it, and it names exactly those two types: a stage is the compiler's
//! execution primitive, what it installs around an item is the compiler's context, and these are
//! modules of one crate. It used to reach them through a registry of function pointers over
//! `*const ()` that `rustc_interface` filled at run time, which is the shape upstream needed only
//! because its crates could not name each other.
//!
//! **One lifetime erasure, in one function.** nagoya's pool takes `'static` work, and an item's
//! function borrows `TyCtxt`, so a stage cannot be handed to a helper as it is.
//! `parallel::detach` erases the stage's lifetime once, as a weak reference, and every `'static`
//! handle to the stage (the scope's list, a helper's batch, the runner in its `Slots`) is that
//! weak reference or an upgrade of it; the scope drops every upgrade before it ends, after which
//! the weak ones can never upgrade again. Its `SAFETY` comment has the proof. What would remove
//! it: `'static` inputs and functions, which means `TyCtxt` reachable through an `Arc`'d
//! `GlobalCtxt` and `SessionGlobals` owned by an `Arc` rather than a stack frame. The helpers
//! themselves need no erasure: what they are handed, the scope's `ScopeShared`, owns everything
//! in it.
//!
//! **`Send` and `Sync` are not checked.** The bounds are rustc's `DynSend` and `DynSync`, which
//! this crate implements for every type (`marker.rs`), so a stage's items cross threads on the
//! call site's word. The compiler's call sites are the ones upstream ran on a thread pool; a new
//! one has to be read.
//!
//! # How items get run
//!
//! Each slot is claimed once, by compare-and-swap, by whichever thread gets to it first:
//!
//! - *helpers*, closures submitted to nagoya's pool when a stage starts (how many: "Waking
//!   helpers" below), each taking unclaimed items from every open stage of the scope in turn;
//! - a thread in [`Slots::wait`] for an item nobody has claimed, which runs it there and then;
//! - the scope's owner at the end of the scope, which takes items exactly as a helper does, and
//!   then waits, in order, for whatever other threads are still running.
//!
//! A thread only ever *waits* for an item another thread is already running; everything it
//! could run itself, it runs. So nothing ever waits on a queued job, which is what starves a
//! pool whose workers join: a worker running a nested stage runs its own pieces (caller-runs) and
//! parks only while a piece it cannot run is running elsewhere. The waits follow stage order and
//! nesting, which are acyclic, and a waiting thread never picks up unrelated work, so no item is
//! run on top of another item's half-finished state. That is why this does not use
//! `nagoya::par_for`: a `ParFor` completes when every piece it *published* has run, and a caller
//! that blocks a worker can end up waiting for pieces queued behind other blocked workers.
//!
//! **Chunks are reserved, items are claimed.** A thread taking work (a helper, or the owner at
//! the end of the scope) reserves a *chunk*, a run of consecutive indices, from the stage's
//! cursor with one `fetch_add`, and then claims the chunk's items one at a time, as it reaches
//! each, with the same compare-and-swap on the item's own slot as before. The reservation only
//! says where that thread looks next; it changes no slot. So an item inside a reserved chunk that
//! its reserver has not reached is still `UNCLAIMED`, and a thread in [`Slots::wait`] for it
//! takes it, runs it, and the reserver skips it when it gets there. Nothing a waiter could run
//! is ever hidden from it. Once every chunk of every stage is reserved, a thread with nothing
//! left *sweeps* the stages for items reserved elsewhere and not reached yet, so the tail of a
//! slow chunk is shared rather than waited for.
//!
//! **Chunks are cut by weight, not by count.** A call site may say what each item is expected to
//! cost ([`run_stage_weighted`], [`StageScope::stage_weighted`]), in estimated nanoseconds: a
//! cheap structural fact about the item it already holds, the bytes of source it covers
//! (`TyCtxt::stage_weight`, `Span::byte_len_untracked`), times what its pass was measured to
//! cost per byte (`cost`). Read at stage start on the owner, never timed and never remembered.
//! The stage's plan cuts its indices into runs of about equal weight,
//! `total / (threads * CHUNKS_PER_THREAD)` each and never less than `MIN_CHUNK_WEIGHT`, so one
//! large body is a chunk of its own and a hundred small ones share one. A chunk is contiguous
//! and one thread claims its items in order, so the slots it fills sit together and different
//! threads write the same cache line only where two chunks meet. A stage with no weights
//! counts every item as one chunk's worth (`UNKNOWN_ITEM_WEIGHT`), which cuts by count exactly
//! as it always did.
//!
//! **Serial when the work cannot pay for the fanout.** A plan with nothing for a helper to take
//! (under two chunks, or under two chunks' weight) runs serially, in order, on the owner: a
//! [`run_stage`] then is the serial loop itself, the same calls in the same order as a serial
//! session, and a stage in a scope is one chunk that wakes nobody (the scope's machinery still
//! runs it, because its replay has to come out in stage order with the scope's other stages).
//! Small sessions were paying 2.5 to 3 times the work of a pass in wakes, parks and contention
//! for items of a few microseconds each; `MIN_CHUNK_WEIGHT` says where the line is drawn.
//!
//! **The context is installed once per run of items, not once per item.** Every item of a scope
//! runs inside the scope's captured context (`ItemContext`: the session's `SessionGlobals` and
//! the scope's `ImplicitCtxt`) and under a catch, and every item of a scope wants exactly the
//! same values installed; every stage of a scope carries a copy of the same one. A thread taking
//! work installs it once, from the first stage it takes work from, and runs item after item
//! inside, across chunks and across the scope's stages, and installs it again only after an item
//! panicked out through it. What stays per item is what is the item's own: the claim, the
//! cut-off check, the replay's collection frame (so an item's diagnostics are still its own
//! output, replayed in serial order), and the settle of its slot. A thread in [`Slots::wait`]
//! runs a single item, so it installs the context for that one.
//!
//! A panic in item `k` unwinds out of the run to the catch, which settles `k` as failed and
//! records the cut-off at `k`, exactly as it did per item; the rest of the chunk is left
//! unclaimed, and whoever claims those items finds them past the cut-off and fails them without
//! running them. A fatal error in item `k` is caught by the diagnostics hook inside the item,
//! as before, and sets the same cut-off, which the next item's check sees. Either way no item
//! after `k` in serial order starts after `k` stopped, which is what a serial run does.
//!
//! **Waking helpers.** When a stage starts, the scope wants one helper per chunk nobody has
//! reserved yet, across its open stages, and no more than one per `MIN_CHUNK_WEIGHT` of their
//! unreserved weight (so a scope of many tiny stages, one chunk each, still wakes nobody), less
//! the one chunk the owner will take itself (`OWNER_CHUNKS`), and never more than the session's
//! width less one (the owner is the first of `jobs.frontend` threads) or the pool's worker
//! count; helpers already in the scope count towards it. A [`run_stage`] reserves the owner's
//! chunk, the first, before it wakes anybody, so a helper can never take it and leave the owner
//! parked; a scope's owner takes its chunk when it settles. A stage of one chunk wakes nobody:
//! its owner runs it, and a helper woken for it could only take it away and leave the owner
//! parked waiting for it, which is a wake, a park and a second wake, for nothing.
//!
//! **What a parked thread costs.** A waiting thread parks in `nagoya::block_on`. nagoya does
//! nothing when a worker parks: no compensating thread, no detection. The pool has one worker
//! fewer until it wakes. The waits here end, for the reason above; a worker blocking on another
//! worker's *query* is a wait the query system owns (its `QueryWaitGraph` sees a stage's items as
//! children of the query that opened the scope, which is why every item runs inside the scope's
//! captured `ImplicitCtxt`), and nagoya will not notice that one either.
//!
//! # Diagnostics, panics, and where a serial run would have stopped
//!
//! In a parallel session every item runs through its stage's `OrderedReplay`
//! (`rustc_errors::item_scope`), begun on the thread that starts the stage: what the item emits
//! becomes part of its output, recorded as the item finishes and forwarded in item order when
//! the stage concludes (in stage order, on the thread that started it), so the diagnostics come
//! out in the serial order at any worker count.
//! A query's diagnostics are part of the query's output in the same way, and come out at its
//! first consumer in serial order, whichever item happened to compute it
//! (`rustc_errors::item_scope`).
//!
//! A serial run stops at the first item that raises (a fatal error or an internal compiler
//! error): it ran every item before it and none after. Here, the first such item in serial order
//! (earlier stage first, then lower index) cuts off every item after it that has not started;
//! items already running finish. The scope then settles everything, waits for its helpers, and
//! raises what the serial run raised: an internal compiler error's original payload, resumed; or
//! the fatal error, raised again once the replay has every earlier item's diagnostics out. A
//! `FatalError` is still one to `catch_fatal_errors`. [`Slots::wait`] answers `None` for an item
//! that failed or was cut off.
//!
//! # Chains: item `i` of one stage triggers item `i` of the next
//!
//! Two stages over the same items, where the second's item `i` needs only the first's item `i`
//! (type checking, then borrow checking, of one body), are two back-to-back serial loops in a
//! serial run, and a barrier between them in a parallel one: every thread idles on the first
//! stage's tail, and body `i`'s type check has left the cache by the time some other core borrow
//! checks it. [`StageScope::stage_then`] starts the second stage *chained* to the first: when the
//! first stage's item `i` settles with `true`, the thread that ran it runs the second stage's
//! item `i` straight after, inside the same install of the scope's context (`Run::follow`).
//!
//! - **Its own stage in serial order.** The chained stage takes the next stage number, so every
//!   one of its items comes after every item of its upstream in serial order: its replay
//!   forwards after the upstream's, and a fatal error or a panic anywhere in the upstream cuts
//!   off every chained item that has not started, exactly as for a later stage that never
//!   started. An item that already ran speculatively is only diagnostics in its own replay,
//!   dropped with it, and query results, whose diagnostics come out at their first consumer in
//!   serial order whoever computed them (`rustc_errors::item_scope`).
//! - **Held until its upstream item says so.** A chained stage hands out no chunks and is
//!   skipped by sweeps (`Run::ready`) until an item's upstream item has settled with `true`, or
//!   the stage has been released ([`Chain::release`]); an item whose upstream said `false` waits
//!   for the release, which the owner calls where the serial loop ran. Whoever runs a chained
//!   item runs or waits for its upstream item first, so the order holds on every path.
//! - **Owner work between the two.** [`StageScope::conclude_through`] settles and concludes the
//!   upstream (and every stage before it) early, raising what a serial run raised by then, so
//!   what the owner emits next comes after the upstream's diagnostics and before the chained
//!   stage's, as it did serially, while the chained items go on running.
//! - **Serial is the two loops.** In a serial scope nothing is chained: the upstream runs when
//!   started, and the chained stage runs in full, in order, when it is released.

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::Range;
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering, fence};
use core::task::{Context, Poll, Waker};

use parking_lot::Mutex;

use crate::rustc_data_structures::sync::{DynSend, DynSync};
use crate::unwind_janky::Payload;

// ---- slots ---------------------------------------------------------------------------------

/// Nobody has started this item.
const UNCLAIMED: u8 = 0;
/// Some thread claimed the item and is running it.
const RUNNING: u8 = 1;
/// The value is written and will not change.
const READY: u8 = 2;
/// The item raised, or was cut off because a serial run would not have reached it.
const FAILED: u8 = 3;

struct Slot<O> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<O>>,
}

impl<O> Slot<O> {
    fn empty() -> Slot<O> {
        Slot { state: AtomicU8::new(UNCLAIMED), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }
}

/// One stage's outputs: one slot per index, each written once and then frozen.
///
/// Read a settled slot in place with [`get`](Slots::get), wait for one with
/// [`wait`](Slots::wait) (blocking, and running the item if nobody has) or
/// [`ready`](Slots::ready) (a future, for a task), and move every value out with
/// [`into_values`](Slots::into_values) once the scope that filled it has ended.
pub struct Slots<O> {
    slots: Box<[Slot<O>]>,
    /// Threads and tasks waiting for a slot, by index. Woken when that slot settles.
    waiters: Mutex<Vec<(usize, Waker)>>,
    /// How many entries `waiters` holds, readable without its lock.
    ///
    /// Every settle has to find out whether somebody waits for that slot, and nearly always
    /// nobody does; this answers that with a load instead of the lock, which every thread that
    /// settles an item of this stage would otherwise take, one after another, once per item.
    /// Written only under the lock; see `wake` and `ReadySlot::poll` for why a waiter that
    /// registers as the slot settles is never missed.
    waiting: AtomicUsize,
    /// The stage that fills these, while it can still run an item: `None` for a stage run
    /// eagerly. Weak, because the stage owns these. It is the stage's one erased handle (see
    /// `parallel::detach`), so these `Slots` can outlive the scope, as a stage's output does; it
    /// upgrades only while the scope that owns the stage is open, and never after.
    runner: Option<Weak<dyn Run>>,
}

// SAFETY: a slot's value is written once, by the one thread that claimed the slot, before the
// `Release` store of `READY`; it is read only after an `Acquire` load of `READY`, and never
// written again while `&self` exists (`into_values` owns `self`). So sharing needs `O: Sync` for
// the readers, and sending needs `O: Send` for the writer.
unsafe impl<O: Send> Send for Slots<O> {}
unsafe impl<O: Send + Sync> Sync for Slots<O> {}

impl<O> Slots<O> {
    fn with_runner(len: usize, runner: Option<Weak<dyn Run>>) -> Slots<O> {
        Slots {
            slots: (0..len).map(|_| Slot::empty()).collect(),
            waiters: Mutex::new(Vec::new()),
            waiting: AtomicUsize::new(0),
            runner,
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn claim(&self, index: usize) -> bool {
        self.slots[index]
            .state
            .compare_exchange(UNCLAIMED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Whether slot `index` is claimed and not settled. Only meaningful to the thread that
    /// claimed it, which is the only thread that can settle it.
    #[cfg_attr(not(feature = "parallel"), allow(dead_code))]
    fn is_running(&self, index: usize) -> bool {
        self.slots[index].state.load(Ordering::Acquire) == RUNNING
    }

    /// Write the value of a slot this thread claimed.
    fn fill(&self, index: usize, value: O) {
        let slot = &self.slots[index];
        debug_assert_eq!(slot.state.load(Ordering::Relaxed), RUNNING);
        // SAFETY: the slot is `RUNNING` and this thread claimed it, so nobody else writes it and
        // nobody reads it until the `READY` store below.
        unsafe { (*slot.value.get()).write(value) };
        slot.state.store(READY, Ordering::Release);
        self.wake(index);
    }

    /// Settle a slot this thread claimed without a value.
    #[cfg_attr(not(feature = "parallel"), allow(dead_code))]
    fn fail(&self, index: usize) {
        self.slots[index].state.store(FAILED, Ordering::Release);
        self.wake(index);
    }

    /// The value of slot `index`, if it is ready. Never waits.
    pub fn get(&self, index: usize) -> Option<&O> {
        let slot = &self.slots[index];
        if slot.state.load(Ordering::Acquire) == READY {
            // SAFETY: `READY` was stored after the write, with `Release`, and a ready slot is
            // never written again while `&self` exists.
            Some(unsafe { (*slot.value.get()).assume_init_ref() })
        } else {
            None
        }
    }

    /// Whether slot `index` has settled, with a value or without one.
    pub fn is_settled(&self, index: usize) -> bool {
        matches!(self.slots[index].state.load(Ordering::Acquire), READY | FAILED)
    }

    /// A future for slot `index`: `Some` with the value once written, `None` if the item failed.
    ///
    /// For a task. It only waits: it never runs the item itself, so something else has to (the
    /// stage's helpers, a [`wait`](Slots::wait)er, or the end of the scope).
    pub fn ready(&self, index: usize) -> ReadySlot<'_, O> {
        ReadySlot { slots: self, index }
    }

    /// The value of slot `index`, blocking until it settles; `None` if the item failed.
    ///
    /// If nobody has started the item, the calling thread runs it here, rather than waiting for a
    /// helper to get to it, so a waiter only ever blocks for an item that is already running.
    /// That holds inside a chunk another thread has reserved, too: a reservation claims nothing
    /// (see "How items get run"). Call it from the scope's owner or from inside one of the
    /// scope's items: an item run here uses the thread's registry slot, which every participant
    /// holds.
    pub fn wait(&self, index: usize) -> Option<&O> {
        loop {
            match self.slots[index].state.load(Ordering::Acquire) {
                READY => return self.get(index),
                FAILED => return None,
                UNCLAIMED => {
                    let runner = self.runner.as_ref().and_then(Weak::upgrade);
                    match runner {
                        // Somebody may claim it between the load and here; then go round again.
                        Some(runner) => {
                            if self.claim(index) {
                                runner.run_claimed(index);
                            }
                        }
                        // No stage can fill it any more: a serial stage that unwound part way.
                        None => return None,
                    }
                }
                _ => return self.block(index),
            }
        }
    }

    #[cfg(feature = "parallel")]
    fn block(&self, index: usize) -> Option<&O> {
        crate::rustc_data_structures::sync::pool::block_on(self.ready(index))
    }

    #[cfg(not(feature = "parallel"))]
    fn block(&self, index: usize) -> Option<&O> {
        // Without the pool only this thread runs items, so a running item can only be waited for
        // from inside itself.
        panic!("stage item {index} waited for itself")
    }

    /// Wait for every slot, in order, running whatever nobody has started.
    pub fn wait_all(&self) {
        for index in 0..self.len() {
            let _ = self.wait(index);
        }
    }

    /// Every value, moved out in index order.
    ///
    /// # Panics
    ///
    /// If a slot has no value: its item failed, or never ran. After a scope that returned
    /// normally every slot has one, because a failed item makes the scope raise instead.
    pub fn into_values(mut self) -> Vec<O> {
        let len = self.len();
        let mut values = Vec::with_capacity(len);
        for index in 0..len {
            let slot = &mut self.slots[index];
            assert!(*slot.state.get_mut() == READY, "stage slot {index} has no value");
            // Marked empty before the read, so `Drop` does not drop what was moved out even if a
            // later slot panics.
            *slot.state.get_mut() = UNCLAIMED;
            // SAFETY: it was `READY`, so written, and `self` is owned, so nobody reads it.
            values.push(unsafe { slot.value.get_mut().assume_init_read() });
        }
        values
    }

    /// Wake whoever waits for slot `index`, which has just settled.
    fn wake(&self, index: usize) {
        // The settle's store, then this fence, then the count; a waiter registers (the count),
        // then its own fence, then reads the state (`ReadySlot::poll`). With a `SeqCst` fence
        // on both sides at least one of the two reads sees the other side's write: either the
        // waiter sees the slot settled and does not sleep, or this sees it counted and takes the
        // lock to wake it. Nobody waiting is by far the common case, and then this is a fence
        // and a load of a line nobody writes, where it was a lock every settling thread shared.
        fence(Ordering::SeqCst);
        if self.waiting.load(Ordering::Relaxed) == 0 {
            return;
        }
        let woken: Vec<Waker> = {
            let mut waiters = self.waiters.lock();
            let woken = waiters
                .extract_if(.., |(waiting_for, _)| *waiting_for == index)
                .map(|(_, waker)| waker)
                .collect();
            self.waiting.store(waiters.len(), Ordering::Relaxed);
            woken
        };
        // Woken outside the lock: a waker is foreign code.
        for waker in woken {
            waker.wake();
        }
    }
}

impl<O> Drop for Slots<O> {
    fn drop(&mut self) {
        for slot in self.slots.iter_mut() {
            if *slot.state.get_mut() == READY {
                // SAFETY: `READY` means written, and `&mut self` means no reader is left.
                unsafe { slot.value.get_mut().assume_init_drop() };
            }
        }
    }
}

/// The future [`Slots::ready`] returns.
pub struct ReadySlot<'a, O> {
    slots: &'a Slots<O>,
    index: usize,
}

impl<'a, O> Future for ReadySlot<'a, O> {
    type Output = Option<&'a O>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<&'a O>> {
        let (slots, index) = (self.slots, self.index);
        if slots.is_settled(index) {
            return Poll::Ready(slots.get(index));
        }
        {
            let mut waiters = slots.waiters.lock();
            let waker = context.waker();
            if !waiters.iter().any(|(i, w)| *i == index && w.will_wake(waker)) {
                waiters.push((index, waker.clone()));
            }
            slots.waiting.store(waiters.len(), Ordering::Relaxed);
        }
        // Registered, and counted, before the state is read again; the fence pairs with the one
        // in `wake`, so a settle between the two reads either is seen here or sees the count and
        // takes the lock, which is ordered after the push above.
        fence(Ordering::SeqCst);
        if slots.is_settled(index) { Poll::Ready(slots.get(index)) } else { Poll::Pending }
    }
}

// ---- stages --------------------------------------------------------------------------------

/// A stage, seen from the threads that run it.
///
/// Only parallel scopes build stages that implement it; a serial stage settles every slot before
/// it returns, so nothing ever has to run one of its items later.
#[cfg_attr(not(feature = "parallel"), allow(dead_code))]
trait Run: Send + Sync {
    /// The stage's item count.
    fn len(&self) -> usize;
    /// Reserve the next chunk of indices nobody has reserved, in index order, or `None` once
    /// every index has been. A reservation claims nothing: see "How items get run".
    fn reserve(&self) -> Option<Range<usize>>;
    /// How many chunks nobody has reserved yet.
    fn unreserved_chunks(&self) -> usize;
    /// Claim item `index`, if nobody has started it.
    fn claim(&self, index: usize) -> bool;
    /// The weight of the chunks nobody has reserved yet.
    fn unreserved_weight(&self) -> u64;
    /// The weight of the chunk that starts at index `start`, or zero if no chunk does.
    fn chunk_weight_at(&self, start: usize) -> u64;
    /// Run `run` inside the scope's context, which every stage of the scope carries a copy of.
    fn enter_context(&self, run: &mut dyn FnMut());
    /// Run item `index`, which this thread claimed, and settle its slot, inside the scope's
    /// context, which the caller has installed. May unwind, leaving the slot claimed; the caller
    /// catches the unwind outside the context and hands it to [`panicked`](Run::panicked).
    fn run_in_context(&self, index: usize);
    /// `payload` unwound out of `run_in_context(index)`: record it, and settle the item.
    fn panicked(&self, index: usize, payload: Payload);
    /// Run item `index`, which this thread claimed, and settle its slot. Never unwinds.
    fn run_claimed(&self, index: usize);
    /// Block until every item has settled, running any nobody has started.
    fn settle_all(&self);
    /// Every item has settled and nothing earlier raised: finish the stage's replay, which
    /// forwards its items' diagnostics, and raise the fatal error this stage stopped at, if it
    /// did.
    fn conclude(&self);
    /// The stage's number in its scope, which is also its place in the scope's list.
    fn seq(&self) -> u32;
    /// Whether item `index` may start now. Always, for a stage started with
    /// [`StageScope::stage`]; for one started with [`StageScope::stage_then`], once its upstream
    /// item has settled and either said so or the stage was released ("Chains" below).
    fn ready(&self, index: usize) -> bool;
    /// Item `index` of this stage's upstream has just settled with a value, on this thread,
    /// inside the scope's context: run this stage's item `index` here and now if it is ready
    /// and nobody has claimed it. Never unwinds.
    fn follow(&self, index: usize);
    /// Make `next` this stage's follower, whose item `i` [`follow`](Run::follow)s this stage's
    /// item `i`. A stage has at most one; `false` if it already had one.
    fn set_then(&self, next: Arc<dyn Run>) -> bool;
    /// Let every item of a chained stage start, whatever its upstream item said.
    fn release(&self);
}

/// A scope in which stages run. See the module header, and [`stages`].
///
/// Neither `Send` nor `Sync`: stages are started by the thread that owns the scope, in the order
/// a serial run would run them, which is what decides which items a raise cuts off.
pub struct StageScope<'scope, 'env: 'scope> {
    /// `None` for a serial scope.
    #[cfg(feature = "parallel")]
    parallel: Option<parallel::Open<'scope>>,
    /// The next stage's number.
    #[cfg_attr(not(feature = "parallel"), allow(dead_code))]
    next_seq: Cell<u32>,
    /// How many of the scope's first stages [`conclude_through`](Self::conclude_through) has
    /// concluded already; the end of the scope concludes the rest.
    #[cfg_attr(not(feature = "parallel"), allow(dead_code))]
    concluded: Cell<u32>,
    /// Invariant in both, as `std::thread::Scope` is: `'scope` must not shrink to a borrow the
    /// body could end early, nor `'env` stretch past the data it names.
    _scope: PhantomData<&'scope mut &'scope ()>,
    _env: PhantomData<&'env mut &'env ()>,
    _owner_thread: PhantomData<*const ()>,
}

/// Run `body` with a scope to start stages in, and return once every item of every stage it
/// started has settled.
///
/// Raises what a serial run would have raised, as described in the module header, after that.
/// If `body` itself panics, the scope still settles everything before the panic goes on, and
/// whatever the items raised is dropped in favour of it.
pub fn stages<'env, R>(body: impl for<'scope> FnOnce(&'scope StageScope<'scope, 'env>) -> R) -> R {
    #[cfg(feature = "parallel")]
    if let Some(shared) = parallel::ScopeShared::for_this_session() {
        // The whole scope runs inside the capture, so `'scope` ends inside the frames that
        // installed the context it carries; see `ItemContext`.
        return parallel::ItemContext::capture(move |context| {
            parallel::scope(shared, context, body)
        });
    }
    body(&StageScope {
        #[cfg(feature = "parallel")]
        parallel: None,
        next_seq: Cell::new(0),
        concluded: Cell::new(0),
        _scope: PhantomData,
        _env: PhantomData,
        _owner_thread: PhantomData,
    })
}

/// One stage in a scope of its own: `f(&input, i)` for every `i` in `0..len`, and every output,
/// moved out in index order, once all of them have settled.
///
/// The common case, a `par_*` loop's replacement: an arena slice of ids in, one output per id
/// out (`()` for a stage run for its effects on the query system).
///
/// Every item counts as heavy (`UNKNOWN_ITEM_WEIGHT`): the stage is cut by count, and only a
/// stage of one item runs serially. Where the call site can say what an item costs, use
/// [`run_stage_weighted`].
pub fn run_stage<'env, In, O, F>(input: In, len: usize, f: F) -> Vec<O>
where
    In: DynSync + DynSend + 'env,
    O: DynSync + DynSend + 'env,
    F: Fn(&In, usize) -> O + DynSync + DynSend + 'env,
{
    run_stage_by(input, len, None, f)
}

/// [`run_stage`], with `weight(&input, i)` the expected cost of item `i` in nanoseconds: the
/// bytes of source it covers times its pass's cost per byte (`cost::weight`,
/// `TyCtxt::stage_weight`).
///
/// In a parallel session the weights are read once, on this thread, before any item runs; a
/// stage whose weight cannot pay for a helper runs as the serial loop, here and now (module
/// header, "Serial when the work cannot pay for the fanout"), and a larger one is cut into
/// chunks of about equal weight. `weight` must be cheap and must not emit anything: read a
/// table, never run a query that could report. In a serial session it is never called.
pub fn run_stage_weighted<'env, In, O, W, F>(input: In, len: usize, weight: W, f: F) -> Vec<O>
where
    In: DynSync + DynSend + 'env,
    O: DynSync + DynSend + 'env,
    W: Fn(&In, usize) -> u32,
    F: Fn(&In, usize) -> O + DynSync + DynSend + 'env,
{
    run_stage_by(input, len, Some(&weight), f)
}

/// What a pass costs per byte of the source an item covers, in nanoseconds: the factor that
/// turns a byte count into a stage weight (`cost::weight`).
///
/// Measured on this crate's own `src/` (1,424 files, 28.9 MB of source) at width one with
/// `time_passes`: a pass's time over the bytes it read. These are estimates for deciding whether
/// work pays for a helper and for cutting it evenly, nothing else: a wrong one changes when items
/// run, never what they answer. A pass nobody measured takes the rate of the measured pass it
/// resembles, and says so; where that is a guess, the lower rate, which errs towards serial.
pub mod cost {
    /// Walks of an item's HIR that check attributes, stability, privacy, liveness or lints and
    /// infer nothing. `misc_checking_1` (the attribute walk, the unstable-API walk and a few
    /// crate lookups) is 54 ms, about 2 ns per byte for its two walks: 1 each.
    pub const WALK: u32 = 1;

    /// Type checking and the passes as heavy: `type_check_crate` 109 ms and
    /// `coherence_checking` 104 ms, about 4 ns per byte. Borrow checking, well-formedness and the
    /// facts' body walk (which type checks) were not measured on their own and take this rate;
    /// so does lowering, which lies between this and parsing (45 ns per byte), taking the lower.
    pub const TYPECK: u32 = 4;

    /// Late name resolution: `late_resolve_crate` 2,110 ms, about 73 ns per byte.
    pub const RESOLVE: u32 = 73;

    /// `bytes` of source at `ns_per_byte`, as a stage weight. Saturates.
    #[inline]
    pub fn weight(bytes: u32, ns_per_byte: u32) -> u32 {
        bytes.saturating_mul(ns_per_byte)
    }
}

fn run_stage_by<'env, In, O, F>(
    input: In,
    len: usize,
    weight: Option<&dyn Fn(&In, usize) -> u32>,
    f: F,
) -> Vec<O>
where
    In: DynSync + DynSend + 'env,
    O: DynSync + DynSend + 'env,
    F: Fn(&In, usize) -> O + DynSync + DynSend + 'env,
{
    #[cfg(feature = "parallel")]
    if len > 1
        && let Some(threads) = parallel::threads_here()
    {
        // The serial loop, timed: the first items run here, in order, exactly as the serial
        // loop below runs them, until what they took says whether the rest pays for helpers
        // (module header, "Serial until the work is measured"). The rest is then a stage of
        // its own over `start..len`, whose replay forwards after everything this loop emitted,
        // so the order is the serial one either way.
        let weigh = |index: usize| match weight {
            Some(weight) => u64::from(weight(&input, index).max(1)),
            None => 1,
        };
        let mut out = Vec::with_capacity(len);
        let clock = eko::time::Instant::now();
        let mut done = 0u64;
        let mut check = parallel::FIRST_LOOK_NS;
        let mut start = 0;
        while start < len {
            out.push(f(&input, start));
            done += weigh(start);
            start += 1;
            if start == len {
                return out;
            }
            let spent = u64::try_from(clock.elapsed().as_nanos()).unwrap_or(u64::MAX);
            if spent < check {
                continue;
            }
            // Nanoseconds per unit of weight, as measured on this stage's own items: the rest's
            // weights become nanoseconds, and those decide the helpers and cut the chunks.
            let in_ns = |w: u64| {
                u64::try_from(u128::from(w) * u128::from(spent) / u128::from(done.max(1)))
                    .unwrap_or(u64::MAX)
            };
            let rest = start;
            let plan = parallel::Plan::cut(
                len - rest,
                &|k| in_ns(weigh(rest + k)),
                threads,
                parallel::MIN_CHUNK_NS,
            );
            if plan.pays()
                && let Some(shared) = parallel::ScopeShared::for_this_session()
            {
                let slots = parallel::ItemContext::capture(move |context| {
                    parallel::scope(shared, context, move |scope| {
                        scope.start_planned(
                            (input, rest),
                            plan,
                            move |(input, rest): &(In, usize), index| f(input, rest + index),
                            true,
                            true,
                            None,
                        )
                    })
                });
                match Arc::try_unwrap(slots) {
                    Ok(slots) => out.extend(slots.into_values()),
                    // The scope has dropped its stages and every helper has left it, so this
                    // `Arc` is the only one.
                    Err(_) => {
                        unreachable!("a stage's slots were still shared after its scope ended")
                    }
                }
                return out;
            }
            // Not yet: look again once this stage has run twice as long.
            check = spent.saturating_mul(2);
        }
        return out;
    }
    #[cfg(not(feature = "parallel"))]
    let _ = weight;
    // The serial loop: in a serial session, and in a parallel one for a stage too small to pay
    // for a helper. The same calls in the same order either way, on this thread, with what an
    // item emits going where it goes in a serial session: straight out, or into the item this
    // stage is nested in, which is where the replay would have forwarded it at once too.
    (0..len).map(|index| f(&input, index)).collect()
}

impl<'scope, 'env> StageScope<'scope, 'env> {
    /// Start a stage: `f(&input, i)` for every `i` in `0..len`, each output in its own slot.
    ///
    /// Returns at once in a parallel session, with the items running on the pool; runs them all
    /// first, in order, in a serial one. `input` is the stage's frozen input, owned by the stage
    /// for its life (a slice, an `Arc`, another stage's slots), and `f` reads its item in place.
    ///
    /// Every item counts as heavy; see [`run_stage`] and [`stage_weighted`](Self::stage_weighted).
    pub fn stage<In, O, F>(&'scope self, input: In, len: usize, f: F) -> Arc<Slots<O>>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        self.stage_by(input, len, None, f)
    }

    /// [`stage`](Self::stage), with `weight(&input, i)` the expected cost of item `i`; see
    /// [`run_stage_weighted`]. A stage too small to pay for a helper is one chunk and wakes
    /// nobody; the scope's owner runs it in order when it settles, unless a helper the scope
    /// woke for a larger stage gets there first.
    pub fn stage_weighted<In, O, W, F>(
        &'scope self,
        input: In,
        len: usize,
        weight: W,
        f: F,
    ) -> Arc<Slots<O>>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        W: Fn(&In, usize) -> u32,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        self.stage_by(input, len, Some(&weight), f)
    }

    fn stage_by<In, O, F>(
        &'scope self,
        input: In,
        len: usize,
        weight: Option<&dyn Fn(&In, usize) -> u32>,
        f: F,
    ) -> Arc<Slots<O>>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        #[cfg(feature = "parallel")]
        if let Some(open) = &self.parallel {
            // Weights here are relative sizes, not yet nanoseconds: the owner measures what
            // they cost when it settles and wakes helpers then (`ScopeShared::drain`), so the
            // stage is cut by weight alone and wakes nobody now.
            let weigh = |index: usize| match weight {
                Some(weight) => u64::from(weight(&input, index).max(1)),
                None => 1,
            };
            let plan = parallel::Plan::cut(len, &weigh, open.threads(), 0);
            return self.start_planned(input, plan, f, false, false, None);
        }
        let _ = weight;
        let slots = Slots::with_runner(len, None);
        for index in 0..len {
            let value = f(&input, index);
            assert!(slots.claim(index));
            slots.fill(index, value);
        }
        Arc::new(slots)
    }

    /// Start a stage whose plan is made, in this parallel scope. `owner_first`: reserve the first
    /// chunk for the owner before waking anybody, for a scope whose owner settles right after
    /// (a [`run_stage`]). `wake`: the plan's weights are measured nanoseconds, so helpers are
    /// woken for them now; otherwise the owner wakes them once it has measured (`drain`).
    /// `upstream`: the stage is chained to that one (see [`stage_then`](Self::stage_then)).
    #[cfg(feature = "parallel")]
    fn start_planned<In, O, F>(
        &'scope self,
        input: In,
        plan: parallel::Plan,
        f: F,
        owner_first: bool,
        wake: bool,
        upstream: Option<Arc<Slots<bool>>>,
    ) -> Arc<Slots<O>>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        let open = self.parallel.as_ref().expect("only a parallel scope starts a planned stage");
        let seq = self.next_seq.get();
        self.next_seq.set(seq.saturating_add(1));
        parallel::start(open, seq, input, plan, f, owner_first, wake, upstream)
    }

    /// Start a stage chained to `upstream`, a stage of this scope with one `bool` per item:
    /// item `i` of the new stage runs `f(&input, i)` as soon as `upstream`'s item `i` has settled
    /// with `true`, on the thread that settled it, right after it. An item whose upstream said
    /// `false` waits for [`Chain::release`]. See "Chains" in the module header.
    ///
    /// The new stage is a stage of its own in every other way: its own number, after
    /// `upstream`'s, so its items come after every one of `upstream`'s in serial order; its own
    /// replay; its own slots. In a serial session, and in a scope that is serial, nothing runs
    /// here: the whole stage runs, in order, when it is released, which is where the serial
    /// loop it replaces ran.
    ///
    /// `upstream` must have `len` items and belong to this scope.
    pub fn stage_then<In, O, W, F>(
        &'scope self,
        upstream: &Arc<Slots<bool>>,
        input: In,
        len: usize,
        weight: W,
        f: F,
    ) -> Chain<'scope, In, O, F>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        W: Fn(&In, usize) -> u32,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        assert_eq!(upstream.len(), len, "a chained stage has one item per upstream item");
        #[cfg(feature = "parallel")]
        if let Some(open) = &self.parallel {
            let weigh = |index: usize| u64::from(weight(&input, index).max(1));
            let plan = parallel::Plan::cut(len, &weigh, open.threads(), 0);
            let slots =
                self.start_planned(input, plan, f, false, false, Some(Arc::clone(upstream)));
            let stage = slots.runner.clone();
            open.follow(upstream, stage.as_ref().and_then(Weak::upgrade));
            return Chain { slots, serial: None, stage, _scope: PhantomData };
        }
        let _ = weight;
        Chain {
            slots: Arc::new(Slots::with_runner(len, None)),
            serial: Some((input, f)),
            #[cfg(feature = "parallel")]
            stage: None,
            _scope: PhantomData,
        }
    }

    /// Settle every item of `upstream`'s stage and of every stage started before it, then
    /// conclude those stages in order, now: forward their diagnostics and raise what a serial
    /// run would have raised by the end of `upstream`'s stage. Stages started after it, a
    /// chained stage included, go on running.
    ///
    /// For owner work that a serial run did between two stages: after this, what the owner
    /// emits comes after everything those stages emitted, as it did serially. Does nothing in a
    /// serial scope, where every stage has emitted and raised as it ran.
    pub fn conclude_through<O>(&'scope self, upstream: &Arc<Slots<O>>) {
        #[cfg(feature = "parallel")]
        if let Some(open) = &self.parallel {
            let Some(through) = open.seq_of(upstream) else { return };
            let from = self.concluded.get();
            if through < from {
                return;
            }
            open.conclude_through(from, through);
            self.concluded.set(through + 1);
        }
        let _ = upstream;
    }
}

/// A stage chained to another by [`StageScope::stage_then`], until it is released.
///
/// Must be released inside the scope: in a serial session that is where the stage runs at all.
#[must_use = "a chained stage runs in a serial session only when it is released"]
pub struct Chain<'scope, In, O, F> {
    slots: Arc<Slots<O>>,
    /// A serial scope's stage, run in order by `release`.
    serial: Option<(In, F)>,
    /// A parallel scope's stage, to release.
    #[cfg(feature = "parallel")]
    stage: Option<Weak<dyn Run>>,
    _scope: PhantomData<&'scope ()>,
}

impl<'scope, In, O, F> Chain<'scope, In, O, F>
where
    F: Fn(&In, usize) -> O,
{
    /// Let every item run, and hand back the stage's slots.
    ///
    /// In a serial scope this runs the whole stage, here, in index order, exactly as
    /// [`StageScope::stage`] runs one when it starts: call it where the serial loop ran. In a
    /// parallel one the items whose upstream said `false` become free to take; the end of the
    /// scope runs whatever is left.
    pub fn release(mut self) -> Arc<Slots<O>> {
        if let Some((input, f)) = self.serial.take() {
            for index in 0..self.slots.len() {
                let value = f(&input, index);
                assert!(self.slots.claim(index));
                self.slots.fill(index, value);
            }
        }
        #[cfg(feature = "parallel")]
        if let Some(stage) = self.stage.as_ref().and_then(Weak::upgrade) {
            stage.release();
        }
        Arc::clone(&self.slots)
    }
}

#[cfg(feature = "parallel")]
mod parallel {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::sync::{Arc, Weak};
    use alloc::vec::Vec;

    use core::future::Future;
    use core::ops::Range;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};

    use eko::thread::OnceLock;
    use parking_lot::Mutex;

    use core::cell::Cell;
    use core::marker::PhantomData;

    use super::{Run, Slots, StageScope};
    // The crate's `AtomicU64`, which falls back to `portable_atomic` where the target has none.
    use crate::rustc_data_structures::sync::{AtomicU64, mode};
    use crate::rustc_data_structures::sync::pool;
    use crate::rustc_data_structures::sync::worker_local::{Registry, RegistrySlot};
    // What an item runs in, and where its diagnostics go: the compiler's own types, named
    // directly. The module header says why this module may.
    use crate::rustc_errors::OrderedReplay;
    pub(super) use crate::rustc_middle::ty::tls::ItemContext;
    use crate::unwind_janky::Payload;

    /// How many chunks a stage is cut into per thread that may take them.
    ///
    /// Eight: the last chunks are what balances the load between threads that finish at
    /// different times, and with eight per thread the most a thread can be left holding when the
    /// others run dry is an eighth of its share (less, since the others then sweep its chunk's
    /// tail). Fewer and larger chunks would write the cursor less, but it is already written once
    /// per chunk and not once per item, which is the cost that mattered.
    const CHUNKS_PER_THREAD: usize = 8;

    /// How many unreserved chunks the owner of a scope is counted on to take itself, when it
    /// settles, and so wakes no helper for.
    ///
    /// **A decision rule, not a tuned number.** The measurements (`examples/parallel_timing.rs`,
    /// six clean files) gave one worker 479 ms per pass and two workers 515 ms: adding a helper
    /// made the pass slower, and the profile at two workers had about 106 samples of
    /// `semaphore_signal_trap` and 27 of `__psynch_mutexwait` that one worker does not have.
    /// Those are the wakes and parks of helpers that found nothing to do or took the one item the
    /// owner was about to run and left it parked. A stage whose remaining work is one chunk is
    /// finished by its owner at no cost, and a helper woken for it costs a wake at best and a
    /// wake, a park and a second wake at worst; so one chunk is the threshold below which nobody
    /// is woken. How much work a chunk must hold is the other half of the rule, and is by
    /// weight: `MIN_CHUNK_WEIGHT`.
    const OWNER_CHUNKS: usize = 1;

    /// The least measured work a chunk is cut to, and so the least a helper is ever woken for:
    /// 125 us. A stage needs two chunks of it, 250 us, before it wakes anybody.
    ///
    /// **Where the line comes from.** Fanning one pass out to the full width added about 80 to
    /// 120 us per session whatever the session's size (wakes, parks, registry leases, and
    /// contention on shared query state and the replay, with up to eleven helpers): about 10 us
    /// per helper, measured on this crate's own `src/` at width one against width twelve. A
    /// chunk of 125 us is twelve times what its helper costs, so the fanout is at most a tenth
    /// of what it runs.
    ///
    /// The work it is compared with is measured, never estimated: a stage's weights are only
    /// relative sizes (bytes of source, or one per item), and what they cost in nanoseconds is
    /// read off the clock while the owner runs the stage's first items (`run_stage`) or first
    /// chunks (`ScopeShared::drain`). A pass's cost per byte differs a hundredfold between a
    /// file whose bodies fail to resolve and one that type checks, so no constant rate can say
    /// which stages pay.
    pub(super) const MIN_CHUNK_NS: u64 = 125_000;

    /// How long the owner runs a stage before it first asks whether the rest pays for helpers:
    /// a fifth of a chunk. A stage done by then never touches the pool or the replay; one that is
    /// not has been measured over enough items for the rate to mean something, and has spent
    /// under a tenth of the smallest stage that fans out.
    pub(super) const FIRST_LOOK_NS: u64 = MIN_CHUNK_NS / 5;

    /// An item's place in the serial order: its stage's number, then its index. Packed so the
    /// cut-off can be one atomic `fetch_min`; an index past `u32::MAX` saturates, which can only
    /// make a cut-off late, never early.
    fn serial_order(seq: u32, index: usize) -> u64 {
        (u64::from(seq) << 32) | u64::from(u32::try_from(index).unwrap_or(u32::MAX))
    }

    /// How many helpers unreserved work pays for, before the width and the pool cap it: one per
    /// chunk, and no more than one per `MIN_CHUNK_NS` of measured work (a scope of several tiny
    /// stages has one chunk each and still wakes nobody), less the owner's own chunk.
    fn helpers_for(chunks: usize, weight: u64) -> usize {
        let affordable = usize::try_from(weight / MIN_CHUNK_NS).unwrap_or(usize::MAX);
        chunks.min(affordable).saturating_sub(OWNER_CHUNKS)
    }

    /// How many threads may take a scope's items at once, the owner included: the session's
    /// width, or fewer when the pool has fewer workers.
    fn threads_for(width: usize) -> usize {
        width.min(pool::workers().saturating_add(1))
    }

    /// [`threads_for`] this thread's session, if it runs in parallel.
    pub(super) fn threads_here() -> Option<usize> {
        if mode::is_parallel_here() { Some(threads_for(pool::width())) } else { None }
    }

    /// A value on a cache line of its own: 128 bytes, the line size (and the adjacent-line
    /// prefetch pair) of the machines this runs on. For the few words every thread taking work
    /// writes (a stage's cursor, a scope's helper count and stage list), so those writes do not
    /// evict the words every item reads (a stage's input, function and slots, the cut-off).
    #[repr(align(128))]
    struct Padded<T>(T);

    impl<T> core::ops::Deref for Padded<T> {
        type Target = T;

        fn deref(&self) -> &T {
            &self.0
        }
    }

    /// How a stage's indices are cut into chunks: chunk `k` is `starts[k]..starts[k + 1]`, and
    /// `before[k]` is the weight of every chunk before it, so the last entry of `before` is the
    /// whole stage's. Made once, on the thread starting the stage, and only read after.
    pub(super) struct Plan {
        starts: Box<[usize]>,
        before: Box<[u64]>,
    }

    impl Plan {
        /// Cut `0..len` into runs of about `total / (threads * CHUNKS_PER_THREAD)` weight and at
        /// least `floor`, in index order: an item is added to the open chunk, and the chunk
        /// closes once it holds the target. A last chunk under half the target joins the one
        /// before it. A stage of one item, or under two floors' weight, is one chunk.
        ///
        /// `weigh(i)` is item `i`'s weight, at least one; it is called twice per item, on this
        /// thread, before any item runs. Weights in measured nanoseconds (`run_stage`) take
        /// `MIN_CHUNK_NS` as the floor; relative weights (a scope's stages) take none.
        pub(super) fn cut(
            len: usize,
            weigh: &dyn Fn(usize) -> u64,
            threads: usize,
            floor: u64,
        ) -> Plan {
            let weigh = |index: usize| weigh(index).max(1);
            let total: u64 = (0..len).map(&weigh).sum();
            let per_chunk = u64::try_from(threads.max(1).saturating_mul(CHUNKS_PER_THREAD))
                .unwrap_or(u64::MAX);
            let target = (total / per_chunk).max(floor).max(1);
            let mut starts = Vec::with_capacity(2);
            let mut before = Vec::with_capacity(2);
            starts.push(0);
            before.push(0);
            if len > 1 && total >= 2 * floor {
                // The weight of the chunks closed so far, and of the open one.
                let mut closed = 0u64;
                let mut open = 0u64;
                // The last item never closes a chunk: whatever is open then is the tail.
                for index in 0..len - 1 {
                    open += weigh(index);
                    if open >= target {
                        closed += open;
                        open = 0;
                        starts.push(index + 1);
                        before.push(closed);
                    }
                }
                open += weigh(len - 1);
                if open < target / 2 && starts.len() > 1 {
                    starts.pop();
                    before.pop();
                }
            }
            if len > 0 {
                starts.push(len);
                before.push(total);
            }
            Plan { starts: starts.into_boxed_slice(), before: before.into_boxed_slice() }
        }

        /// The weight of the chunk that starts at `start`, or zero if none does.
        fn chunk_weight_at(&self, start: usize) -> u64 {
            match self.starts.binary_search(&start) {
                Ok(k) if k + 1 < self.starts.len() => self.before[k + 1] - self.before[k],
                _ => 0,
            }
        }

        fn len(&self) -> usize {
            self.starts[self.starts.len() - 1]
        }

        fn chunks(&self) -> usize {
            self.starts.len() - 1
        }

        fn chunk(&self, k: usize) -> Range<usize> {
            self.starts[k]..self.starts[k + 1]
        }

        fn total(&self) -> u64 {
            self.before[self.before.len() - 1]
        }

        /// The weight of chunks `k..`.
        fn weight_from(&self, k: usize) -> u64 {
            self.total() - self.before[k.min(self.chunks())]
        }

        /// Whether a stage of this plan, alone in a scope, would wake a helper. When it would
        /// not, [`run_stage`](super::run_stage) runs it as the serial loop instead.
        pub(super) fn pays(&self) -> bool {
            helpers_for(self.chunks(), self.total()) > 0
        }
    }

    /// A caught panic, and its place in the serial order.
    struct Caught {
        order: u64,
        payload: Payload,
        message: Option<String>,
    }

    /// What a parallel [`StageScope`] holds: the state it shares with its helpers, and the
    /// context its stages' items run in, borrowed for the scope.
    pub(super) struct Open<'scope> {
        shared: Arc<ScopeShared>,
        context: ItemContext<'scope>,
    }

    impl Open<'_> {
        /// How many threads may take this scope's items at once; what a stage's plan is cut for.
        pub(super) fn threads(&self) -> usize {
            self.shared.threads
        }
    }

    /// Run a parallel scope's `body`, then settle it and raise what a serial run would have.
    /// Called inside `ItemContext::capture`, which is what `context` borrows from.
    pub(super) fn scope<'c, 'env, R>(
        shared: Arc<ScopeShared>,
        context: ItemContext<'c>,
        body: impl for<'scope> FnOnce(&'scope StageScope<'scope, 'env>) -> R,
    ) -> R {
        let owner = Arc::clone(&shared);
        let scope = StageScope {
            parallel: Some(Open { shared, context }),
            next_seq: Cell::new(0),
            concluded: Cell::new(0),
            _scope: PhantomData,
            _env: PhantomData,
            _owner_thread: PhantomData,
        };
        let guard = SettleOnUnwind(&owner);
        let result = body(&scope);
        core::mem::forget(guard);
        let settled = owner.settle();
        // The stages `conclude_through` concluded already are not concluded again.
        let concluded = usize::try_from(scope.concluded.get()).unwrap_or(usize::MAX);
        owner.conclude(settled, concluded);
        result
    }

    impl Open<'_> {
        /// The number of the stage of this scope that fills `slots`, or `None` for slots no
        /// stage can fill any more (a stage run eagerly).
        ///
        /// # Panics
        ///
        /// If `slots` belong to a stage of another scope.
        pub(super) fn seq_of<O>(&self, slots: &Arc<Slots<O>>) -> Option<u32> {
            let stage = slots.runner.as_ref().and_then(Weak::upgrade)?;
            let seq = stage.seq();
            let ours = self
                .shared
                .stages
                .lock()
                .get(usize::try_from(seq).unwrap_or(usize::MAX))
                .is_some_and(|mine| Arc::ptr_eq(mine, &stage));
            assert!(ours, "a stage of another scope");
            Some(seq)
        }

        /// Make `next`, a stage just started in this scope, the follower of the stage that fills
        /// `upstream`, which must be a stage of this scope too: the follower is held by the
        /// upstream stage's value, so it must not outlive this scope, and a stage of an enclosing
        /// scope does. With no upstream stage to follow (slots a stage filled eagerly), nothing
        /// triggers `next`'s items, and they run when it is released.
        pub(super) fn follow(&self, upstream: &Arc<Slots<bool>>, next: Option<Arc<dyn Run>>) {
            let Some(next) = next else { return };
            if self.seq_of(upstream).is_none() {
                return;
            }
            let Some(stage) = upstream.runner.as_ref().and_then(Weak::upgrade) else { return };
            let first = stage.set_then(next);
            assert!(first, "a stage has one chained follower");
        }

        /// See [`StageScope::conclude_through`]: stages `from..=through`, which are started and
        /// not concluded.
        pub(super) fn conclude_through(&self, from: u32, through: u32) {
            let range = usize::try_from(from).unwrap_or(usize::MAX)
                ..usize::try_from(through).unwrap_or(usize::MAX).saturating_add(1);
            self.shared.conclude_through(range, serial_order(through, usize::MAX));
        }
    }

    /// Settles a parallel scope whose body unwound, so no item outlives what it borrows, then
    /// drops its stages, which discards every stage's replay: the body's panic goes on.
    struct SettleOnUnwind<'a>(&'a ScopeShared);

    impl Drop for SettleOnUnwind<'_> {
        fn drop(&mut self) {
            drop(self.0.settle());
        }
    }

    /// One scope, shared by its owner and its helpers, and handed to the pool with each helper.
    /// Everything in it is owned: the stages are in it as `detach`ed handles, and the context
    /// they run in is in each stage, not here.
    pub(super) struct ScopeShared {
        /// The stages started so far, in order. Taken when the scope ends, which also breaks the
        /// `ScopeShared` -> stage -> `ScopeShared` cycle. Locked by every thread for every chunk
        /// it takes, so on a line of its own.
        stages: Padded<Mutex<Vec<Arc<dyn Run>>>>,
        /// The chunk the owner reserved for itself before waking anybody (`start` with
        /// `owner_first`), which it runs first when it settles. Only the owner touches it, and
        /// `settle` always takes it, which breaks the cycle through its stage as `stages` does.
        owner_first: Mutex<Option<Batch>>,
        /// Set when the scope ends. A helper that sees it touches nothing.
        closed: AtomicBool,
        /// Helpers between arriving and leaving. Written by every job as it arrives and leaves,
        /// so on a line of its own.
        active: Padded<AtomicUsize>,
        /// The owner, waiting for `active` to reach zero.
        idle: Mutex<Option<Waker>>,
        /// No item after this one in serial order starts. `u64::MAX` until something raises.
        /// Read before every item by every thread and written only when something raises, so
        /// on a line of its own, where the writes above cannot evict it.
        cutoff: Padded<AtomicU64>,
        /// The first item, in serial order, that ended in a fatal error its replay caught.
        stop: AtomicU64,
        /// The first item, in serial order, that panicked.
        panic: Mutex<Option<Caught>>,
        registry: Option<Registry>,
        /// The session's `jobs.frontend`: the owner and at most `width - 1` helpers.
        width: usize,
        /// This scope, for the helpers the owner wakes from `&self` (`wake`).
        me: Weak<ScopeShared>,
        /// How many threads may take this scope's items at once, the owner included: `width`,
        /// or fewer when the pool has fewer workers than the session asked for. What a stage's
        /// chunks are sized by.
        threads: usize,
    }

    /// The owner's measure of a scope's work while it drains: when it started, how much weight
    /// it has run, and when it looks next.
    struct Probe {
        clock: eko::time::Instant,
        done: u64,
        check: u64,
    }

    impl Probe {
        /// A chunk of `weight` is done. Once the owner has run `FIRST_LOOK_NS`, and again
        /// each time it has run twice as long, wake the helpers the rest pays for at the rate
        /// measured so far.
        fn chunk_done(&mut self, shared: &ScopeShared, weight: u64) {
            if weight == 0 {
                return;
            }
            self.done += weight;
            let spent = u64::try_from(self.clock.elapsed().as_nanos()).unwrap_or(u64::MAX);
            if spent < self.check {
                return;
            }
            self.check = spent.saturating_mul(2);
            shared.wake_measured(self.done, spent);
        }
    }

    /// A run of one stage's indices that a thread goes through in order, claiming each item it
    /// finds unclaimed: a chunk it reserved, or a whole stage it sweeps.
    struct Batch {
        stage: Arc<dyn Run>,
        indices: Range<usize>,
    }

    impl ScopeShared {
        /// A parallel scope if this thread's session runs in parallel, else `None`.
        pub(super) fn for_this_session() -> Option<Arc<ScopeShared>> {
            if !mode::is_parallel_here() {
                return None;
            }
            let width = pool::width();
            Some(Arc::new_cyclic(|me| ScopeShared {
                stages: Padded(Mutex::new(Vec::new())),
                owner_first: Mutex::new(None),
                closed: AtomicBool::new(false),
                active: Padded(AtomicUsize::new(0)),
                idle: Mutex::new(None),
                cutoff: Padded(AtomicU64::new(u64::MAX)),
                stop: AtomicU64::new(u64::MAX),
                panic: Mutex::new(None),
                registry: Registry::try_current(),
                width,
                me: me.clone(),
                threads: threads_for(width),
            }))
        }

        fn stop_at(&self, order: u64) {
            self.stop.fetch_min(order, Ordering::AcqRel);
            self.cutoff.fetch_min(order, Ordering::AcqRel);
        }

        fn stash(&self, order: u64, payload: Payload) {
            let message = crate::unwind_janky::take_last_panic();
            self.cutoff.fetch_min(order, Ordering::AcqRel);
            let displaced = {
                let mut slot = self.panic.lock();
                if slot.as_ref().is_none_or(|caught| order < caught.order) {
                    slot.replace(Caught { order, payload, message })
                } else {
                    Some(Caught { order, payload, message })
                }
            };
            // Dropped outside the lock: a payload's destructor is foreign code.
            drop(displaced);
        }

        /// Helpers that `chunks` unreserved chunks of `weight_ns` measured work pay for, capped
        /// by the session's width and the pool (module header, "Waking helpers").
        fn wanted(&self, chunks: usize, weight_ns: u64) -> usize {
            helpers_for(chunks, weight_ns).min(self.width - 1).min(pool::workers())
        }

        /// Submit helpers until `wanted` are in the scope. Helpers already in it pick up new work
        /// when they finish their chunk, so only the shortfall is submitted, within the
        /// application's budget shared by every scope and session: when the pool is full, the
        /// rest is the owner's to run. A helper on its way out may be counted and not come back;
        /// the owner settles whatever is left, so that costs time, never an item.
        fn wake(&self, wanted: usize) {
            let present = self.active.load(Ordering::Relaxed);
            for _ in present..wanted {
                // Alive: this is called by a thread holding the scope.
                let Some(shared) = self.me.upgrade() else { return };
                if !pool::try_submit(move || help(shared)) {
                    break;
                }
            }
        }

        /// The owner has run `done` weight of this scope's chunks in `spent_ns`: wake the
        /// helpers the rest pays for at that rate.
        fn wake_measured(&self, done: u64, spent_ns: u64) {
            let (chunks, weight) = self.stages.lock().iter().fold((0usize, 0u64), |(c, w), stage| {
                (c + stage.unreserved_chunks(), w + stage.unreserved_weight())
            });
            if chunks == 0 {
                return;
            }
            let rest_ns = u64::try_from(
                u128::from(weight) * u128::from(spent_ns) / u128::from(done.max(1)),
            )
            .unwrap_or(u64::MAX);
            self.wake(self.wanted(chunks, rest_ns));
        }

        /// The next unreserved chunk of any open stage, earliest stage first.
        ///
        /// `from` is where this thread looks first, and moves past every stage whose indices
        /// are all reserved: a reservation is never given back, so such a stage has nothing
        /// more to hand out. Stages are only ever appended, so one started later is still found.
        fn next_chunk(&self, from: &mut usize) -> Option<Batch> {
            loop {
                // A `let`, so the lock is released before the reservation.
                let stage = self.stages.lock().get(*from).cloned()?;
                if let Some(indices) = stage.reserve() {
                    return Some(Batch { stage, indices });
                }
                *from += 1;
            }
        }

        /// Chunks nobody has reserved, across every open stage.
        fn unreserved_chunks(&self) -> usize {
            self.stages.lock().iter().map(|stage| stage.unreserved_chunks()).sum()
        }

        /// The next stage to sweep, whole, for items reserved by another thread and not reached
        /// yet. Each stage once per thread; `from` is how far this thread has got.
        fn next_sweep(&self, from: &mut usize) -> Option<Batch> {
            let stage = self.stages.lock().get(*from).cloned()?;
            *from += 1;
            let indices = 0..stage.len();
            Some(Batch { stage, indices })
        }

        /// Run, on this thread, every item of the scope it can claim: unreserved chunks first,
        /// earliest stage first, then a sweep of every stage for items reserved elsewhere and not
        /// started. Returns when there is nothing left to claim; items other threads are running
        /// may still be running. Never unwinds.
        ///
        /// The scope's context is installed once around the whole run and the catch is outside
        /// it, so an item costs neither (module header, "How items get run"). It is installed
        /// from the first stage the run takes work from: every stage of the scope carries a copy
        /// of the scope's one context, so it is the right one for every stage the run goes on
        /// to. An item that panics unwinds to the catch, is settled as failed with the cut-off
        /// at it, and the run goes on with the context installed again; the rest of the
        /// panicking item's batch is dropped unclaimed, which is only items after it in serial
        /// order, all cut off, and settled (failed, unrun) by whichever thread claims them, the
        /// owner at the latest.
        ///
        /// `whole` is the owner's run: every unreserved chunk, then the sweep. A pool job runs
        /// with `whole` false: one chunk, then it returns, and `help` hands the next chunk to a
        /// new job. The owner is the thread that called into the compiler; a job is fanout work
        /// that ends when its chunk does, not a worker that keeps taking work.
        ///
        /// `first` is a chunk this thread reserved earlier (the owner's, `owner_first`), run
        /// before anything else is taken.
        fn drain(&self, whole: bool, first: Option<Batch>) {
            let mut chunks_from = 0;
            let mut sweep_from = 0;
            // The weight of the chunk being run, counted towards `probe` when it is done (zero
            // for a sweep, which runs only what other threads left).
            let mut batch_weight =
                first.as_ref().map_or(0, |b| b.stage.chunk_weight_at(b.indices.start));
            let mut batch: Option<Batch> = first;
            let mut taken = false;
            // The owner's clock: what it has run and how long that took says what the rest
            // costs, and so how many helpers it pays for (`wake_measured`). A helper runs one
            // chunk and never looks.
            let mut probe = whole.then(|| Probe {
                clock: eko::time::Instant::now(),
                done: 0,
                check: FIRST_LOOK_NS,
            });
            // The item this thread claimed and is running, so a panic out of it can be settled.
            let mut running: Option<usize> = None;
            let next = |chunks_from: &mut usize,
                        sweep_from: &mut usize,
                        taken: &mut bool,
                        weight: &mut u64| {
                if !whole && *taken {
                    return None;
                }
                *taken = true;
                if let Some(chunk) = self.next_chunk(chunks_from) {
                    *weight = chunk.stage.chunk_weight_at(chunk.indices.start);
                    return Some(chunk);
                }
                *weight = 0;
                if whole { self.next_sweep(sweep_from) } else { None }
            };
            loop {
                if batch.is_none() {
                    batch = next(&mut chunks_from, &mut sweep_from, &mut taken, &mut batch_weight);
                }
                // A handle of its own, so `batch` is free to move on to other stages under it.
                let installer = match &batch {
                    Some(first) => Arc::clone(&first.stage),
                    None => return,
                };
                let outcome = pool::catch(|| {
                    installer.enter_context(&mut || {
                        loop {
                            if batch.is_none() {
                                batch = next(
                                    &mut chunks_from,
                                    &mut sweep_from,
                                    &mut taken,
                                    &mut batch_weight,
                                );
                            }
                            let Some(current) = &mut batch else { return };
                            match current.indices.next() {
                                Some(index) => {
                                    // A chained item whose upstream has not let it start is
                                    // left for whoever it is released to.
                                    if current.stage.ready(index) && current.stage.claim(index) {
                                        running = Some(index);
                                        current.stage.run_in_context(index);
                                        running = None;
                                    }
                                }
                                None => {
                                    batch = None;
                                    if let Some(probe) = &mut probe {
                                        probe.chunk_done(self, batch_weight);
                                    }
                                }
                            }
                        }
                    })
                });
                match outcome {
                    Ok(()) => return,
                    Err(payload) => match (batch.take(), running.take()) {
                        (Some(batch), Some(index)) => batch.stage.panicked(index, payload),
                        // Not out of an item: the scaffolding (the context install, a thread-local
                        // out of keys). Recorded like a helper's own failure, and this thread
                        // stops taking work; the owner's settle still settles every item.
                        _ => {
                            self.stash(u64::MAX, payload);
                            return;
                        }
                    },
                }
            }
        }

        /// Settle every item of every stage, running whatever nobody has started, wait for the
        /// helpers to leave, and hand back the stages. Never unwinds.
        pub(super) fn settle(&self) -> Vec<Arc<dyn Run>> {
            // First everything this thread can start, taken exactly as a helper takes it. It was
            // a walk of every index in order, waiting at each one a helper was running: at two
            // threads the owner and its one helper went through the same indices side by side,
            // and the owner parked at nearly every other item.
            // The chunk it reserved for itself, if any, goes first; taken here on every path,
            // the unwinding one included, which also breaks the cycle through its stage.
            let first = self.owner_first.lock().take();
            self.drain(true, first);
            // Then every item, in stage order, waiting only for what other threads are running.
            // No stage can be added meanwhile: only the owner starts stages, and the owner is
            // here.
            //
            // The lock is taken per lookup in a `let`, never in a `while let` scrutinee: that
            // would hold it across `settle_all`, and helpers need it to find their next item.
            let mut next = 0;
            loop {
                let stage = self.stages.lock().get(next).cloned();
                let Some(stage) = stage else { break };
                stage.settle_all();
                next += 1;
            }
            // Then the helpers. `SeqCst` on both sides, against `help`: either a helper sees
            // `closed` and leaves without touching anything, or this sees it in `active` and
            // waits for it.
            self.closed.store(true, Ordering::SeqCst);
            pool::block_on(Idle(self));
            // The only strong references left: every helper is gone, and slots hold weak ones.
            // Dropping them drops every stage's input and function, inside the scope.
            core::mem::take(&mut *self.stages.lock())
        }

        /// Settle and conclude the stages `range` of this scope, which the owner started and has
        /// not concluded, before the scope ends: `last` is the serial order of the last item any
        /// of them can have. Raises what a serial run would have raised by then, if anything:
        /// the first panic at or before `last` unless a fatal error came first, else the first
        /// fatal error, from the stage that stopped at it. Later stages keep running and keep
        /// whatever they raise for the end of the scope. Owner only.
        ///
        /// Settling runs what this thread can take, exactly as the end of the scope does
        /// (`settle`), and then waits for these stages' items other threads are running, and for
        /// nothing else: a later stage's item, a chained one run right after its upstream item
        /// included, may still be running when this returns.
        pub(super) fn conclude_through(&self, range: Range<usize>, last: u64) {
            let first = self.owner_first.lock().take();
            self.drain(true, first);
            for index in range.clone() {
                let stage = self.stages.lock().get(index).cloned();
                if let Some(stage) = stage {
                    stage.settle_all();
                }
            }
            let stop = self.stop.load(Ordering::Acquire);
            let caught = {
                let mut slot = self.panic.lock();
                match &*slot {
                    Some(caught) if caught.order <= last && caught.order <= stop => slot.take(),
                    _ => None,
                }
            };
            if let Some(Caught { payload, message, .. }) = caught {
                // The unwind settles the scope and drops every stage, discarding their replays,
                // as a panic at the end of the scope does.
                pool::resume(payload, message);
            }
            // In stage order; a stage that stopped at a fatal error raises it here, after every
            // earlier item's diagnostics, and the unwind discards the rest.
            for index in range {
                let stage = self.stages.lock().get(index).cloned();
                if let Some(stage) = stage {
                    stage.conclude();
                }
            }
        }

        /// Raise what a serial run would have raised, if anything: the earliest of the first
        /// panic and the first fatal error, in serial order. The first `concluded` stages were
        /// concluded already, by `conclude_through`.
        pub(super) fn conclude(&self, stages: Vec<Arc<dyn Run>>, concluded: usize) {
            let caught = self.panic.lock().take();
            let stop = self.stop.load(Ordering::Acquire);
            if let Some(Caught { order, payload, message }) = caught {
                // `<=`, not `<`: an item cannot both panic and stop, so the two are equal only
                // when both are `u64::MAX`, a helper's panic outside any item with no fatal
                // error anywhere. That one is raised rather than lost.
                if order <= stop {
                    // Dropping the stages discards every stage's replay.
                    drop(stages);
                    pool::resume(payload, message);
                }
                // A panic after a fatal error in serial order is one a serial run never reached.
                drop(payload);
            }
            // In stage order: every stage before the one a fatal error stopped hands its hook a
            // normal `finish`; that one's raises, and the unwind drops the rest, discarding.
            for stage in stages.iter().skip(concluded) {
                stage.conclude();
            }
        }
    }

    /// Ready when no helper is inside the scope.
    struct Idle<'a>(&'a ScopeShared);

    impl Future for Idle<'_> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
            let shared = self.0;
            if shared.active.load(Ordering::SeqCst) == 0 {
                return Poll::Ready(());
            }
            let old = shared.idle.lock().replace(context.waker().clone());
            drop(old);
            if shared.active.load(Ordering::SeqCst) == 0 { Poll::Ready(()) } else { Poll::Pending }
        }
    }

    struct Stage<'scope, In, O, F> {
        input: In,
        f: F,
        slots: Arc<Slots<O>>,
        /// This stage's items' diagnostics, forwarded in item order when it concludes, and
        /// dropped with the stage when it does not (the unwind path).
        replay: OrderedReplay,
        /// The scope's context, copied in when the stage started; see `drain`.
        context: ItemContext<'scope>,
        /// The first chunk nobody has reserved. Only says which chunk is handed out next: an item
        /// is claimed by its own slot's state, never by this. Written by every thread that takes
        /// a chunk, so on a line of its own, away from the fields every item reads.
        cursor: Padded<AtomicUsize>,
        /// How the indices are cut into chunks; see `Plan`.
        plan: Plan,
        seq: u32,
        scope: Arc<ScopeShared>,
        /// The stage chained to this one, whose item `i` runs right after this one's item `i`
        /// settles, on the same thread (`follow`). Set once, by the owner, right after the
        /// follower starts. A strong reference that is part of this stage's value, so it is
        /// dropped with it, inside the scope (see `detach`).
        then: OnceLock<Arc<dyn Run>>,
        /// For a chained stage: what its items wait for.
        gate: Option<Gate>,
    }

    /// What a chained stage's items wait for: their upstream item, and either its `true` or the
    /// stage's release.
    struct Gate {
        upstream: Arc<Slots<bool>>,
        released: AtomicBool,
    }

    impl<In, O, F> Stage<'_, In, O, F> {
        /// A chained stage that has not been released: its chunks are not handed out, because
        /// its items are run by their upstream items (`follow`) or wait for the release.
        fn held(&self) -> bool {
            self.gate.as_ref().is_some_and(|gate| !gate.released.load(Ordering::Acquire))
        }
    }

    // SAFETY: unchecked, as the module header says: the call site's `DynSend`/`DynSync` bounds
    // are implemented for every type, and this is the one place that word is taken. `Slots` is
    // accessed through its own synchronisation, `replay` through its own (per-item entries and
    // a lock, `rustc_errors::item_scope`), `then` and `gate` through their atomics, and `input`,
    // `f` and `context` only through `&`.
    unsafe impl<In, O, F> Send for Stage<'_, In, O, F> {}
    unsafe impl<In, O, F> Sync for Stage<'_, In, O, F> {}

    impl<In, O, F> Run for Stage<'_, In, O, F>
    where
        F: Fn(&In, usize) -> O,
    {
        fn len(&self) -> usize {
            self.slots.len()
        }

        fn reserve(&self) -> Option<Range<usize>> {
            if self.held() {
                return None;
            }
            let chunks = self.plan.chunks();
            // Read first: once every chunk is reserved, every later look is a load of a line
            // nobody writes, rather than one more `fetch_add` on it.
            if self.cursor.load(Ordering::Relaxed) >= chunks {
                return None;
            }
            let chunk = self.cursor.fetch_add(1, Ordering::Relaxed);
            if chunk >= chunks {
                return None;
            }
            Some(self.plan.chunk(chunk))
        }

        fn unreserved_chunks(&self) -> usize {
            if self.held() {
                return 0;
            }
            let chunks = self.plan.chunks();
            chunks - self.cursor.load(Ordering::Relaxed).min(chunks)
        }

        fn unreserved_weight(&self) -> u64 {
            if self.held() {
                return 0;
            }
            self.plan.weight_from(self.cursor.load(Ordering::Relaxed))
        }

        fn chunk_weight_at(&self, start: usize) -> u64 {
            self.plan.chunk_weight_at(start)
        }

        fn claim(&self, index: usize) -> bool {
            self.slots.claim(index)
        }

        fn enter_context(&self, run: &mut dyn FnMut()) {
            self.context.enter(run)
        }

        fn run_in_context(&self, index: usize) {
            // A chained item starts after its upstream item has settled, whoever runs it. When it
            // is run by `follow`, or taken after the check in `drain`, it has; otherwise (the end
            // of the scope, a `Slots::wait`) this runs the upstream item or waits for it. An
            // upstream item that failed set the cut-off at or before itself first (`stop_at`,
            // `stash`, or it was cut off), and this item comes after it in serial order, so the
            // check below then fails this one unrun, as a serial run never reaches it.
            if let Some(gate) = &self.gate {
                let _ = gate.upstream.wait(index);
            }
            let order = serial_order(self.seq, index);
            if order > self.scope.cutoff.load(Ordering::Acquire) {
                self.slots.fail(index);
                return;
            }
            // The item runs inside the scope's captured context on *every* thread, the owner's
            // included, and not in whatever the running thread happens to have: a thread that
            // runs it while waiting for it is inside another item, maybe inside a query. The
            // item's queries must see the scope's `ImplicitCtxt`, whose `query` is the parent the
            // query system's cycle check walks from a stage's jobs back to the query that opened
            // the scope. The caller installed it; inside it, the replay collects what the item
            // emits, per item.
            let mut value = None;
            let stopped = self.replay.run_item(index, || value = Some((self.f)(&self.input, index)));
            if stopped {
                self.scope.stop_at(order);
            }
            match value {
                Some(value) => {
                    self.slots.fill(index, value);
                    // The chained stage's item `index`, here and now, while what this item
                    // touched is still in this core's cache.
                    if let Some(next) = self.then.get() {
                        next.follow(index);
                    }
                }
                None => self.slots.fail(index),
            }
        }

        fn seq(&self) -> u32 {
            self.seq
        }

        fn ready(&self, index: usize) -> bool {
            match &self.gate {
                None => true,
                Some(gate) => {
                    gate.upstream.is_settled(index)
                        && (gate.released.load(Ordering::Acquire)
                            || gate.upstream.get(index) != Some(&false))
                }
            }
        }

        fn follow(&self, index: usize) {
            if !self.ready(index) || !self.slots.claim(index) {
                return;
            }
            // Caught here, inside the context the caller installed, so a panic in this item is
            // this item's, settled and ordered as its own, and never reaches the upstream item's
            // run, which has already settled.
            if let Err(payload) = pool::catch(|| self.run_in_context(index)) {
                self.panicked(index, payload);
            }
        }

        fn set_then(&self, next: Arc<dyn Run>) -> bool {
            self.then.set(next).is_ok()
        }

        fn release(&self) {
            if let Some(gate) = &self.gate {
                gate.released.store(true, Ordering::Release);
            }
        }

        fn panicked(&self, index: usize, payload: Payload) {
            // Only this thread can settle an item it claimed, so "still running" here means the
            // unwind came out of the item itself, and the cut-off goes at the item. Anything else
            // (the item had settled, and something after it unwound) is not the item's.
            if self.slots.is_running(index) {
                // Recorded before the slot settles, so the cut-off is in place before anyone
                // woken by the settle looks for more work.
                self.scope.stash(serial_order(self.seq, index), payload);
                self.slots.fail(index);
            } else {
                self.scope.stash(u64::MAX, payload);
            }
        }

        fn run_claimed(&self, index: usize) {
            // One item, run by a thread that waited for it: the context is installed for it
            // alone. A thread taking work in runs installs it once per run instead (`drain`).
            let outcome =
                pool::catch(|| self.context.enter(&mut || self.run_in_context(index)));
            if let Err(payload) = outcome {
                self.panicked(index, payload);
            }
        }

        fn settle_all(&self) {
            self.slots.wait_all();
        }

        fn conclude(&self) {
            if let Err(fatal) = self.replay.finish() {
                fatal.raise()
            }
        }
    }

    /// The one place a stage lets go of `'scope`: its weak self-reference, as the `'static`
    /// handle nagoya's pool and a stage's escaping `Slots` need.
    ///
    /// Every other `'static` handle to the stage is an upgrade of what this returns: `start`
    /// takes the scope's strong reference that way, helpers clone that one into their batches,
    /// and `Slots::wait` upgrades the runner. Nothing else erases anything; the helpers are
    /// handed `Arc<ScopeShared>`, which borrows nothing.
    ///
    /// Why no owned design removes it: the stage owns its input and function, but the function
    /// borrows `TyCtxt<'tcx>`, and `'tcx` is a stack frame of the session (`GlobalCtxt` is not
    /// behind an `Arc`); the pool's `submit` requires `'static`. The scope blocking until every
    /// helper has left is what makes the erasure sound, and that is exactly what `std::thread`
    /// scopes rely on too.
    fn detach<'scope>(stage: Weak<dyn Run + 'scope>) -> Weak<dyn Run> {
        // SAFETY: only the trait object's lifetime changes; the layout and the vtable are the
        // same. What `'scope` guards is the stage's value (its input, function, replay and
        // context borrow for `'scope`), and that value is reached only through a strong
        // reference, so the proof is that no strong reference exists once `'scope` may end:
        //
        // - Strong references come only from upgrading this weak one. They are held by the
        //   scope's `stages` list, by a thread's current `Batch` (the owner's, or a helper's), by
        //   the owner's reserved first chunk in `owner_first` until `settle` takes it, by
        //   `drain` while it installs the context, by `Slots::wait` while it runs an
        //   unclaimed item, and, for a chained stage, by its upstream stage's `then`, which is
        //   part of the upstream stage's value and dropped with it, so it is gone once every
        //   strong reference to the upstream stage is (`Open::follow` only chains stages of one
        //   scope).
        // - `stages` (and the unwind guard `SettleOnUnwind`) runs `settle` before it returns or
        //   unwinds, and `stages` is where `'scope` ends. `settle` settles every item, so no
        //   `Slots::wait` finds one unclaimed any more; closes the scope and waits until no
        //   helper is inside it, and a helper's `Leave` drops after its batches; and takes the
        //   list, which `conclude` drops (or the unwind out of `conclude` does) before `stages`
        //   returns. A helper that arrives after the scope closed sees `closed` and touches no
        //   stage, and `closed` is `SeqCst` against its `active` count, so it is either seen and
        //   waited for or it sees `closed`.
        // - So the strong count reaches zero inside `'scope`, the stage's value is dropped
        //   there, and from then on `upgrade` fails for good (a strong count never comes back
        //   from zero). What is left is weak references, in `Slots` that may outlive the
        //   scope, whose only other use is their drop, which frees the allocation using the
        //   layout in the vtable, a `'static` table.
        unsafe { core::mem::transmute::<Weak<dyn Run + 'scope>, Weak<dyn Run>>(stage) }
    }

    /// Start a stage in a parallel scope, cut as `plan` says. See `StageScope::stage`.
    ///
    /// `owner_first`: reserve the stage's first chunk for the owner before waking anybody, so a
    /// helper can never take the chunk the owner is about to run and leave it parked; for a
    /// scope whose owner settles as soon as this returns (`run_stage`).
    pub(super) fn start<'scope, In, O, F>(
        open: &Open<'scope>,
        seq: u32,
        input: In,
        plan: Plan,
        f: F,
        owner_first: bool,
        wake: bool,
        upstream: Option<Arc<Slots<bool>>>,
    ) -> Arc<Slots<O>>
    where
        In: 'scope,
        O: 'scope,
        F: Fn(&In, usize) -> O + 'scope,
    {
        let shared = &open.shared;
        let len = plan.len();
        // Begun here, on the thread starting the stage, which is where the replay finds the
        // item this stage is nested in, if any.
        let replay = OrderedReplay::new(len);
        let mut detached: Option<Weak<dyn Run>> = None;
        let typed = Arc::new_cyclic(|this: &Weak<Stage<'scope, In, O, F>>| {
            let runner = detach(this.clone());
            detached = Some(runner.clone());
            Stage {
                input,
                f,
                slots: Arc::new(Slots::with_runner(len, Some(runner))),
                replay,
                context: open.context,
                cursor: Padded(AtomicUsize::new(0)),
                plan,
                seq,
                scope: shared.clone(),
                then: OnceLock::new(),
                gate: upstream
                    .map(|upstream| Gate { upstream, released: AtomicBool::new(false) }),
            }
        });
        let slots = typed.slots.clone();
        // The scope's strong reference, as an upgrade of the detached one (see `detach`); then
        // the typed one goes, and this is the stage's only strong reference.
        let stage: Arc<dyn Run> = detached
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("a stage is alive while its starter holds it");
        drop(typed);
        let own = if owner_first { Some(Arc::clone(&stage)) } else { None };
        // The work nobody has taken yet, in chunks and in weight, across every open stage of the
        // scope, this one included: counted under the same lock that publishes the stage.
        let (chunks, weight) = {
            let mut stages = shared.stages.lock();
            stages.push(stage);
            stages.iter().fold((0usize, 0u64), |(chunks, weight), stage| {
                (chunks + stage.unreserved_chunks(), weight + stage.unreserved_weight())
            })
        };
        // Helpers already in the scope pick this stage up when they finish their current chunk,
        // so only the shortfall is submitted. The owner is counted on for `OWNER_CHUNKS` of the
        // work, a helper is woken only for a chunk's weight of it, and the session's width caps
        // the rest (module header, "Waking helpers"). A helper on its way out may be counted and
        // not come back; the owner settles whatever is left, so that costs time, never an item.
        let wanted = if wake { shared.wanted(chunks, weight) } else { 0 };
        // The owner's chunk, the one `OWNER_CHUNKS` counted, reserved before anybody is woken.
        // A reservation claims nothing, so a waiter can still run any item of it.
        if let Some(own) = own
            && let Some(indices) = own.reserve()
        {
            let displaced = shared.owner_first.lock().replace(Batch { stage: own, indices });
            debug_assert!(displaced.is_none(), "a scope reserves one first chunk for its owner");
            drop(displaced);
        }
        shared.wake(wanted);
        slots
    }

    /// Leaves the scope when dropped, waking the owner if it was the last helper out of a closed
    /// scope. Declared first in `help`, so it drops last, after every `Arc` into a stage.
    struct Leave<'a>(&'a ScopeShared);

    impl Drop for Leave<'_> {
        fn drop(&mut self) {
            let shared = self.0;
            let last = shared.active.fetch_sub(1, Ordering::SeqCst) == 1;
            if last && shared.closed.load(Ordering::SeqCst) {
                let owner = shared.idle.lock().take();
                if let Some(waker) = owner {
                    waker.wake();
                }
            }
        }
    }

    /// One fanout job: arrive, take a registry slot, run one unreserved chunk, give the slot back,
    /// and if chunks are left, submit the next job for them.
    ///
    /// A job runs one chunk and ends; it does not loop taking work. Handing the rest on as a new
    /// job, submitted after this one's slot is free, keeps at most `width - 1` jobs holding slots
    /// at once, the session's budget, with nagoya's fanout queues deciding where each runs.
    fn help(shared: Arc<ScopeShared>) {
        shared.active.fetch_add(1, Ordering::SeqCst);
        let _leave = Leave(&shared);
        if shared.closed.load(Ordering::SeqCst) {
            return;
        }
        run_one_chunk(&shared);
        let more = !shared.closed.load(Ordering::SeqCst) && shared.unreserved_chunks() > 0;
        if more {
            let next = Arc::clone(&shared);
            // This job's place in the application budget passes to its successor.
            pool::submit_continuation(move || help(next));
        }
    }

    /// The body of one `help` job, with its registry slot held only around its chunk.
    fn run_one_chunk(shared: &ScopeShared) {
        // `drain` never unwinds, so this catches only the scaffolding (a registry or mode
        // scope out of thread-local keys). Nothing is claimed while it can fail except inside
        // `drain`, so a failure here strands no item.
        let outcome = pool::catch(|| {
            // The registry slot is also the session's thread budget: the session asked for
            // `width` threads and its registry has that many slots. A helper that finds none
            // free leaves the items to the threads that have one.
            let slot: Option<RegistrySlot> = match &shared.registry {
                Some(registry) => match registry.lease() {
                    Some(slot) => Some(slot),
                    None => return,
                },
                None => None,
            };
            let _in_slot = slot.as_ref().map(RegistrySlot::enter);
            let _mode = mode::enter_session_width(shared.width);
            shared.drain(false, None);
        });
        if let Err(payload) = outcome {
            shared.stash(u64::MAX, payload);
        }
    }
}
