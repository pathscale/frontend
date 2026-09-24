// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::fmt::Debug;
use core::hash::Hash;
use core::num::NonZero;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use alloc::sync::Arc;

// `parking_lot_lite_hack` has no `Condvar`: upstream's is built on `parking_lot_core`'s thread
// parking, which is the part the fork dropped. The one in `eko::thread` is a
// `pthread_cond_t`, so the latch below takes its `Mutex` too - a condvar and the lock it releases
// have to be the same pair.
use eko::thread::{Condvar, Mutex as WaitMutex};
use parking_lot::Mutex;
use crate::rustc_data_structures::hash_table::HashTable;
use crate::rustc_data_structures::sharded::Sharded;
use crate::rustc_errors::QueryDiagnostics;
use crate::rustc_span::Span;

use crate::rustc_middle::queries::TaggedQueryKey;

/// A value uniquely identifying an active query job.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct QueryJobId(pub NonZero<u64>);

/// Represents an active query job.
#[derive(Clone, Debug)]
pub struct QueryJob<'tcx> {
    pub id: QueryJobId,

    /// The span corresponding to the reason for which this query was required.
    pub span: Span,

    /// The parent query job which created this job and is implicitly waiting on it.
    pub parent: Option<QueryJobId>,

    /// The latch that is used to wait on this job.
    pub latch: Option<QueryLatch<'tcx>>,

    /// The thread running this job, as [`thread_token`] gave it, or 0 in a serial session. A
    /// waiter compares it with its own to learn, without a snapshot, whether the job is on its
    /// own stack.
    pub thread: usize,
}

impl<'tcx> QueryJob<'tcx> {
    /// Creates a new query job.
    #[inline]
    pub fn new(id: QueryJobId, span: Span, parent: Option<QueryJobId>) -> Self {
        let thread = if crate::rustc_data_structures::sync::is_dyn_thread_safe() {
            thread_token()
        } else {
            0
        };
        QueryJob { id, span, parent, latch: None, thread }
    }

    pub fn latch(&mut self) -> QueryLatch<'tcx> {
        self.latch.get_or_insert_with(QueryLatch::new).clone()
    }

    /// Signals to waiters that the query is complete.
    ///
    /// This does nothing for single threaded rustc,
    /// as there are no concurrent jobs which could be waiting on us
    ///
    /// **Also the path a panicking query takes.** `ActiveJobGuard`'s drop in
    /// `rustc_query_impl::execution` poisons the key and then calls this, so a thread blocked on
    /// a query whose computing thread unwound (a `FatalError`, an ICE) is woken, finds the
    /// `Poisoned` status and raises `FatalError` itself instead of sleeping forever.
    ///
    /// `graph` is the session's [`QueryWaitGraph`]. Setting the latch removes wait edges, and
    /// the cycle check in [`QueryLatch::wait_on`] needs the set of edges frozen while it looks,
    /// so the latch is set under the same lock. A job nobody waited on has no latch, so the
    /// common case never touches the lock.
    #[inline]
    pub fn signal_complete(self, graph: &QueryWaitGraph) {
        if let Some(latch) = self.latch {
            latch.set(graph);
        }
    }
}

/// The lock that serialises every change to the set of *wait edges* in one query system.
///
/// # What an edge is
///
/// A wait edge is "job J is blocked until job K finishes". There are two kinds:
///
/// - **Stack edges.** A job's `parent` waits for it implicitly: the parent is further up the
///   same stack (or, for a `par_*` piece running on a nagoya worker, the caller's query, which
///   is blocked in the join until the piece retires). These are recorded in `QueryJob::parent`
///   when a job starts and never change.
/// - **Latch edges.** A thread that asks for a key another thread is computing pushes a
///   [`QueryWaiter`] onto that job's [`QueryLatch`] and sleeps. The waiter's `parent` is the
///   query the sleeping thread was running.
///
/// A deadlock is a cycle in this graph. Stack edges alone cannot form one (a job id is always
/// larger than its parent's), so every cycle contains at least one latch edge, and the *last*
/// latch edge added is the one that closes it.
///
/// # Why one lock
///
/// Upstream found cycles after the fact: rayon noticed that every worker was blocked and
/// called a deadlock handler, which looked at a graph that could no longer change. That
/// handler is gone (see `rustc_interface::util`), so the check moves to the moment an edge is
/// added: before a thread sleeps on a latch, it asks whether its new edge closes a cycle. For
/// that answer to be right, two threads must not add the two halves of one cycle at the same
/// time, each looking at a graph without the other's half. So adding a latch edge
/// (`QueryLatch::wait_on`) and removing latch edges (`QueryLatch::set`, and the cycle breaker)
/// all happen under this lock. While a thread holds it, every latch edge it can see belongs to
/// a thread that really is asleep, and a sleeping thread's stack cannot change, so the stack
/// edges below every latch edge are frozen too. The graph the check walks is then the real
/// one wherever it matters.
///
/// # Cost
///
/// Only a thread that is about to sleep on another thread's job takes this lock, plus the
/// completion of a job that somebody waited on. The serial compiler never creates a latch and
/// never takes it.
///
/// Lock order: this lock, then a latch's `waiters` mutex or a query-state shard. Nothing that
/// holds a shard or a latch mutex ever takes this lock.
#[derive(Default)]
pub struct QueryWaitGraph {
    /// Who waits on whom, one entry per thread asleep on a latch: the waiting thread, and the
    /// thread running the job it waits on (see [`thread_token`]). Held while an edge is added and
    /// while a latch is set, which is the lock the module header describes.
    ///
    /// **Why a chain and not a count.** A cycle through a new wait edge is a chain of waits that
    /// comes back to the waiting thread: the job it waits on runs on a thread that is itself
    /// waiting, on a job running on a thread that is waiting, and so on. Each thread waits on at
    /// most one latch, so the chain is at most one step per waiting thread, and walking it is a
    /// few comparisons. Only when it does come back is the full snapshot of active jobs taken, to
    /// build the cycle's report. A count of waiters was not enough: in a stage where many items
    /// wait on one shared query there is always another waiter, so every wait took the snapshot,
    /// a walk of every query's shards under this lock, and every completing latch queued behind it.
    edges: WaitMutex<Vec<(usize, usize)>>,
}

