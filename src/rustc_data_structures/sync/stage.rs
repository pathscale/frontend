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
//! The size of a chunk is `len / (threads * CHUNKS_PER_THREAD)`, at least one: small enough that
//! the last chunks balance the load, large enough that the cursor, which every thread taking
//! work writes, is written once per chunk and not once per item.
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
//! reserved yet, across its open stages, less the one chunk the owner will take itself when it
//! settles (`OWNER_CHUNKS`), and never more than the session's width less one (the owner is the
//! first of `jobs.frontend` threads) or the pool's worker count; helpers already in the scope
//! count towards it. A stage of one item, alone in its scope, wakes nobody: its owner runs it,
//! and a helper woken for it could only take it away and leave the owner parked waiting for it,
//! which is a wake, a park and a second wake, for nothing.
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
pub fn run_stage<'env, In, O, F>(input: In, len: usize, f: F) -> Vec<O>
where
    In: DynSync + DynSend + 'env,
    O: DynSync + DynSend + 'env,
    F: Fn(&In, usize) -> O + DynSync + DynSend + 'env,
{
    let slots = stages(|scope| scope.stage(input, len, f));
    match Arc::try_unwrap(slots) {
        Ok(slots) => slots.into_values(),
        // The scope has dropped its stages and every helper has left it, so this `Arc` is the
        // only one.
        Err(_) => unreachable!("a stage's slots were still shared after its scope ended"),
    }
}

impl<'scope, 'env> StageScope<'scope, 'env> {
    /// Start a stage: `f(&input, i)` for every `i` in `0..len`, each output in its own slot.
    ///
    /// Returns at once in a parallel session, with the items running on the pool; runs them all
    /// first, in order, in a serial one. `input` is the stage's frozen input, owned by the stage
    /// for its life (a slice, an `Arc`, another stage's slots), and `f` reads its item in place.
    pub fn stage<In, O, F>(&'scope self, input: In, len: usize, f: F) -> Arc<Slots<O>>
    where
        In: DynSync + DynSend + 'scope,
        O: DynSync + DynSend + 'scope,
        F: Fn(&In, usize) -> O + DynSync + DynSend + 'scope,
    {
        #[cfg(feature = "parallel")]
        if let Some(open) = &self.parallel {
            let seq = self.next_seq.get();
            self.next_seq.set(seq.saturating_add(1));
            return parallel::start(open, seq, input, len, f);
        }
        let slots = Slots::with_runner(len, None);
        for index in 0..len {
            let value = f(&input, index);
            assert!(slots.claim(index));
            slots.fill(index, value);
        }
        Arc::new(slots)
    }
}

#[cfg(feature = "parallel")]
mod parallel {
    use alloc::string::String;
    use alloc::sync::{Arc, Weak};
    use alloc::vec::Vec;

    use core::future::Future;
    use core::ops::Range;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};

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
    /// is woken. A larger threshold would need what the measurements do not give, the cost of an
    /// item, and would be wrong for the stages that have few items and heavy ones: the two
    /// whole-crate lint stages (`rustc_lint::late::check_crate`, one item each, in one scope, so
    /// together they still wake one helper under this rule) and the two diagnostics passes of
    /// `frontend_facts`, one stage of two items, which still wakes one.
    const OWNER_CHUNKS: usize = 1;

    /// An item's place in the serial order: its stage's number, then its index. Packed so the
    /// cut-off can be one atomic `fetch_min`; an index past `u32::MAX` saturates, which can only
    /// make a cut-off late, never early.
    fn serial_order(seq: u32, index: usize) -> u64 {
        (u64::from(seq) << 32) | u64::from(u32::try_from(index).unwrap_or(u32::MAX))
    }

