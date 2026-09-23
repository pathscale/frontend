# Parallel audit: query system and shared compiler state

Scope: what the internal `par_*` loops (`par_hir_body_owners`, `par_hir_for_each_module`,
`par_fns`, `par_join`) touch when their pieces really run on nagoya workers, with the caller's
`SESSION_GLOBALS` and `ImplicitCtxt` installed on each worker and parallel mode meaning
`sync::is_dyn_thread_safe()` is true. Written for the owner to review. Nothing here was built or
run: every change is source only, and the verify command is at the end.

## The owner's rules, and how each verdict below uses them

Data forward, zero copy, reactive. For each piece of shared mutable state the parallel passes
touch, the preferred verdicts are, in order:

1. **Freeze**: compute it before the parallel stage and share it read-only (by `Arc` or `&'tcx`
   to a value that never changes again).
2. **Per item**: make it the output of each piece, returned as data and merged in order.
3. **Delete**: it is a cache that exists to shave single-thread time.
4. **Lock**: last resort, with a sentence on why 1 to 3 do not apply.

Plus: no `Rc` anywhere a worker runs; no copying to make something shareable.

Verdict words used in the table: **Safe** (already one of the above, nothing to do), **Fixed**
(changed in this pass, how is said), **Kept, lock** (rule 4, reason given), **Hazard** (left, with
why and who owns it).

## How `Lock` picks its implementation

`src/rustc_data_structures/sync/lock.rs:106` decides once, at construction, from
`mode::might_be_dyn_thread_safe()`, which is true while the process-wide mode is
`UNINITIALIZED` or `DYN_THREAD_SAFE`. So:

- A `Lock` created **before** the mode is set is the real mutex (`Mode::Sync`,
  `parking_lot_lite_hack::RawMutex`, which parks threads for real).
- A `Lock` created after the mode is set to parallel is the real mutex.
- Only a `Lock` created after the mode was set to **serial** is the `Cell<bool>` kind, and the mode
  is set-once (`sync.rs` `set_dyn_thread_safe_mode` asserts on a change), so such a lock can
  never be used by a second thread in the same process.

The one type that does not follow that rule is `Sharded`
(`src/rustc_data_structures/sharded.rs:44`): it calls `is_dyn_thread_safe()`, which **panics**
while the mode is uninitialized, and picks `Single` (one `Lock`, then locked with
`lock_assume(Mode::NoSync)`) when the mode is serial. Every query state, query cache, interner
and `AllocMap::to_alloc` is a `Sharded`. Therefore:

