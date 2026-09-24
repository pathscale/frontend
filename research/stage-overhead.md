# What a stage costs per item and per stage, and what was cut

Status: source changed, not built or measured by the agent that wrote this (building is the main
session's job). "Checking it" at the end says what to run. Line numbers under "Before" are the
tree at `727e510`; under "After" they are the working tree this note was written with.

## The measurements this starts from

`examples/parallel_timing.rs`, `PROFILE=large:<workers>:30`, six clean `no_core` files of
5,000 to 8,000 lines, sampled with macOS `sample`:

- `check_source` per pass: 479 ms at one worker, 515 ms at two (slower), 236 ms at eight.
- At two workers the top of stack gains about 106 samples of `semaphore_signal_trap` and 27 of
  `__psynch_mutexwait` that one worker does not have, plus `pthread_getspecific`.
- nagoya's pool has 12 threads whatever the session's `jobs.frontend`; at eight workers each
  is busy 59% and parked 41% (`st3` fanout `StdHost::park`).
- Stages have one item per body or owner, and every item paid for everything below.

## Before: per item

Every item, on every thread, went through `Stage::run_claimed`
(`src/rustc_data_structures/sync/stage.rs:692`):

| Cost | Where |
| --- | --- |
| Find the item: the scope's `stages` lock, an `Arc` clone of the stage, and a rescan from stage 0, per item (helpers) | `stage.rs:579-589` (`claim_any`), lock and clone at `:583` |
| Claim: one `fetch_add` on the stage's cursor, which every helper writes, then a compare-and-swap on the slot | `stage.rs:680-690`, `:682`, `:686`; slot CAS `:180-185` |
| Cut-off check: one `Acquire` load | `stage.rs:694` |
| `catch_unwind` frame | `stage.rs:704`, `pool.rs:85-87` |
| Context install through two `&mut dyn FnMut` hops | `stage.rs:713`, `par_context.rs:104-117` |
| `SessionGlobals`: `is_set` then `with` (two `pthread_getspecific`) on a thread that has them, or a set and a reset | `util.rs:251-266` |
| `ImplicitCtxt`: `TLV.with`, replace, and a deferred restore | `util.rs:287-296`, `rustc_middle/ty/context/tls.rs:75-87` |
| Item hook dispatch | `par_context.rs:185-195`, `util.rs:314-323` |
| Mode check: `is_dyn_thread_safe` (one `pthread_getspecific`) | `rustc_errors/item_scope.rs:565`, `sync.rs` `mode::latched` |
| Collection: `Arc<Mutex<Vec<Event>>>` allocated, plus an `Arc` clone, though nearly every item emits nothing | `item_scope.rs:569`, `:574` |
| Clock: `tick()`, a `fetch_add` on the one `CLOCK` every worker shares | `item_scope.rs:572`, `:156-158` |
| Scope push and pop on `SCOPES` (two `pthread_getspecific`) | `item_scope.rs:576`, `:582` |
| `catch_fatal_errors` frame | `item_scope.rs:588` |
| Report: the stage's one `waiting` lock, shared by every thread, and a `BTreeMap` insert (node allocation) | `item_scope.rs:604-606` |
| Settle: `fill` or `fail`, then `wake`, which takes the stage's `waiters` lock on every settle, waited on or not | `stage.rs:188-203`, `:299-311` |

## Before: per stage and per scope

| Cost | Where |
| --- | --- |
| Scope: `Arc<ScopeShared>`, a `Vec` for the captured context, two capture reads, `Registry::try_current` (TLS and an `Arc` clone), `pool::width` (TLS) | `stage.rs:540-556`, `par_context.rs:86-96` |
| Stage: `OrderedReplay` boxed, `SCOPES` read, a tick at the top level | `stage.rs:757`, `util.rs:307-309`, `item_scope.rs:532-550` |
| Stage: `Arc<Stage>`, `Arc<Slots>`, `Box<[Slot]>`, `stages` lock | `stage.rs:758-783` |
| Wakes: `len.min(width - 1).min(workers)` helpers submitted, each a boxed job and a worker wake (`semaphore_signal`), even for a stage of one item the owner is about to run | `stage.rs:787-792` |
| Helper arrival: `active` `SeqCst`, registry lease (a mutex), registry enter (two TLS writes and an `Arc` clone), mode latch (TLS) | `stage.rs:818-847`, `worker_local.rs:139-142`, `:191-196`, `:212-220` |
| Settle: the owner walked every index in order (`wait_all`), running unclaimed items one at a time (a `Weak::upgrade` compare-and-swap each, and the whole per-item context install each), and parking on every item a helper was running | `stage.rs:593-605`, `:236-257`, `:271-276` |
| Parking: `nagoya::block_on` allocates an `Arc<Signal>`, clones the waker into `waiters` | `stage.rs:260-262`, nagoya `block_on.rs:92-104` |
| Conclude: `OrderedReplay::finish` drains the `BTreeMap` | `item_scope.rs:612-632` |

**The two-worker regression, read from the code.** At two workers the owner and its one
helper went through the same indices side by side: the helper from the cursor, the owner in
`wait_all` from index 0. Every item the helper held when the owner reached it parked the owner
in `block_on` and cost a wake when it settled; that is `semaphore_signal_trap`. The stage's
`waiters` lock taken on every settle and the `waiting` lock taken on every report are the two
locks both threads took per item; that is `__psynch_mutexwait`. And every stage, including the
many of one or two items, woke a helper that either found nothing or took the owner's item.

## After

### Chunks: reserved from the cursor, claimed per item

`reserve` (`stage.rs:905`) takes `chunk` consecutive indices with one `fetch_add`, where
`chunk = len / (threads * CHUNKS_PER_THREAD)`, at least one (`chunk_size`, `stage.rs:628`;
`CHUNKS_PER_THREAD = 8`, `:598`; `threads` is `min(width, pool workers + 1)`). A reservation
changes no slot. The reserving thread walks its chunk and claims each item with the same slot
compare-and-swap as before, as it reaches it.

**Why waiters still work.** An item inside a reserved chunk that its reserver has not reached
is still `UNCLAIMED`. `Slots::wait(i)` finds it so, claims it and runs it there, exactly as
before; the reserver's own claim then fails and it moves on. Nothing a waiter could run is ever
hidden from it, so the invariant the module header rests on (a thread only ever waits for an
item that is running) holds unchanged. Of the two designs the brief offered this is the second:
lazy per-item claims inside a chunk-sized reservation, with the context held across them.

Once every chunk of every stage is reserved, a thread with nothing left sweeps each stage once
(`next_sweep`, `stage.rs:735`) for items reserved elsewhere and not reached, so the tail of a
slow chunk is shared instead of waited for.

### The context: once per run of items

`ScopeShared::drain` (`stage.rs:753`) installs the scope's captured context once, inside one
`catch`, and runs item after item in it, across chunks and across the scope's stages
(`run_in_context`, `stage.rs:928`). The context is the scope's, identical for every item of
every stage in it, and an item restores whatever it changes, so holding it across items is
the same state per item as installing it per item. Helpers (`help`, `stage.rs:1078`) and the
owner at settle (`stage.rs:810`) both use `drain`. `run_claimed` (`stage.rs:966`), a waiter's
single item, installs it for that one item, as before.

Per item this removes: the `catch_unwind` frame, both context hooks (four to six
`pthread_getspecific` and the matching writes), the `stages` lock, the stage `Arc` clone and the
stage rescan. Per chunk it removes the per-item cursor write.

**A panic or a fatal error in item `k` of a chunk.** A panic unwinds out of the run to the
catch; `panicked` (`stage.rs:952`) sees the slot still `RUNNING`, records the cut-off at `k`
(`stash`) before settling `k` as failed, exactly the order it had. The rest of the chunk stays
unclaimed; whoever claims one of those items (the next thread, or the owner's settle at the
latest) finds it past the cut-off and fails it unrun. Then the run goes on, context installed
again, for items earlier in serial order in other stages. A fatal error is caught inside the
item by the diagnostics hook, as before, and sets the cut-off (`stop_at`); the next item's
check (`stage.rs:930`) sees it. Either way nothing after `k` in serial order starts after `k`
stopped. A panic that is not out of an item (the slot already settled, or none claimed) is
stashed at `u64::MAX` as a helper's scaffolding failure was, and that thread stops draining.

### The owner at settle

`settle` (`stage.rs:805`) now drains first, taking chunks exactly as a helper does, and only
then walks the stages in order to wait for items other threads are still running. Owner and
helpers no longer walk the same indices: each has its own chunk, and they meet only in the
sweep, where a failed claim is skipped, not waited on. The owner parks at most once per item
still in flight when it runs out of work, which is at most one per other thread.

### Wakes: the rule

At stage start (`stage.rs:1036-1052`):

```
wanted = (unreserved chunks across the scope's open stages - OWNER_CHUNKS)
         .min(width - 1)            // jobs.frontend, less the owner
         .min(pool workers)
submit wanted - helpers already in the scope
```

`OWNER_CHUNKS = 1` (`stage.rs:616`): the owner takes one chunk itself when it settles, so a
scope whose remaining work is one chunk wakes nobody. That is the threshold, and it is a
decision rule, not a tuned number: the measured cost of a helper that finds nothing (or takes
the owner's only item and parks it) is visible in the two-worker profile, while the cost of an
item is not measured, so a larger threshold cannot be derived from what we have. It would also
be wrong for the stages whose items are whole passes: `rustc_lint::late::check_crate` (two
stages of one item in one scope: two chunks, one helper) and `frontend_facts::diagnostics`'s
`both_passes` (one stage of two items: one helper). `run_stage` over one item wakes nobody.

The cap is `width - 1`, which is `jobs.frontend - 1` for a session; the registry lease in `help`
still bounds how many helpers of one session run at once to its registry's slots.

### Diagnostics hook (`rustc_errors/item_scope.rs`, `rustc_interface/util.rs`)

- **Collection created on first use** (`item_scope.rs:609`, `collect_in_stage`): an item scope
  starts with `collection: None`, like a query frame, and the scope is popped and its collection
  (if any) taken at the end. An item that emits nothing allocates nothing. Removed per item: one
  `Arc<Mutex<Vec>>` allocation and one `Arc` clone.
- **The stage's clock value, not a tick per item** (`item_scope.rs:544`, `:574`): every item
  scope opens at the replay's `opened`. The argument that this captures exactly the same
  contexts is on the field: a context created between the stage's start and an item's start is
  one the item cannot reach (a stage's function and input outlive its scope, so they cannot
  borrow anything the scope's body made later, and another item's scratch context is that
  item's local). Removed per item: one `fetch_add` on the process-wide `CLOCK`.
- **No mode check on the hook path** (`util.rs:327` calls `collect_in_stage`): the hook only
  runs inside a parallel stage, where every participating thread has a parallel session
  latched. `collect` keeps the check for any other caller. Removed per item: one
  `pthread_getspecific`.
- **Reports by index, not through one lock** (`item_scope.rs:547`, `:651`, `:666`): the replay
  is sized at `begin` (the hook's `begin` now takes the stage's `len`, `par_context.rs:155`,
  `util.rs:312`). An item reports with one `Release` store into its own byte; only an item that
  emitted something takes the shared `emitted` lock. `finish` sorts the (few) emitters by index
  and walks the report bytes in order, forwarding and stopping at the first fatal exactly as
  the `BTreeMap` walk did. Removed per item: the shared lock and a `BTreeMap` insert.

### Slots

- **Settle without the lock when nobody waits** (`stage.rs:361`, `:424`): `waiting` counts the
  registered wakers; `wake` checks it after a `SeqCst` fence and takes the `waiters` lock only
  when it is not zero. `ReadySlot::poll` updates it under the lock and fences before re-reading
  the state, so one of the two sides always sees the other (the comment in `wake` spells it
  out). Removed per item: one lock shared by every thread settling items of the stage.

### Per scope

- `CapturedContext` is a fixed array of `MAX_HOOKS` (`par_context.rs:79`, `:89`), no `Vec`.

## Unchanged, on purpose

- **Serial mode.** `StageScope::stage`'s serial loop is untouched. The only change a serial
  stage sees is `Slots::fill` taking the lock-free path in `wake` (a fence and a load instead
  of an uncontended lock), which is behaviourally identical.
- **Per item, kept:** the slot claim and settle, the cut-off load, the diagnostics hook's
  `catch_fatal_errors` frame and its `SCOPES` push and pop (the frame is the item's; this is
  what keeps diagnostics per item and the replay order serial).
- **Answers.** Items run in the same set, each exactly once, cut off by the same rule; the
  replay forwards the same events in the same index order.

## Not done, and why

- **Registry lease per helper arrival** (`worker_local.rs:139-142`, a mutex): per helper, not
  per item, and fewer helpers now arrive. Outside the files this change owns.
- **Wakes for a nested scope whose session has no free registry slot.** A scope opened inside
  an item still counts only its own helpers, so it can submit helpers that then find the
  registry full and leave. Fixing that needs the registry's free-slot count, a small read-only
  method in `worker_local.rs`, outside the files this change owns.
- **`SCOPES` push and pop** (two `pthread_getspecific` per item): the item's scope has to be on
  the thread's stack while it runs, and `eko`'s thread locals are `pthread` keys. Holding the
  stack's borrow across a run of items would need the hook to take a batch, which would put
  chunking into `rustc_errors`.

## Checking it

Nothing here has been compiled. The main session:

1. `cargo check --features parallel` and `cargo check` (serial build: `Run`, `Payload` and
   `Range` are named outside the `parallel` module and must not warn).
2. `cargo test --features parallel --test parallel`, and `--test unwind`.
3. `PROFILE=large:1:30`, `large:2:30`, `large:8:30` with `examples/parallel_timing.rs`, answers
   compared across worker counts, and `sample` at two workers for `semaphore_signal_trap` and
   `__psynch_mutexwait`.

## The gate: serial when the work cannot pay (speed-01, item 3)

Small sessions lost at width 12: on `src/`, `type_check_crate` 109 to 276 ms, coherence 104 to
268, `misc_checking_1` 54 to 171 (CPU summed). Fanning a pass out added 80 to 120 us per
session whatever its size, on 38 to 77 us of work.

- **Weights.** `run_stage_weighted` and `StageScope::stage_weighted` take a per-item weight in
  estimated nanoseconds: the item's source bytes (`TyCtxt::stage_weight`, read from the
  resolver's span table with no query; `Span::byte_len_untracked` for AST units) times its
  pass's measured rate (`sync::cost`: `WALK` 1, `TYPECK` 4, `RESOLVE` 73 ns per byte). Read once
  on the owner at stage start, only in a parallel session. `run_stage` and `stage` stay
  unweighted: every item counts as one chunk, which is the old count-based behaviour.
- **Plan.** `stage.rs` `Plan::new` cuts the indices into contiguous chunks of about
  `total / (threads * 8)` weight and at least `MIN_CHUNK_WEIGHT` (125 us, about 32 KiB at
  typeck's rate); a tail under half a chunk joins the previous one.
- **Gate.** `helpers_for(chunks, weight) = min(chunks, weight / MIN_CHUNK_WEIGHT) - 1`. A
  `run_stage` whose plan wakes nobody is the plain serial loop (same calls, same order, what
  items emit goes where a serial session sends it). A stage in a scope that does not pay is one
  chunk and wakes nobody; the scope's replay still orders it with the other stages.
- **Owner first.** A parallel `run_stage` reserves its first chunk for the owner before waking
  anybody (`ScopeShared::owner_first`, taken by `settle` on every path).
- **False sharing.** Chunks are contiguous and claimed in order, so slot and replay-byte writes
  from different threads meet only at chunk boundaries. A stage's cursor and a scope's `stages`
  lock, `active` count and `cutoff` sit on 128-byte lines of their own (`Padded`).
- **Threshold rationale.** About 10 us of overhead per helper (117 us over up to eleven); a
  125 us chunk keeps it under a tenth; the smallest stage that fans out (250 us) is twice the
  whole full-width overhead. The average `src/` file (20 KB, 80 us of typeck) runs serially.

Not weighted: `frontend_facts` definition and import stages (cost is per path printed, not per
byte, and unmeasured), parse chunks (handover item 4), `hir_id_validator` (debug only).
