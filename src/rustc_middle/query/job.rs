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
use alloc::sync::Arc;

// `parking_lot_lite_hack` has no `Condvar`: upstream's is built on `parking_lot_core`'s thread
// parking, which is the part the fork dropped. The one in `eko::thread` is a
// `pthread_cond_t`, so the latch below takes its `Mutex` too - a condvar and the lock it releases
// have to be the same pair.
use eko::thread::{Condvar, Mutex as WaitMutex};
use parking_lot::Mutex;
use crate::rustc_data_structures::hash_table::HashTable;
use crate::rustc_data_structures::sharded::Sharded;
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
}

impl<'tcx> QueryJob<'tcx> {
    /// Creates a new query job.
    #[inline]
    pub fn new(id: QueryJobId, span: Span, parent: Option<QueryJobId>) -> Self {
        QueryJob { id, span, parent, latch: None }
    }

    pub fn latch(&mut self) -> QueryLatch<'tcx> {
        self.latch.get_or_insert_with(QueryLatch::new).clone()
    }

    /// Signals to waiters that the query is complete.
    ///
    /// This does nothing for single threaded rustc,
    /// as there are no concurrent jobs which could be waiting on us
    #[inline]
    pub fn signal_complete(self) {
        if let Some(latch) = self.latch {
            latch.set();
        }
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
    Poisoned,
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
    pub fn wait_on(&self, query: Option<QueryJobId>, span: Span) -> Result<(), QueryCycle<'tcx>> {
        let mut waiters_guard = self.waiters.lock();
        let Some(waiters) = &mut *waiters_guard else {
            return Ok(()); // already complete
        };

        let waiter = Arc::new(QueryWaiter {
            parent: query,
            span,
            cycle: Mutex::new(None),
            condvar: Condvar::new(),
        });

        // We push the waiter on to the `waiters` list. It can be accessed inside
        // the `wait` call below, by 1) the `set` method or 2) by deadlock detection.
        // Both of these will remove it from the `waiters` list before resuming
        // this thread.
        waiters.push(Arc::clone(&waiter));

        // Awaits the caller on this latch by blocking the current thread.
        // If this detects a deadlock and the deadlock handler wants to resume this thread
        // we have to be in the `wait` call. This is ensured by the deadlock handler
        // getting the self.info lock.
        // **Nothing to tell.** This was `rustc_thread_pool::mark_blocked_and_wait`, which
        // released the worker thread, armed the pool's deadlock handler, ran this wait, then
        // re-acquired. That existed because a *pool worker* blocking here could deadlock the
        // pool. With no pool there is no worker to release and no handler to arm, and with the
        // frontend serial inside one request there is no second thread to be waited on either -
        // a query cycle here is a real cycle, which is what `waiter.cycle` already reports.
        // `wait` takes the guard and hands it back, which is `std::sync::Condvar::wait`'s shape:
        // the lock is held on the way in, released while waiting, and held again on return.
        let waiters_guard = waiter.condvar.wait(waiters_guard);
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
    fn set(&self) {
        let mut waiters_guard = self.waiters.lock();
        let waiters = waiters_guard.take().unwrap(); // mark the latch as complete
        for waiter in waiters {
            waiter.condvar.notify_one();
        }
    }

    /// Removes a single waiter from the list of waiters.
    /// This is used to break query cycles.
    pub fn extract_waiter(&self, waiter: usize) -> Arc<QueryWaiter<'tcx>> {
        let mut waiters_guard = self.waiters.lock();
        let waiters = waiters_guard.as_mut().expect("non-empty waiters vec");
        // Remove the waiter from the list of waiters
        waiters.remove(waiter)
    }
}
