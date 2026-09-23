use core::fmt::Write as _;
use alloc::vec::Vec;
use alloc::string::String;
use core::ops::ControlFlow;
use core::{iter, mem};

use crate::rustc_data_structures::fx::{FxHashMap, FxHashSet};
use crate::rustc_data_structures::hash_table::HashTable;
use crate::rustc_data_structures::sync::{DynSend, DynSync};
use crate::rustc_errors::DiagCtxtHandle;
use crate::rustc_middle::queries::TaggedQueryKey;
use crate::rustc_middle::query::{
    ActiveKeyStatus, QueryCache, QueryCycle, QueryJob, QueryJobId, QueryKey, QueryLatch,
    QueryStackFrame, QueryVTable,
};
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_span::{DUMMY_SP, Span};
use tracing::debug;

use crate::rustc_query_impl::query_vtables::for_each_query_vtable;

/// Map from query job IDs to job information collected by
/// `collect_active_query_jobs`.
#[derive(Debug, Default)]
pub struct QueryJobMap<'tcx> {
    map: FxHashMap<QueryJobId, QueryJobInfo<'tcx>>,
}

impl<'tcx> QueryJobMap<'tcx> {
    /// Adds information about a job ID to the job map.
    ///
    /// Should only be called by `collect_active_query_jobs_inner`.
    pub(crate) fn insert(&mut self, id: QueryJobId, info: QueryJobInfo<'tcx>) {
        self.map.insert(id, info);
    }

    fn tagged_key_of(&self, id: QueryJobId) -> TaggedQueryKey<'tcx> {
        self.map[&id].tagged_key
    }

    fn latch_of(&self, id: QueryJobId) -> Option<&QueryLatch<'tcx>> {
        self.map[&id].job.latch.as_ref()
    }
}

#[derive(Debug)]
pub(crate) struct QueryJobInfo<'tcx> {
    pub(crate) tagged_key: TaggedQueryKey<'tcx>,
    pub(crate) job: QueryJob<'tcx>,
}

#[derive(Clone, Copy)]
pub enum CollectActiveJobsKind {
    /// We need the full query job map, and we are willing to wait to obtain the query state
    /// shard lock(s).
    Full,

    /// We need the full query job map, and we shouldn't need to wait to obtain the shard lock(s),
    /// because we are in a place where nothing else could hold the shard lock(s).
    FullNoContention,

    /// We can get by without the full query job map, so we won't bother waiting to obtain the
    /// shard lock(s) if they're not already unlocked.
    PartialAllowed,
}

/// Returns a map of currently active query jobs, collected from all queries.
pub fn collect_active_query_jobs<'tcx>(
    tcx: TyCtxt<'tcx>,
    collect_kind: CollectActiveJobsKind,
) -> QueryJobMap<'tcx> {
    let mut job_map = QueryJobMap::default();

    for_each_query_vtable!(ALL, tcx, |query| {
        collect_active_query_jobs_inner(query, collect_kind, &mut job_map);
    });

    job_map
}

/// Internal plumbing for collecting the set of active jobs for this query.
///
/// Aborts if jobs can't be gathered as specified by `collect_kind`.
fn collect_active_query_jobs_inner<'tcx, C>(
    query: &'tcx QueryVTable<'tcx, C>,
    collect_kind: CollectActiveJobsKind,
    job_map: &mut QueryJobMap<'tcx>,
) where
    C: QueryCache<Key: QueryKey + DynSend + DynSync>,
    QueryVTable<'tcx, C>: DynSync,
{
    let mut collect_shard_jobs = |shard: &HashTable<(C::Key, ActiveKeyStatus<'tcx>)>| {
        for (key, status) in shard.iter() {
            if let ActiveKeyStatus::Started(job) = status {
                // It's fine to call `create_tagged_key` with the shard locked,
                // because it's just a `TaggedQueryKey` variant constructor.
                let tagged_key = (query.create_tagged_key)(*key);
                job_map.insert(job.id, QueryJobInfo { tagged_key, job: job.clone() });
            }
        }
    };

    match collect_kind {
        CollectActiveJobsKind::Full => {
            for shard in query.state.active.lock_shards() {
                collect_shard_jobs(&shard);
            }
        }
        CollectActiveJobsKind::FullNoContention => {
            for shard in query.state.active.try_lock_shards() {
                match shard {
                    Some(shard) => collect_shard_jobs(&shard),
                    None => panic!("Failed to collect active jobs for query `{}`!", query.name),
                }
            }
        }
        CollectActiveJobsKind::PartialAllowed => {
            for shard in query.state.active.try_lock_shards() {
                match shard {
                    Some(shard) => collect_shard_jobs(&shard),
                    // This collection is best-effort (it is only used to print the query
                    // stack on panic), so a contended shard is expected and fine to skip.
                    // Emitting this at `warn!` would leak nondeterministically into the
                    // panic output under the parallel front-end, where another thread may
                    // still hold a shard lock, so keep it at `debug!`.
                    None => debug!("Failed to collect active jobs for query `{}`!", query.name),
                }
            }
        }
    }
}