impl QueryWaitGraph {
    pub fn new() -> Self {
        QueryWaitGraph { edges: WaitMutex::new(Vec::new()) }
    }
}

/// Whether following waits from `waitee_thread` comes back to `me`, given who waits on whom.
fn chain_reaches(edges: &[(usize, usize)], me: usize, waitee_thread: usize) -> bool {
    let mut thread = waitee_thread;
    // One step per recorded wait at most; a chain longer than that would repeat a thread, which
    // cannot happen (a thread waits on one latch), so the bound only guards the loop.
    for _ in 0..=edges.len() {
        if thread == me {
            return true;
        }
        match edges.iter().find(|(waiter, _)| *waiter == thread) {
            Some(&(_, next)) => thread = next,
            None => return false,
        }
    }
    true
}

/// A number no other live thread shares: the address of this thread's own slot.
pub fn thread_token() -> usize {
    static SLOT: eko::thread::ThreadLocal<u8> = eko::thread::ThreadLocal::new();
    SLOT.with(|| 0, |slot| slot as *mut u8 as usize).unwrap_or(0)
}

impl Debug for QueryWaitGraph {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("QueryWaitGraph")
    }
}

/// For a particular query and key, tracks the status of a query evaluation
/// that has started, but has not yet finished successfully.
///
/// (Successful query evaluation for a key is represented by an entry in the
/// query's in-memory cache.)
pub enum ActiveKeyStatus<'tcx> {
    /// Some thread is already evaluating the query for this key.
    ///
    /// The enclosed [`QueryJob`] can be used to wait for it to finish.
    Started(QueryJob<'tcx>),

    /// The query panicked. Queries trying to wait on this will raise a fatal error which will
    /// silently panic.
    ///
    /// With what the query emitted before it failed, when it ran inside a par item of a parallel
    /// session and emitted anything: the diagnostics part of its output, which it has instead of
    /// a value. Whoever observes the poison consumes the record before raising, so the partial
    /// diagnostics come out at the first consumer in serial order, as a serial run printed them
    /// before its fatal error. See `rustc_errors::item_scope`. Always `None` in a serial session.
    Poisoned(Option<Arc<QueryDiagnostics>>),
}

/// For a particular query, keeps track of "active" keys, i.e. keys whose
/// evaluation has started but has not yet finished successfully.
///
/// (Successful query evaluation for a key is represented by an entry in the
/// query's in-memory cache.)
pub struct QueryState<'tcx, K> {
    pub active: Sharded<HashTable<(K, ActiveKeyStatus<'tcx>)>>,
}

impl<'tcx, K> Default for QueryState<'tcx, K> {
    fn default() -> QueryState<'tcx, K> {
        QueryState { active: Default::default() }
    }
}

/// Description of a frame in the query stack.
///
/// This is mostly used in case of cycles for error reporting.
#[derive(Debug)]
pub struct QueryStackFrame<'tcx> {
    pub span: Span,

    /// The query and key of the query method call that this stack frame
    /// corresponds to.
    ///
    /// Code that doesn't care about the specific key can still use this to
    /// check which query it's for, or obtain the query's name.
    pub tagged_key: TaggedQueryKey<'tcx>,
}