    /// How many consecutive indices one reservation takes: `len / (threads * CHUNKS_PER_THREAD)`,
    /// at least one. `threads` counts every thread that may take the stage's items, its owner
    /// included.
    fn chunk_size(len: usize, threads: usize) -> usize {
        (len / threads.max(1).saturating_mul(CHUNKS_PER_THREAD)).max(1)
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
            _scope: PhantomData,
            _env: PhantomData,
            _owner_thread: PhantomData,
        };
        let guard = SettleOnUnwind(&owner);
        let result = body(&scope);
        core::mem::forget(guard);
        let settled = owner.settle();
        owner.conclude(settled);
        result
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
        /// `ScopeShared` -> stage -> `ScopeShared` cycle.
        stages: Mutex<Vec<Arc<dyn Run>>>,
        /// Set when the scope ends. A helper that sees it touches nothing.
        closed: AtomicBool,
        /// Helpers between arriving and leaving.
        active: AtomicUsize,
        /// The owner, waiting for `active` to reach zero.
        idle: Mutex<Option<Waker>>,
        /// No item after this one in serial order starts. `u64::MAX` until something raises.
        cutoff: AtomicU64,
        /// The first item, in serial order, that ended in a fatal error its replay caught.
        stop: AtomicU64,
        /// The first item, in serial order, that panicked.
        panic: Mutex<Option<Caught>>,
        registry: Option<Registry>,
        /// The session's `jobs.frontend`: the owner and at most `width - 1` helpers.
        width: usize,
        /// How many threads may take this scope's items at once, the owner included: `width`,
        /// or fewer when the pool has fewer workers than the session asked for. What a stage's
        /// chunks are sized by.
        threads: usize,
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
            Some(Arc::new(ScopeShared {
                stages: Mutex::new(Vec::new()),
                closed: AtomicBool::new(false),
                active: AtomicUsize::new(0),
                idle: Mutex::new(None),
                cutoff: AtomicU64::new(u64::MAX),
                stop: AtomicU64::new(u64::MAX),
                panic: Mutex::new(None),
                registry: Registry::try_current(),
                width,
                threads: width.min(pool::workers().saturating_add(1)),
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
        fn drain(&self, whole: bool) {
            let mut chunks_from = 0;
            let mut sweep_from = 0;
            let mut batch: Option<Batch> = None;
            let mut taken = false;
            // The item this thread claimed and is running, so a panic out of it can be settled.
            let mut running: Option<usize> = None;
            let mut next = |chunks_from: &mut usize, sweep_from: &mut usize, taken: &mut bool| {
                if !whole && *taken {
                    return None;
                }
                *taken = true;
                self.next_chunk(chunks_from).or_else(|| {
                    if whole { self.next_sweep(sweep_from) } else { None }
                })
            };
            loop {
                if batch.is_none() {
                    batch = next(&mut chunks_from, &mut sweep_from, &mut taken);
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
                                batch = next(&mut chunks_from, &mut sweep_from, &mut taken);
                            }
                            let Some(current) = &mut batch else { return };
                            match current.indices.next() {
                                Some(index) => {
                                    if current.stage.claim(index) {
                                        running = Some(index);
                                        current.stage.run_in_context(index);
                                        running = None;
                                    }
                                }
                                None => batch = None,
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
            self.drain(true);
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

        /// Raise what a serial run would have raised, if anything: the earliest of the first
        /// panic and the first fatal error, in serial order.
        pub(super) fn conclude(&self, stages: Vec<Arc<dyn Run>>) {
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
            for stage in &stages {
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
        /// The first index nobody has reserved. Only says where the next chunk starts: an item
        /// is claimed by its own slot's state, never by this.
        cursor: AtomicUsize,
        /// How many indices one reservation takes; see `chunk_size`.
        chunk: usize,
        seq: u32,
        scope: Arc<ScopeShared>,
    }

    // SAFETY: unchecked, as the module header says: the call site's `DynSend`/`DynSync` bounds
    // are implemented for every type, and this is the one place that word is taken. `Slots` is
    // accessed through its own synchronisation, `replay` through its own (per-item entries and
    // a lock, `rustc_errors::item_scope`), and `input`, `f` and `context` only through `&`.
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
            let len = self.slots.len();
            // Read first: once every index is reserved, every later look is a load of a line
            // nobody writes, rather than one more `fetch_add` on it.
            if self.cursor.load(Ordering::Relaxed) >= len {
                return None;
            }
            let start = self.cursor.fetch_add(self.chunk, Ordering::Relaxed);
            if start >= len {
                return None;
            }
            Some(start..len.min(start + self.chunk))
        }

        fn unreserved_chunks(&self) -> usize {
            let len = self.slots.len();
            (len - self.cursor.load(Ordering::Relaxed).min(len)).div_ceil(self.chunk)
        }

        fn claim(&self, index: usize) -> bool {
            self.slots.claim(index)
        }

        fn enter_context(&self, run: &mut dyn FnMut()) {
            self.context.enter(run)
        }

        fn run_in_context(&self, index: usize) {
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
                Some(value) => self.slots.fill(index, value),
                None => self.slots.fail(index),
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
        //   `drain` while it installs the context, and by `Slots::wait` while it runs an
        //   unclaimed item.
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

    /// Start a stage in a parallel scope. See `StageScope::stage`.
    pub(super) fn start<'scope, In, O, F>(
        open: &Open<'scope>,
        seq: u32,
        input: In,
        len: usize,
        f: F,
    ) -> Arc<Slots<O>>
    where
        In: 'scope,
        O: 'scope,
        F: Fn(&In, usize) -> O + 'scope,
    {
        let shared = &open.shared;
        // Begun here, on the thread starting the stage, which is where the replay finds the
        // item this stage is nested in, if any.
        let replay = OrderedReplay::new(len);
        let chunk = chunk_size(len, shared.threads);
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
                cursor: AtomicUsize::new(0),
                chunk,
                seq,
                scope: shared.clone(),
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
        // The work nobody has taken yet, in chunks, across every open stage of the scope, this
        // one included: counted under the same lock that publishes the stage.
        let unreserved: usize = {
            let mut stages = shared.stages.lock();
            stages.push(stage);
            stages.iter().map(|stage| stage.unreserved_chunks()).sum()
        };
        // Helpers already in the scope pick this stage up when they finish their current chunk,
        // so only the shortfall is submitted. The owner is counted on for `OWNER_CHUNKS` of the
        // work, and the session's width caps the rest (module header, "Waking helpers"). A
        // helper on its way out may be counted and not come back; the owner settles whatever is
        // left, so that costs time, never an item.
        let wanted = unreserved
            .saturating_sub(OWNER_CHUNKS)
            .min(shared.width - 1)
            .min(pool::workers());
        let present = shared.active.load(Ordering::Relaxed);
        for _ in present..wanted {
            let shared = shared.clone();
            // Within the application's budget, shared by every scope and session: when the pool
            // is already full, the rest of this stage is the owner's to run.
            if !pool::try_submit(move || help(shared)) {
                break;
            }
        }
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
            shared.drain(false);
        });
        if let Err(payload) = outcome {
            shared.stash(u64::MAX, payload);
        }
    }
}