pub(crate) fn find_cycle_in_stack<'tcx>(
    id: QueryJobId,
    job_map: QueryJobMap<'tcx>,
    current_job: &Option<QueryJobId>,
    span: Span,
) -> QueryCycle<'tcx> {
    // Find the waitee amongst `current_job` parents.
    let mut frames = Vec::new();
    let mut current_job = Option::clone(current_job);

    while let Some(job) = current_job {
        let info = &job_map.map[&job];
        frames.push(QueryStackFrame { span: info.job.span, tagged_key: info.tagged_key });

        if job == id {
            frames.reverse();

            // This is the end of the cycle. The span entry we included was for
            // the usage of the cycle itself, and not part of the cycle.
            // Replace it with the span which caused the cycle to form.
            frames[0].span = span;
            // Find out why the cycle itself was used.
            // `Option::map` in place of an unstable `try {}` block.
            let usage = info.job.parent.map(|parent| QueryStackFrame {
                span: info.job.span,
                tagged_key: job_map.tagged_key_of(parent),
            });
            return QueryCycle { usage, frames };
        }

        current_job = info.job.parent;
    }

    panic!("did not find a cycle")
}

/// Finds the query job closest to the root that is for the same query method as `id`
/// (but not necessarily the same query key), and returns information about it.
#[cold]
#[inline(never)]
pub(crate) fn find_dep_kind_root<'tcx>(
    tcx: TyCtxt<'tcx>,
    id: QueryJobId,
    job_map: QueryJobMap<'tcx>,
) -> (Span, String, usize) {
    let mut depth = 1;
    let mut info = &job_map.map[&id];
    // Two query jobs are for the same query method if they have the same
    // `TaggedQueryKey` discriminant.
    let expected_query = mem::discriminant::<TaggedQueryKey<'tcx>>(&info.tagged_key);
    let mut last_info = info;

    while let Some(id) = info.job.parent {
        info = &job_map.map[&id];
        if mem::discriminant(&info.tagged_key) == expected_query {
            depth += 1;
            last_info = info;
        }
    }
    (last_info.job.span, last_info.tagged_key.description(tcx), depth)
}

/// The locaton of a resumable waiter. The usize is the index into waiters in the query's latch.
/// We'll use this to remove the waiter using `QueryLatch::resume_waiter_with_cycle` if we're
/// waking it up.
type ResumableWaiterLocation = (QueryJobId, usize);

/// This abstracts over non-resumable waiters which are found in `QueryJob`'s `parent` field
/// and resumable waiters are in `latch` field.
struct AbstractedWaiter {
    /// The span corresponding to the reason for why we're waiting on this query.
    span: Span,
    /// The query which we are waiting from, if none the waiter is from a compiler root.
    parent: Option<QueryJobId>,
    resumable: Option<ResumableWaiterLocation>,
}

/// Returns all the non-resumable and resumable waiters of a query.
/// This is used so we can uniformly loop over both non-resumable and resumable waiters.
///
/// **A job missing from the map has no waiters.** Upstream only walked the map from the
/// deadlock handler, when every thread was asleep and the map could not change, so every id it
/// met was in it. The wait-time check in [`find_cycle_closed_by_wait`] walks a snapshot while
/// running threads keep starting and finishing jobs, so an id reached through a `parent` link
/// can be absent: a job that finished after its child was read, or one that started after its
/// shard was read. Neither can be on a cycle - a job on a cycle belongs to a sleeping thread,
/// whose stack was in place before the snapshot and cannot change during it - so answering
/// "nothing waits on it" is exact for the question being asked.
fn abstracted_waiters_of(job_map: &QueryJobMap<'_>, query: QueryJobId) -> Vec<AbstractedWaiter> {
    let mut result = Vec::new();

    let Some(info) = job_map.map.get(&query) else {
        return result;
    };

    // Add the parent which is a non-resumable waiter since it's on the same stack
    result.push(AbstractedWaiter {
        span: info.job.span,
        parent: info.job.parent,
        resumable: None,
    });

    // Add the explicit waiters which use condvars and are resumable. A latch that has
    // completed reads as an empty list rather than panicking, for the same reason as above.
    if let Some(latch) = info.job.latch.as_ref() {
        latch.with_waiters(|waiters| {
            for (i, waiter) in waiters.iter().enumerate() {
                result.push(AbstractedWaiter {
                    span: waiter.span,
                    parent: waiter.parent,
                    resumable: Some((query, i)),
                });
            }
        });
    }

    result
}