#[derive(Debug)]
pub struct QueryCycle<'tcx> {
    /// The query and related span that uses the cycle.
    pub usage: Option<QueryStackFrame<'tcx>>,

    /// The span here corresponds to the reason for which this query was required.
    pub frames: Vec<QueryStackFrame<'tcx>>,
}

#[derive(Debug)]
pub struct QueryWaiter<'tcx> {
    pub parent: Option<QueryJobId>,
    pub condvar: Condvar,
    pub span: Span,
    pub cycle: Mutex<Option<QueryCycle<'tcx>>>,
    /// Set, under the latch's `waiters` mutex, by whoever takes this waiter off the latch
    /// (`QueryLatch::set` or the cycle breaker) just before it notifies `condvar`.
    ///
    /// **The condition the sleeper re-checks.** Upstream slept on a `parking_lot::Condvar`,
    /// which never wakes spuriously, so one `wait` call was enough. `eko::thread::Condvar` is a
    /// `pthread_cond_t`, which may, and a spurious return read as "the job is done" would look
    /// the value up in the cache before it is there and report a poisoned query that is not. So
    /// the sleeper loops until this is true. It is written and read only under the latch
    /// mutex; an atomic rather than a `Cell` only so that `QueryWaiter` stays `Sync` behind its
    /// `Arc`.
    pub resumed: AtomicBool,
    /// The waiting thread, as [`thread_token`] gave it: its entry in [`QueryWaitGraph`].
    pub thread: usize,
}

#[derive(Clone, Debug)]
pub struct QueryLatch<'tcx> {
    /// The `Option` is `Some(..)` when the job is active, and `None` once completed.
    pub waiters: Arc<WaitMutex<Option<Vec<Arc<QueryWaiter<'tcx>>>>>>,
}

impl<'tcx> QueryLatch<'tcx> {
    fn new() -> Self {
        QueryLatch { waiters: Arc::new(WaitMutex::new(Some(Vec::new()))) }
    }

    /// Awaits for the query job to complete.
    ///
    /// `query` is the job the calling thread is running (the waiter's side of the new edge),
    /// and `detect_cycle` answers whether the edge this call adds closes a cycle. It is called
    /// with `graph` locked and this thread's waiter already on the latch, so it sees the new
    /// edge and every other one (see [`QueryWaitGraph`]). If it finds a cycle, the edge is taken
    /// back off and the cycle is returned without sleeping: this thread is the one that closed
    /// the cycle, so it is the one that reports it, which is also what the serial compiler does
    /// with a cycle on its own stack. `rustc_query_impl::execution::wait_for_query` supplies it.
    ///
    /// A caller outside any query (`query == None`) is a root: nothing can be waiting on it, so
    /// its edge cannot close a cycle and `detect_cycle` is not called.
    pub fn wait_on(
        &self,
        graph: &QueryWaitGraph,
        query: Option<QueryJobId>,
        span: Span,
        waitee_thread: usize,
        detect_cycle: impl FnOnce() -> Option<QueryCycle<'tcx>>,
    ) -> Result<(), QueryCycle<'tcx>> {
        let me = thread_token();
        // Add the edge and look for a cycle through it, all under the graph lock.
        let mut graph_guard = graph.edges.lock();

        let waiter = {
            let mut waiters_guard = self.waiters.lock();
            let Some(waiters) = &mut *waiters_guard else {
                return Ok(()); // already complete
            };

            let waiter = Arc::new(QueryWaiter {
                parent: query,
                span,
                cycle: Mutex::new(None),
                condvar: Condvar::new(),
                resumed: AtomicBool::new(false),
                thread: me,
            });

            // We push the waiter on to the `waiters` list. It can be accessed inside
            // the `wait` call below, by 1) the `set` method or 2) by the cycle breaker.
            // Both of these will remove it from the `waiters` list before resuming
            // this thread.
            waiters.push(Arc::clone(&waiter));
            waiter
            // The latch mutex is released here: `detect_cycle` reads every latch's waiters,
            // this one included, and the mutex is not reentrant.
        };

        // A cycle through this edge is a chain of waits from the awaited job's thread back to this
        // one (see `QueryWaitGraph::edges`). Only then is the snapshot taken, to report it.
        if query.is_some() && chain_reaches(&graph_guard, me, waitee_thread) {
            if let Some(cycle) = detect_cycle() {
                // Our edge closes the cycle. Take it back out, so the graph holds no cycle
                // again, and report instead of sleeping. The removal is under `graph` too,
                // like every other change to the latch edges.
                self.remove_waiter(&waiter);
                drop(graph_guard);
                return Err(cycle);
            }
        }
        graph_guard.push((me, waitee_thread));