- **The mode must be set before `GlobalCtxt` construction** (`query_system()` builds every
  `QueryState` and cache; `CtxtInterners::new` builds the interners). If it is not, the process
  panics there, which is loud and safe. This is the parallel-mode agent's `set from
  opts.jobs.frontend` step; it must happen before `create_global_ctxt`.
- Because the mode is process-wide and set-once, a process that ever ran a serial session cannot
  later run a parallel one (the assert fires). Owned by the `sync.rs` mode section.

Locks created at `GlobalCtxt` construction (`GlobalCaches`, `AllocMap::dedup`,
`QuerySystem::side_effects`, `used_features`) and at `Session` construction are therefore the
thread-safe kind whenever the session is parallel. Nothing needed changing in `lock.rs`.

## Task 1: query waits, cycles and poisoning

### What was wrong

- `QueryLatch::wait_on` slept once on an `eko::thread::Condvar` (a `pthread_cond_t`). Upstream
  slept on `parking_lot::Condvar`, which never wakes spuriously; a pthread condvar may, and a
  spurious return was read as "the job is done", so the waiter would look the value up, miss,
  and report a poisoned query that was not poisoned.
- With the deadlock handler gone, a cross-thread cycle (A sleeps on B's job, B sleeps on A's)
  slept forever. So did a same-thread cycle in parallel mode: `try_execute_query` takes the
  latch path for any `Started` job when `is_dyn_thread_safe()`, including an ancestor on the
  caller's own stack.
- `QuerySystem::cycle_handler_nesting` was one `Lock<u8>` per session, so two threads handling
  unrelated cycles at once looked "nested", and three looked "doubly nested", which is fatal.
- The "check the cache again under the shard lock" guard in `try_execute_query` was keyed on
  `opts.jobs.frontend.is_some()` rather than on the mode.

### Design: detect the cycle at the moment the closing edge is added

Wait graph: job J waits for job K by a **stack edge** (K's `parent` is J, including a `par_*`
piece whose parent is the caller's query blocked in the join) or a **latch edge** (a
`QueryWaiter` with `parent == J` on K's latch). Stack edges alone form a tree (a child's id is
larger than its parent's), so every cycle contains a latch edge, and the last latch edge added
closes it.

`QueryWaitGraph` (`src/rustc_middle/query/job.rs:122`, one per session in
`QuerySystem::wait_graph`, `src/rustc_middle/query/system.rs:150`) is an `eko::thread::Mutex<()>`
that serialises every change to latch edges:

- `QueryLatch::wait_on` (`job.rs:235`): take the graph lock; push this thread's waiter onto K's
  latch (or return at once if K already completed); if this thread is inside a query, call
  `find_cycle_closed_by_wait` (`src/rustc_query_impl/job.rs:495`); on a cycle, take the waiter
  back off and return `Err(cycle)` without sleeping; otherwise release the graph lock and sleep on
  the latch mutex in a loop until `QueryWaiter::resumed` (`job.rs:209`) is true.
- `QueryLatch::set` (`job.rs:319`): takes the graph lock, then the latch mutex, sets `resumed` on
  every waiter and notifies it.
- `find_cycle_closed_by_wait`: a full snapshot of active jobs (`CollectActiveJobsKind::Full`),
  then upstream's `find_cycle` starting at K, then upstream's `process_cycle` to shape the error.

Why it is race-free:

- While the checking thread holds the graph lock, no latch edge can be added or removed, and every
  latch edge it sees belongs to a thread that is asleep. A sleeping thread's stack cannot change,
  so the stack edges between two latch edges on a cycle are current as well. A found cycle is
  made only of current edges: it is a real deadlock.
- Every earlier edge was added under the same lock by a thread whose own check found nothing, so
  the graph never holds a cycle between checks. A new cycle must use the new edge, and all of its
  other edges belong to sleeping threads that are in the snapshot. A real cycle is always found.
- Jobs on running threads can be half-seen in the snapshot (a parent read after it finished, a
  job started after its shard was read). They cannot be on a cycle.
  `abstracted_waiters_of` (`src/rustc_query_impl/job.rs:221`) now treats an id missing from the
  map as "nothing waits on it" and a completed latch as an empty waiter list instead of panicking,
  and `process_cycle` (`job.rs:330`) no longer indexes `entry_points[0]` blindly.
- No lost wakeup: `resumed` is set under the latch mutex the sleeper waits with, before the
  notify.
- Lock order: graph lock, then a query-state shard or a latch mutex, one at a time. Nothing that
  holds a shard or a latch mutex ever takes the graph lock (`drop_and_maybe_poison` releases the
  shard before `signal_complete`; the latch is created under the shard lock but that takes no
  other lock).

Which waiter gets the error: always the thread that closed the cycle, which is also what the
serial path does. So no other thread is ever woken by the check, and upstream's
`break_query_cycle` is no longer called; it is kept working (now through
`QueryLatch::resume_waiter_with_cycle`) for a driver that wants a last-resort check.

Serial path: unchanged. `try_execute_query` still goes to `find_and_handle_cycle` when
`!is_dyn_thread_safe()`, and a latch is only ever created on the parallel branch, so
`signal_complete` and the graph lock are never reached in serial mode.

Cost: the snapshot locks every shard of every query state, once per wait. It is paid only when a
thread is about to sleep on another thread's job. The copies it makes are one `QueryJob` per
active job (an id, a span, a parent id and an `Arc` clone of the latch); nothing deep.

### Poisoning

`ActiveJobGuard::drop` (`src/rustc_query_impl/execution.rs:159`) runs while a computing thread
unwinds (a `FatalError` or an ICE), replaces the job with `Poisoned` and calls
`signal_complete`, which sets the latch. A waiter wakes, misses the cache, finds `Poisoned` and
raises `FatalError` itself. This was already right; it now also takes the graph lock, after the
shard lock is released. It depends on unwinding being enabled (`unwind_janky` with an installed
catcher); under `panic = "abort"` there is nothing to wake.

### Minimal, given the data-forward direction

With a serial pre-pass that forces the item-level queries before the parallel stage (see
"Serial pre-pass" below), cross-worker waits become rare, but they do not become impossible:
typeck of a closure shares its root's `typeck`; `type_of` on an opaque type runs `typeck` of its
defining bodies; a const item's value runs `mir_built` and const eval of another body. So the
check stays, as a safety net whose cost is paid only on an actual wait. It is not the mechanism
the parallel stages rely on.

## Task 2: shared state, site by site

| Site | Where | Touched in parallel by | Verdict |
| --- | --- | --- | --- |
| Query latch wait | `src/rustc_middle/query/job.rs:235` | any query hit while another worker computes it | **Fixed**: spurious-wake loop on `QueryWaiter::resumed`; cycle check before sleeping (above). |
| Query latch set | `src/rustc_middle/query/job.rs:319` | job completion and poisoning | **Fixed**: under the wait-graph lock; sets `resumed` before notify. |
| `QueryWaitGraph` | `src/rustc_middle/query/job.rs:122`, `system.rs:150` | every latch wait | **Kept, lock**: the one lock this design adds. Freezing or per-item output cannot describe "who is asleep on whom right now", and without the lock two threads can each add half of one cycle unseen. Taken only on a wait or on completing a job somebody waited on. |
| `QuerySystem::cycle_handler_nesting` | was `system.rs`, now `execution.rs:44` | cycle handlers on several threads | **Fixed**: per-thread `ThreadLocal<u8>`; nesting is a property of a stack. |
| Cache re-check under shard lock | `src/rustc_query_impl/execution.rs:278` | every query miss | **Fixed**: keyed on `sync::is_dyn_thread_safe()` instead of `opts.jobs.frontend`. |
| `QueryState::active` | `src/rustc_middle/query/job.rs:159` | every query miss | **Kept, lock** (`Sharded`, 32 shards in parallel mode): it is the "who is computing this key" table, the thing that makes a key computed once; it cannot be frozen before the stage because the stage is what fills it. |
| `QuerySystem::jobs` | `system.rs:140` | job id allocation | **Safe**: `AtomicU64`, `Relaxed` is enough for unique ids. |
| `DefaultCache` | `src/rustc_middle/query/caches.rs:54` | query results | **Kept, lock** (`ShardedHashMap`): the query caches are the memoization model and the owner keeps them. They are fill-once per key already (see "Reactive"). |
| `SingleCache` | `caches.rs:97` | `()`-keyed queries | **Safe**: `eko::thread::OnceLock`, a fill-once slot. Only the computing thread calls `set`, because the active-key table admits one. |
| `VecCache` (`DefIdCache::local`) | `caches.rs:141`, `src/rustc_data_structures/vec_cache.rs:313` | local `DefId` queries | **Safe**: upstream's lock-free atomic slots; bucket allocation under a static `eko` mutex (`vec_cache.rs:126`). |
| `QuerySystem::arenas` | `system.rs:115` | `arena_cache` queries | **Safe** if every nagoya worker is registered in the session's `Registry` (per-worker arenas, `WorkerLocal`). **Hazard, owned by the parallel-mode agent**: `WorkerLocal::deref` panics on an unregistered thread (`worker_local.rs` `verify`). |
| `QuerySystem::side_effects`, `used_features` | `system.rs:124`, `system.rs:129` | incremental side effects; feature use tracking | **Kept, lock**: side effects are empty with the dep graph disabled; `used_features` is a set union whose order does not matter. Could become per-item output merged after the stage if it ever shows up in a profile. |
| `on_disk_cache` | `system.rs:135` | none | **Safe**: always `None` here (dep graph disabled). |
| Query feeding | `src/rustc_middle/query/calls.rs` `query_feed` | `TyCtxtFeed` | **Safe**: a key is fed by the one query that creates its def; same as upstream parallel. |
| `CtxtInterners` (all `InternedSet`s) | `src/rustc_middle/ty/context.rs:144` | every type, const, list created in typeck/borrowck/MIR build | **Kept, lock** (`ShardedHashMap`): interning is identity-by-address across the whole `TyCtxt`, so it cannot be per item (two workers creating `Vec<u8>` must get the same pointer) and cannot be frozen (typeck creates new types). Values are allocated in the calling worker's arena. |
| `GlobalCtxt::arena`, `hir_arena` | `context.rs:740`, `context.rs:741` | interning, `arena.alloc` everywhere | **Safe** under the same `Registry` condition as `QuerySystem::arenas`: `WorkerLocal<Arena>`, one `DroplessArena`/`TypedArena` set per worker, never shared. Allocations live until the `GlobalCtxt` drops. |
| `GlobalCaches` (selection, evaluation, new-solver, param-env, clauses caches) | `context.rs:672` to `context.rs:695` | trait selection in every body | **Kept, lock** (each a `Lock<FxHashMap>`). These memoize pure functions of interned, tcx-global inputs across bodies, the same category as the query caches; the per-body alternative (the `InferCtxt`-local caches) already exists and is what they sit on top of. Deleting them is an owner call to make with a measurement: cross-body trait evaluation reuse is where they pay. Flagged for review, not changed. |
| `ty_rcache` | `context.rs:674` | metadata decoding | **Safe**: only reached when decoding crate metadata, which this `no_core` frontend does not load by default; a `Lock` if it is. |
| `AllocMap` | `src/rustc_middle/mir/interpret/mod.rs:434`, `:440`, `:444` | const eval in any body | **Kept, lock**: `to_alloc` sharded, `dedup` locked, `next_id` atomic. Ids must be unique across the session; same as upstream. |
| `untracked.definitions`, `cstore`, `stable_crate_ids` | `context.rs:774`, built in `rustc_interface::passes` | `def_path_hash`, `def_key` reads | **Safe**: `FreezeLock`, frozen before analysis (`context.rs` freezes definitions when iteration starts); frozen reads take no lock. |
| `untracked.source_span` | `AppendOnlyIndexVec` | span lookups in hashing | **Safe**: `LockFreeAppendOnlyVec`, lock-free reads, writers serialised. |
| `FreezeLock` | `src/rustc_data_structures/sync/freeze.rs:24` | the above | **Safe**: `RwLock` until frozen, `Acquire`/`Release` on the flag. |
| `AppendOnlyVec`, `LockFreeAppendOnlyVec` | `src/rustc_data_structures/sync/vec.rs:195`, `:62` | `raw_identifier_spans`, `source_span` | **Safe**: reviewed the orderings; `get` is sound for a concurrent `push`. |
| `Steal<T>` | `src/rustc_data_structures/steal.rs:35`, `:67` | `mir_built`, `thir_body`, `mir_promoted` | **Kept, lock**, with a **hazard**: `steal()` uses `try_write` and panics if another thread is reading the value at that moment. Correctness rests on query ordering (every reader is forced before the stealing query runs), the same contract upstream's parallel compiler relies on. Data-forward redesign, not done here: a consuming query that returns an owned `Body` once, instead of mutate-in-place behind a lock. |
| `mir::BasicBlocks` cache | `src/rustc_middle/mir/basic_blocks.rs:37` | predecessors, dominators of shared bodies | **Safe**: `eko::thread::OnceLock` fill-once slots; the contended case yields instead of parking. |
| `DepGraph` | `src/rustc_middle/dep_graph/graph.rs:75` | `next_virtual_depnode_index`, `read_index` | **Safe**: disabled graph, `Arc<AtomicU32>` counter. `green_edge_buf` (`:169`) is `WorkerLocal` and only used when incremental. |
| `CurrentGcx` | `context.rs:821` | `GlobalCtxt::enter` | **Safe**: `Arc<RwLock<Option<*const ()>>>`; set once by the caller thread before the stage. |
| `ImplicitCtxt` TLV | `src/rustc_middle/ty/context/tls.rs:56` | every query | **Safe**, installed per worker by the parallel-mode agent. Note for that agent: the copied context's `query` becomes the `parent` of every job the piece starts, which is exactly the stack edge the cycle check needs; do not clear it. |
| Stable-hash address memos (`RawList`, `AdtDefData`) and `ADDRESS_CACHE_GENERATION` | were `src/rustc_middle/ty/impls_ty.rs`, `src/rustc_middle/ty/adt.rs:161`, `src/rustc_data_structures/stable_hash.rs` | query-key fingerprints in debug builds, `type_id_hash`, writeback sorting | **Fixed (delete, then per computation)**. Two thread-local maps keyed by arena address, retired by a process-wide generation counter bumped per `GlobalCtxt`. With sessions sharing nagoya workers the counter was not enough: a worker could hash a live session's value at an address a dead session's entry still named, with no new context created in between (wrong fingerprint), and every new session emptied every worker's memo (thrash). Deleted the thread-locals, the counter and its two functions. The memo that prevents exponential re-hashing of a shared type DAG is now a field of `StableHashState` (`src/rustc_middle/ich.rs:47`), reached through two defaulted `StableHashCtxt` methods (`stable_hash.rs:76`). It lives for one `with_stable_hashing_context` call, cannot outlive its arena and is never shared. What was given up: reuse of a list's fingerprint across separate hashing calls. |
| `PASS_TO_PROFILER_NAMES` | was `src/rustc_mir_transform/pass_manager.rs` | every MIR pass | **Fixed (delete)**: a per-thread cache of leaked `mir_pass_*` strings for a self-profiler whose label argument is ignored (`profiling.rs` `generic_activity_with_arg`). On workers it leaked one copy per pass per thread. `MirPass::profiler_name` (`pass_manager.rs:125`) now returns the static name. |
| `Rc<DenseLocationMap>` in borrowck | `src/rustc_borrowck/mod.rs:367` and the nine files that pass it | borrowck of each body | **Fixed**: `Arc`. Built once per body, then read only: frozen data handed forward. |
| `UniverseInfo::TypeOp(Rc<dyn TypeOpInfo>)` | `src/rustc_borrowck/diagnostics/bound_region_errors.rs:48` | borrowck diagnostics | **Fixed**: `Arc`. Immutable once built. |
| `ChunkedBitSet` chunk words `Rc<[Word; N]>` | `src/rustc_index/bit_set.rs:500` | dataflow in borrowck and MIR passes | **Hazard, left**: confined to one piece's stack (upstream's `DynSync` bound on query values kept it out of every query result, and nothing here stores one in shared state), so no two workers ever reach one chunk. The `Arc` conversion is mechanical (`Arc::new_zeroed`, `Arc::make_mut`, `Arc::get_mut` all exist) but the type's unit tests in `bit_set/tests.rs` build `Rc` chunks directly and tests are outside this pass's remit. |
| `datafrog` `Rc<RefCell<..>>` | `src/datafrog/mod.rs:283` | legacy Polonius only | **Hazard, left**: single-threaded mutation inside one Polonius computation; `Arc<RefCell>` would claim a shareability it does not have, and the honest change is a per-computation owned struct. Off by default. |
| `lint_tail_expr_drop_order` `Rc<RefCell<MixedBitSet>>` | `src/rustc_mir_transform/lint_tail_expr_drop_order.rs:66` | one lint over one body | **Hazard, left**: same reasoning as `datafrog`; per-body, never escapes. |
| `Cell`/`RefCell` fields in `InferCtxt`, `FnCtxt`, `TypeckRootCtxt`, `SelectionContext`, `MirBorrowckCtxt`, `ItemCtxt`, `CheckAttrVisitor`, `RustcPatCtxt`, interpreter frames | `src/rustc_infer/infer/mod.rs`, `src/rustc_hir_typeck/*`, `src/rustc_trait_selection/traits/select/mod.rs`, `src/rustc_borrowck/mod.rs`, `src/rustc_hir_analysis/collect.rs:146`, `src/rustc_passes/check_attr.rs:123`, `src/rustc_pattern_analysis/rustc.rs:114`, `src/rustc_const_eval/interpret/*` | the body being checked | **Safe (per item)**: each is created inside one query provider or one visitor, on one worker's stack, and dropped before it returns. None is stored in `GlobalCtxt`, a query value or a static. |
| Pretty-printer flags (`NO_TRIMMED_PATH` and eight more) | `src/rustc_middle/ty/print/pretty.rs:43` | diagnostics text | **Safe**, per thread. Note: a flag set by the caller around a `par_*` call is not seen by the workers (they start at the defaults). No current call site does that. |
| `INSIDE_VERIFY_PANIC` | `src/rustc_middle/verify_ich.rs:78` | incremental only | **Safe**: per thread, unreachable with the dep graph disabled. |
| `Session` locks (`mir_dumps`, `ctfe_backtrace`, `miri_unleashed_features`, `env_depinfo`, `file_depinfo`) | `src/rustc_session/session.rs:370` to `:414` | MIR dump, const eval, `env!` tracking | **Kept, lock**: created before the mode is set, so the real mutex. `env_depinfo`/`file_depinfo` are set unions; the rest are debug paths. |
| `Session::mir_opt_bisect_eval_count` | `session.rs:434` | `-Zmir-opt-bisect-limit` | **Safe**: atomic. Under parallelism the bisect order is nondeterministic, which only matters to someone bisecting. |
| `ParseSess` (`GatedSpans`, `SymbolGallery`, `bad_unicode_identifiers`, `buffered_lints`, `AttrIdGenerator`) | `src/rustc_session/parse.rs:37`, `:70`, `src/rustc_ast/attr/mod.rs:49` | parsing and expansion, before analysis | **Safe**: filled before the parallel stages and only read after; locks and an atomic where written. |
| `DiagCtxt` | `src/rustc_errors/mod.rs:303` | every diagnostic | **Hazard, not mine**: thread-safe (`Lock<DiagCtxtInner>`), but emission order follows thread timing, so output order is nondeterministic. The data-forward fix is per-piece diagnostics returned as data and merged in body order; `rustc_errors` and `frontend_facts` are outside this pass. |
| `SessionGlobals` (symbol interner, span interner, hygiene, metavar spans, source map) | `src/rustc_span/mod.rs:137`, `:141`, `:224`, `src/rustc_span/symbol.rs:2835`, `src/rustc_span/source_map.rs:174` | spans and symbols everywhere | **Kept, lock**: interners, same argument as `CtxtInterners`; `MetavarSpansMap` is a `FreezeLock` frozen after expansion. Created before the mode is set, so real mutexes. |
| `SourceFile::lines`, `external_src` | `src/rustc_span/mod.rs` (`FreezeLock`) | line lookups in diagnostics and span hashing | **Safe**: lazily filled then frozen. |
| `TRACK_DIAGNOSTIC`, `TRACK_FEATURE`, `DEF_ID_DEBUG`, `SPAN_TRACK` | `src/rustc_errors/mod.rs:436`, `src/rustc_feature/unstable.rs:20`, `src/rustc_span/def_id.rs:352`, `src/rustc_span/mod.rs:2734` | hooks | **Safe**: `AtomicRef`, set once at startup, read after. |
| `CTRL_C_RECEIVED`, `PAT_ID`, `OPAQUE_ID`, `rustc_transmute` `COUNTER` | `src/rustc_const_eval/mod.rs:86`, `src/rustc_pattern_analysis/pat.rs:19`, `constructor.rs:667`, `src/rustc_transmute/layout/dfa.rs:47` | const eval, match checking, transmute checks | **Safe**: atomics used for flags or unique ids only. |
| `LazyLock` statics (`PASS_NAMES`, `DEFAULT_QUERY_PROVIDERS`, `BUILTIN_ATTRIBUTE_MAP`) and `OnceLock` statics | `src/rustc_mir_transform/mod.rs:118`, `src/rustc_interface/passes.rs:928`, `src/rustc_feature/builtin_attrs.rs:422` | read-only tables | **Safe**: frozen once, read-only after. |
| `hir_id_validator` errors | `src/rustc_passes/hir_id_validator.rs:33` | per-module HIR validation | **Kept, lock** (debug-assertions only). Per-module output merged after would be the data-forward form. |
| `unwind_janky` `RESUMING`, `LAST_PANIC` | `src/unwind_janky/mod.rs:149`, `:193` | a panic on a worker | **Hazard, parallel-mode agent**: per thread now. A panic caught on a worker leaves its message in that worker's `LAST_PANIC`; the catch that re-raises on the caller must carry the payload over, or the caller reports the wrong (or no) message. |
| `rustc_attr_parsing` `STATE_OBJECT` | `src/rustc_attr_parsing/context.rs:130` | attribute parsing | **Safe**: per-thread parser state; attribute parsing runs during lowering, before the parallel stages. |
| `Registry`, `WorkerLocal` | `src/rustc_data_structures/sync/worker_local.rs` | all worker-local arenas | Not mine; see the arena rows. |

## Reactive: where the latch wait could become an await on a fill-once slot

What a query result already is: a fill-once slot. `SingleCache` is literally a `OnceLock`;
`VecCache` slots are written once; `DefaultCache` entries are inserted once and never changed; and
`QueryLatch` is a fill-once event per job (`Some(waiters)` until `set`, then `None` for ever).

What a waiting consumer does today: `QueryLatch::wait_on` parks the OS thread on a condvar. On a
nagoya worker that parks the worker.

What an awaitable slot would be: `QueryWaiter` holding a `Waker` instead of a `Condvar`, `set`
waking it, and the consumer `.await`ing. The obstacle is not the slot but the consumer: a query
provider is a synchronous Rust call, typically dozens of frames deep inside typeck or trait
selection, and a synchronous frame cannot yield. Making the consumer awaitable means making every
provider and everything between it and the query call `async`, which is the whole compiler.

What can be done without that, precisely:

1. **Help while waiting** (the parallel-mode agent's `parallel.rs`): a worker about to sleep on a
   latch or a join first runs other queued `par_*` pieces on its own stack, then re-checks. That is
   what rayon's `mark_blocked_and_wait` bought. It keeps the worker busy.
2. **Forced inputs** (below): if the stage's inputs are computed before the stage, the slots the
   stage reads are already full and nothing waits.

Does awaiting remove the cross-worker deadlock class? It depends which class:

- **Query cycles** (A's job needs B's, B's needs A's): no. That is a logical cycle; no scheduler
  can complete it. It needs the cycle check or a cycle error, awaiting or not.
- **Worker starvation** (every worker blocked while the work they wait for sits in a queue): yes,
  a task that awaits does not hold a worker. But note that a latch wait can never cause this
  kind: a job in the active table is always running on some thread's stack, never queued, so a
  latch waiter is always waiting for a thread that is making progress or is itself in a cycle.
  Starvation comes only from **joins**: a worker running a piece that itself calls `par_*` and
  blocks until its sub-pieces retire, with every worker in the same state. The fix belongs in
  `parallel.rs`: the joining thread runs its own pieces (caller-runs), or nested `par_*` on a
  worker runs serially.

## Serial pre-pass (for the owner of `rustc_interface::passes`)

The data-forward form of the analysis stages: before `par_hir_body_owners(typeck)`, force the
item-level queries serially (or in their own earlier stage) so the per-body stage mostly reads
frozen results: the collection queries behind `check_mod_type_wf` (`type_of`, `generics_of`,
`predicates_of`, `fn_sig`, `adt_def`, `impl_trait_header`, `associated_items`), `crate_inherent_impls`,
`coherent_trait` for every trait, `trait_impls_of`, lang items and `resolutions`. Upstream already
orders `check_mod_type_wf` before the body stages; the change is to make that a hard barrier and
to keep closures in the same piece as their typeck root, so two pieces never race for one
`typeck` result. What remains cross-body after that: opaque types (`type_of(opaque)` runs the
defining bodies' `typeck`), const items and const generics (const eval of another body), and
`Steal` ordering. The cycle check covers those.

## Owned by other agents: things this pass depends on

- Set the mode before `create_global_ctxt` (else `Sharded::new` panics); see "How `Lock` picks".
- Register every nagoya worker in the session's `Registry` before it runs a piece, or every
  `WorkerLocal` deref (arenas, query arenas) panics.
- Keep the caller's `ImplicitCtxt::query` on the worker's context: it is the stack edge from the
  caller's query to the piece's jobs, which the cycle check walks.
- Carry `LAST_PANIC` and the payload from the worker's catch to the caller.
- Nested `par_*` on a worker: caller-runs or serial, or the pool can starve (see "Reactive").
- Worker stack size: rustc recursion needs large stacks (ekostd uses 16 MB); a cycle check or a
  deep typeck on a default-size nagoya stack overflows.

## Files changed in this pass

- `src/rustc_middle/query/job.rs`: `QueryWaitGraph`; `QueryWaiter::resumed`; `QueryLatch::wait_on`
  takes the graph and a cycle check; `set` under the graph lock; `remove_waiter`,
  `resume_waiter_with_cycle`, `with_waiters`; `extract_waiter` removed.
- `src/rustc_middle/query/mod.rs`: export `QueryWaitGraph`.
- `src/rustc_middle/query/system.rs`: `wait_graph` replaces `cycle_handler_nesting`.
- `src/rustc_query_impl/mod.rs`: construct `wait_graph`.
- `src/rustc_query_impl/execution.rs`: per-thread cycle nesting; `ActiveJobGuard` carries `tcx`;
  `wait_for_query` passes the waitee and the check; cache re-check keyed on the mode.
- `src/rustc_query_impl/job.rs`: `find_cycle_closed_by_wait`; missing-job-tolerant
  `abstracted_waiters_of`; `process_cycle` without `[0]`; `break_query_cycle` on the new resume
  call; unused helpers removed.
- `src/rustc_data_structures/stable_hash.rs`: `ADDRESS_CACHE_GENERATION` and its functions
  removed; `StableHashCtxt::memoized_address_hash` / `memoize_address_hash` (defaulted).
- `src/rustc_middle/ich.rs`: `StableHashState::address_memo`.
- `src/rustc_middle/ty/impls_ty.rs`, `src/rustc_middle/ty/adt.rs`: memo through `hcx`.
- `src/rustc_middle/ty/context.rs`: the per-`GlobalCtxt` generation bump removed.
- `src/rustc_mir_transform/pass_manager.rs`: profiler-name cache removed.
- `src/rustc_borrowck/{mod.rs, root_cx.rs, nll.rs, type_check/mod.rs, region_infer/mod.rs,
  region_infer/values.rs, region_infer/opaque_types/mod.rs,
  region_infer/opaque_types/region_ctxt.rs, diagnostics/bound_region_errors.rs}`: `Rc` to `Arc`.

## Verify

Not built (agents are banned from cargo). From the checkout:

```sh
. ~/.zshenv && cargo check
```