/// Looks for a query cycle by doing a depth first search starting at `query`.
/// `span` is the reason for the `query` to execute. This is initially DUMMY_SP.
/// If a cycle is detected, this initial value is replaced with the span causing
/// the cycle. `stack` will contain just the cycle on return if detected.
fn find_cycle<'tcx>(
    job_map: &QueryJobMap<'tcx>,
    query: QueryJobId,
    span: Span,
    stack: &mut Vec<(Span, QueryJobId)>,
    visited: &mut FxHashSet<QueryJobId>,
) -> ControlFlow<Option<ResumableWaiterLocation>> {
    if !visited.insert(query) {
        return if let Some(pos) = stack.iter().position(|q| q.1 == query) {
            // We detected a query cycle, fix up the initial span and return Some

            // Remove previous stack entries
            stack.drain(0..pos);
            // Replace the span for the first query with the cycle cause
            stack[0].0 = span;
            ControlFlow::Break(None)
        } else {
            ControlFlow::Continue(())
        };
    }

    // Query marked as visited is added it to the stack
    stack.push((span, query));

    // Visit all the waiters
    for abstracted_waiter in abstracted_waiters_of(job_map, query) {
        let Some(parent) = abstracted_waiter.parent else {
            // Skip waiters which are not queries
            continue;
        };
        if let ControlFlow::Break(maybe_resumable) =
            find_cycle(job_map, parent, abstracted_waiter.span, stack, visited)
        {
            // Return the resumable waiter in `waiter.resumable` if present
            return ControlFlow::Break(abstracted_waiter.resumable.or(maybe_resumable));
        }
    }

    // Remove the entry in our stack since we didn't find a cycle
    stack.pop();

    ControlFlow::Continue(())
}

/// Finds out if there's a path to the compiler root (aka. code which isn't in a query)
/// from `query` without going through any of the queries in `visited`.
/// This is achieved with a depth first search.
fn connected_to_root<'tcx>(
    job_map: &QueryJobMap<'tcx>,
    query: QueryJobId,
    visited: &mut FxHashSet<QueryJobId>,
) -> bool {
    // We already visited this or we're deliberately ignoring it
    if !visited.insert(query) {
        return false;
    }

    // Visit all the waiters
    for abstracted_waiter in abstracted_waiters_of(job_map, query) {
        match abstracted_waiter.parent {
            // This query is connected to the root
            None => return true,
            Some(parent) => {
                if connected_to_root(job_map, parent, visited) {
                    return true;
                }
            }
        }
    }

    false
}