        // The edge is in and closes nothing. Let other threads add and remove edges again.
        // From here this thread counts as asleep: its waiter is on the latch and its stack
        // will not change until the waiter is resumed.
        drop(graph_guard);

        // Awaits the caller on this latch by blocking the current thread.
        //
        // **No pool to tell.** This was `rustc_thread_pool::mark_blocked_and_wait`, which
        // released the worker thread, armed the pool's deadlock handler, ran this wait, then
        // re-acquired. The deadlock handler is replaced by the check above. Whether a nagoya
        // worker blocking here starves the runtime is `sync::parallel`'s concern: this thread
        // is only waiting for a job that another thread is actively computing.
        //
        // `resumed` is set under this same mutex before the notify, so there is no window in
        // which the notify can land before this thread is inside `wait`: either `resumed` is
        // already true when it is checked, or the notifier is still waiting for the mutex that
        // `wait` releases.
        let mut waiters_guard = self.waiters.lock();
        while !waiter.resumed.load(Ordering::Relaxed) {
            // `wait` takes the guard and hands it back, which is `std::sync::Condvar::wait`'s
            // shape: the lock is held on the way in, released while waiting, and held again on
            // return. A spurious return goes round the loop.
            waiters_guard = waiter.condvar.wait(waiters_guard);
        }
        // Release the lock before taking `waiter.cycle` below. Upstream also had a jobserver
        // token to re-acquire here; there is no jobserver in this compiler.
        drop(waiters_guard);

        // FIXME: Get rid of this lock. We have ownership of the QueryWaiter
        // although another thread may still have a Arc reference so we cannot
        // use Arc::get_mut
        let mut cycle = waiter.cycle.lock();
        match cycle.take() {
            None => Ok(()),
            Some(cycle) => Err(cycle),
        }
    }

    /// Sets the latch and resumes all waiters on it
    fn set(&self, graph: &QueryWaitGraph) {
        // Removing edges is a change to the wait graph; see `QueryWaitGraph`.
        let mut graph_guard = graph.edges.lock();
        let mut waiters_guard = self.waiters.lock();
        let waiters = waiters_guard.take().unwrap(); // mark the latch as complete
        for waiter in waiters {
            // Each thread waits on one latch at a time, so its one entry is this wait's.
            graph_guard.retain(|(thread, _)| *thread != waiter.thread);
            // Under the latch mutex, so the sleeper cannot miss it; see `wait_on`.
            waiter.resumed.store(true, Ordering::Relaxed);
            waiter.condvar.notify_one();
        }
    }

    /// Takes one particular waiter back off the latch, if it is still there.
    ///
    /// Used by `wait_on` for its own waiter when that waiter's edge would close a cycle. The
    /// caller holds the graph lock.
    fn remove_waiter(&self, waiter: &Arc<QueryWaiter<'tcx>>) {
        let mut waiters_guard = self.waiters.lock();
        if let Some(waiters) = &mut *waiters_guard {
            waiters.retain(|w| !Arc::ptr_eq(w, waiter));
        }
    }

    /// Removes a single waiter from the list of waiters, hands it `cycle`, and wakes it.
    /// This is used to break query cycles found by `rustc_query_impl::break_query_cycle`.
    ///
    /// Returns whether a thread was asleep on the waiter's condvar, which is what upstream's
    /// `assert!(waiter.condvar.notify_one())` checked. `break_query_cycle` is the deadlock
    /// handler's entry point, meant for a moment when every thread is asleep and so no edge can
    /// change; nothing in the tree calls it now that `wait_on` checks for cycles itself.
    ///
    /// The cycle is stored and `resumed` set before the notify, all under the latch mutex, so
    /// the sleeper can neither miss the wakeup nor wake without its cycle.
    pub fn resume_waiter_with_cycle(&self, waiter: usize, cycle: QueryCycle<'tcx>) -> bool {
        let mut waiters_guard = self.waiters.lock();
        let waiters = waiters_guard.as_mut().expect("non-empty waiters vec");
        // Remove the waiter from the list of waiters
        let waiter = waiters.remove(waiter);
        *waiter.cycle.lock() = Some(cycle);
        waiter.resumed.store(true, Ordering::Relaxed);
        waiter.condvar.notify_one()
    }

    /// Runs `f` on the current waiters of this latch, or on an empty list if the job has
    /// already completed.
    ///
    /// For the cycle search in `rustc_query_impl::job`, which reads the latch edges of every
    /// active job. Called with the graph lock held, so the list cannot change underneath.
    pub fn with_waiters<R>(&self, f: impl FnOnce(&[Arc<QueryWaiter<'tcx>>]) -> R) -> R {
        let waiters_guard = self.waiters.lock();
        match &*waiters_guard {
            Some(waiters) => f(waiters),
            None => f(&[]),
        }
    }
}