/// Processes a found query cycle into a `Cycle`
fn process_cycle<'tcx>(
    job_map: &QueryJobMap<'tcx>,
    stack: Vec<(Span, QueryJobId)>,
) -> QueryCycle<'tcx> {
    // The stack is a vector of pairs of spans and queries; reverse it so that
    // the earlier entries require later entries
    let (mut spans, queries): (Vec<_>, Vec<_>) = stack.into_iter().rev().unzip();

    // Shift the spans so that queries are matched with the span for their waitee
    spans.rotate_right(1);

    // Zip them back together
    let mut stack: Vec<_> = iter::zip(spans, queries).collect();

    struct EntryPoint {
        query_in_cycle: QueryJobId,
        query_waiting_on_cycle: Option<(Span, QueryJobId)>,
    }

    // Find the queries in the cycle which are
    // connected to queries outside the cycle
    let entry_points = stack
        .iter()
        .filter_map(|&(_, query_in_cycle)| {
            let mut entrypoint = false;
            let mut query_waiting_on_cycle = None;

            // Find a direct waiter who leads to the root
            for abstracted_waiter in abstracted_waiters_of(job_map, query_in_cycle) {
                let Some(parent) = abstracted_waiter.parent else {
                    // The query in the cycle is directly connected to root.
                    entrypoint = true;
                    continue;
                };

                // Mark all the other queries in the cycle as already visited,
                // so paths to the root through the cycle itself won't count.
                let mut visited = FxHashSet::from_iter(stack.iter().map(|q| q.1));

                if connected_to_root(job_map, parent, &mut visited) {
                    query_waiting_on_cycle = Some((abstracted_waiter.span, parent));
                    entrypoint = true;
                    break;
                }
            }

            entrypoint.then_some(EntryPoint { query_in_cycle, query_waiting_on_cycle })
        })
        .collect::<Vec<EntryPoint>>();

    // Pick an entry point, preferring ones with waiters
    //
    // `first()` rather than `[0]`: under the deadlock handler some query in a cycle always led
    // to the root, because the map held every thread's whole stack. The wait-time check sees a
    // snapshot in which an ancestor on a *running* thread may be missing (see
    // `abstracted_waiters_of`), and if every path out of the cycle went through one, there is no
    // entry point. The cycle is no less real; it is reported as found, without a usage frame.
    let entry_point = entry_points
        .iter()
        .find(|entry_point| entry_point.query_waiting_on_cycle.is_some())
        .or(entry_points.first());

    // Shift the stack so that our entry point is first
    if let Some(entry_point) = entry_point {
        let entry_point_pos =
            stack.iter().position(|(_, query)| *query == entry_point.query_in_cycle);
        if let Some(pos) = entry_point_pos {
            stack.rotate_left(pos);
        }
    }

    let usage = entry_point
        .and_then(|entry_point| entry_point.query_waiting_on_cycle)
        .map(|(span, job)| QueryStackFrame { span, tagged_key: job_map.tagged_key_of(job) });

    // Create the cycle error
    QueryCycle {
        usage,
        frames: stack
            .iter()
            .map(|&(span, job)| QueryStackFrame { span, tagged_key: job_map.tagged_key_of(job) })
            .collect(),
    }
}

/// Looks for a query cycle starting at `query`, and if one is found, resumes one waiter on it
/// with the cycle error.
///
/// Returns `None` if there is no cycle through `query`, and otherwise whether the resumed
/// waiter's thread was asleep (see `QueryLatch::resume_waiter_with_cycle`).
fn find_and_process_cycle<'tcx>(job_map: &QueryJobMap<'tcx>, query: QueryJobId) -> Option<bool> {
    let mut visited = FxHashSet::default();
    let mut stack = Vec::new();
    if let ControlFlow::Break(resumable) =
        find_cycle(job_map, query, DUMMY_SP, &mut stack, &mut visited)
    {
        // Create the cycle error
        let error = process_cycle(job_map, stack);

        // We unwrap `resumable` here since there must always be one
        // edge which is resumable / waited using a query latch
        let (waitee_query, waiter_idx) = resumable.unwrap();

        // Take the waiter off its latch, give it the cycle error and wake it. These are one
        // call so that the three happen under the latch mutex the waiter sleeps with: see
        // `QueryWaiter::resumed`.
        Some(job_map.latch_of(waitee_query).unwrap().resume_waiter_with_cycle(waiter_idx, error))
    } else {
        None
    }
}

/// Detects query cycles by using depth first search over all active query jobs.
/// If a query cycle is found it will break the cycle by finding an edge which
/// uses a query latch and then resuming that waiter.
///
/// There may be multiple cycles involved in a deadlock, but this only breaks one at a time so
/// there will be multiple rounds through the deadlock handler if multiple cycles are present.
///
/// **Nothing calls this now.** It was the deadlock handler's body, run when every thread was
/// asleep. Cycles are now caught before they form, by [`find_cycle_closed_by_wait`] from
/// `QueryLatch::wait_on`, so a deadlock handler would never find one. It is kept, working,
/// for a driver that wants a last-resort check.
pub fn break_query_cycle<'tcx>(job_map: QueryJobMap<'tcx>) {
    // Look for a cycle starting at each query job
    let woke = job_map
        .map
        .keys()
        .find_map(|query| find_and_process_cycle(&job_map, *query))
        .expect("unable to find a query cycle");

    assert!(woke, "unable to wake the waiter");
}

/// Answers, for a thread about to sleep on `waitee`'s latch, whether that sleep would close a
/// query cycle, and if so returns the cycle to report.
///
/// Called only from `QueryLatch::wait_on`, with the session's `QueryWaitGraph` lock held and
/// the caller's waiter already pushed onto `waitee`'s latch. The new edge is therefore in the
/// graph, and a cycle through it is a cycle through `waitee`: [`find_cycle`] walks from
/// `waitee` along "who waits on this" edges (stack parents and latch waiters) and reports
/// reaching `waitee` again.
///
/// # Why a cycle found here is real, and a real one is always found
///
/// - Every latch edge the walk sees belongs to a thread that is asleep, because adding and
///   removing latch edges both need the lock this thread holds. A sleeping thread's stack
///   cannot change, so every stack edge between two latch edges on a cycle is current too. A
///   cycle is made of latch edges and the stack edges between them (stack edges alone are a
///   tree), so everything on a found cycle is current: it is a real deadlock.
/// - Conversely, every other edge of a real cycle belongs to a thread that went to sleep
///   earlier, under this same lock, and whose check found nothing. So the graph never holds a
///   cycle between checks, a new cycle must use the new edge, and all of its other edges are
///   visible to this snapshot.
///
/// Jobs on running threads may be half-seen (see `abstracted_waiters_of`); they cannot be on a
/// cycle, which is the only thing asked.
///
/// # Cost
///
/// A full snapshot of every query's active jobs, which is every shard of every query state. It
/// is paid only when a thread is about to block on another thread's query, which is rare next
/// to the query calls that hit the cache or run their own provider. The shard locks are taken
/// one at a time, and no thread holding a shard lock ever waits for the graph lock, so this
/// cannot deadlock against a thread that is starting or finishing a job.
pub(crate) fn find_cycle_closed_by_wait<'tcx>(
    tcx: TyCtxt<'tcx>,
    waitee: QueryJobId,
) -> Option<QueryCycle<'tcx>> {
    // `Full`: wait for contended shards rather than skip them. A skipped shard could hide a
    // sleeping thread's job and with it the cycle.
    let job_map = collect_active_query_jobs(tcx, CollectActiveJobsKind::Full);

    let mut visited = FxHashSet::default();
    let mut stack = Vec::new();
    match find_cycle(&job_map, waitee, DUMMY_SP, &mut stack, &mut visited) {
        ControlFlow::Break(_) => {
            // The graph held no cycle before this edge, so the one found runs through it and
            // through `waitee`.
            debug_assert!(stack.iter().any(|&(_, query)| query == waitee));
            Some(process_cycle(&job_map, stack))
        }
        ControlFlow::Continue(()) => None,
    }
}

pub fn print_query_stack<'tcx>(
    tcx: TyCtxt<'tcx>,
    mut current_query: Option<QueryJobId>,
    dcx: DiagCtxtHandle<'_>,
    limit_frames: Option<usize>,
    mut file: Option<eko::file::File>,
) -> usize {
    // Be careful relying on global state here: this code is called from
    // a panic hook, which means that the global `DiagCtxt` may be in a weird
    // state if it was responsible for triggering the panic.
    let mut count_printed = 0;
    let mut count_total = 0;

    // Make use of a partial query job map if we fail to take locks collecting active queries.
    let job_map = collect_active_query_jobs(tcx, CollectActiveJobsKind::PartialAllowed);

    if let Some(ref mut file) = file {
        let _ = writeln!(file, "\n\nquery stack during panic:");
    }
    while let Some(query) = current_query {
        let Some(query_info) = job_map.map.get(&query) else {
            break;
        };
        let description = query_info.tagged_key.description(tcx);
        if Some(count_printed) < limit_frames || limit_frames.is_none() {
            // Only print to stderr as many stack frames as `num_frames` when present.
            dcx.struct_failure_note(format!(
                "#{count_printed} [{query_name}] {description}",
                query_name = query_info.tagged_key.query_name(),
            ))
            .with_span(query_info.job.span)
            .emit();
            count_printed += 1;
        }

        if let Some(ref mut file) = file {
            let _ = writeln!(
                file,
                "#{count_total} [{query_name}] {description}",
                query_name = query_info.tagged_key.query_name(),
            );
        }

        current_query = query_info.job.parent;
        count_total += 1;
    }

    if let Some(ref mut file) = file {
        let _ = writeln!(file, "end of query stack");
    }
    count_total
}
