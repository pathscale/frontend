# Chained stages: item `i` triggers item `i` of the next stage

The owner asked for less queuing and fewer barriers: per-body work chained, so body `i`'s next
step runs on the core that just finished its previous one. This is what was built, where it is
used, and why type checking to borrow checking is not chained.

## The primitive (`src/rustc_data_structures/sync/stage.rs`, "Chains")

- `StageScope::stage_then(&upstream, input, len, weight, f) -> Chain`: `upstream` is a stage of
  the same scope with one `bool` per item. When upstream item `i` settles with `true`, the thread
  that ran it runs the chained item `i` right after (`Run::follow`), inside the same install of
  the scope's context, under its own catch. An item whose upstream said `false` waits for
  `Chain::release`. Until released, a chained stage hands out no chunks and sweeps skip items
  whose upstream has not let them start (`Run::ready`).
- The chained stage is an ordinary stage in every other respect: its own `seq` (the next one,
  so all its items follow all upstream items in serial order), its own `OrderedReplay`, its own
  slots. Cut-off is unchanged: an upstream fatal error or panic at order `o` cuts off every
  chained item, since all of them are later than `o`. Whoever runs a chained item runs or waits
  for its upstream item first, and checks the cut-off after that wait, so an item whose upstream
  failed is failed unrun on every path.
- `StageScope::conclude_through(&slots)`: settles and concludes that stage and every earlier
  one now (replay forwarded, panic resumed or fatal error raised as a serial run would have by
  then), leaving later stages running. For owner work that sits between two stages serially.
- Serial scope: `stage_then` runs nothing; `release` runs the whole stage in index order,
  exactly the old loop, where it is called. `conclude_through` does nothing.
- The follower is held by the upstream stage's value (`then: OnceLock<Arc<dyn Run>>`), so it is
  dropped with it inside the scope; `Open::follow` refuses to chain across scopes. `detach`'s
  SAFETY comment lists the new strong reference.

## Where it is used: name privacy to type privacy (`src/rustc_privacy/mod.rs`)

Both walks were already two stages over the same item-likes in one scope, with no order between
them. Chaining only restricts which interleavings run (type `i` after name `i`, same thread
when possible); every interleaving was already a schedule the scope could produce, and the
existing per-item independence argument (the doc comment on `check_mod_privacy`) holds for all
of them. Stage numbers, replays and cut-off are unchanged. Serial: the type stage is released
straight after the name stage, the two old loops. Identical by construction.

This is the general rule for a provable chain: **two stages already in one scope over the same
index space**. Chaining them never adds a schedule, it removes some.

## Type checking to borrow checking: not chained, and why

What runs between them serially, in order (`rustc_hir_analysis::check_crate`, then
`run_required_analyses`): the typeck stage; the by-move-body stage over the same owners
(creates `DefId`s); the `rustc_attrs` dumps; `check_unused_traits` (lints, on the owner);
`definitions.freeze()`; then the borrowck stage. Item-level analysis:

- Diagnostics order, cut-off, owner work between: solved by the primitive
  (`conclude_through` of typeck, owner work, `release` of borrowck after `freeze`). Every
  borrowck-item emission is a query record, and a record comes out at its first serial
  consumer, owner work included.
- Anon-const feeding: a borrowck item for a type-system anon const or a typeck child (a closure
  inside an anon const) can reach `type_of` of an anon const before the enclosing body's typeck
  feeds it (the hazard the by-move stage's comment describes). Solvable: the typeck item would
  say `true` only when it ran `typeck` on the owner itself.
- `has_errors()` reads on the borrowck path (`check_unsafety.rs:583`, `check_consts/check.rs:193`,
  `dyn_compatibility.rs:590`, error reporting `traits/mod.rs:391`, `validate.rs:138` under
  `-Zvalidate-mir`): each is either reached only after an error it can see was counted, or is
  an ICE guard. Same class the existing stages already carry within a stage.
- By-move bodies created earlier by `mir_promoted` change `DefIndex` order only. The def path
  is fixed (fresh disambiguator, one per coroutine-closure) and facts skip
  `SyntheticCoroutineBody` and count definitions before analysis.

**The blocker: query cycles entered from a different query.** Borrowck of `f` builds MIR, which
const-evaluates the constants in `f`'s patterns and promoteds. Serially the typeck stage
evaluates every non-generic `const` and `static` at its own item (`eval_to_const_value_raw`,
`eval_static_initializer`) before any MIR of an ordinary `fn` is built. Counterexample:

```rust
const fn f() -> u8 { match 1 { K => 0, _ => 1 } }
const K: u8 = f();
```

Width 1: item `f` only type checks (no MIR, no evaluation); item `K` enters the cycle at
`eval_to_const_value_raw(K)` and the cycle error is reported from there. Chained, at any width
above one, even a single chunk on the owner: typeck(`f`) settles, borrowck(`f`) follows at
once, and its THIR (`check_unsafety(f)`, then `mir_built(f)`) lowers pattern `K`, which
evaluates `K`, which calls `f`, whose THIR or MIR is already on the stack. The cycle closes at
`f`'s query: another "cycle detected when" line, another query given the recovery value.
The typeck stage alone cannot do this (a typeck item builds no MIR), so it is a new divergence,
and no per-item gate removes it: the cycle can be entered by any later typeck item, so the only
safe gate is "every typeck item has settled", which is the barrier.

What would make it provable: const evaluation of the crate's consts and statics moved out of the
typeck stage into a stage of its own before it (then every eval cycle is entered from the same
query at any schedule), plus an argument that no other cycle through MIR building is reachable
from a typeck item. Neither is done here.

## Other candidates looked at

- `check_liveness` after `mir_borrowck`: already the same item.
- By-move stage after typeck: it builds MIR (`mir_built`) for coroutine-closures, so the same
  cycle argument applies; and each item costs about a nanosecond.
- `frontend_facts::extract`'s body stage: `analysis` (lints, per-module checks) sits between it
  and borrowck, all of which consume queries a speculative body walk would compute.
- `check_mod_deathness`: two lint stages over the same items in one scope, provable by the rule
  above, but binaries only and cheap. Left as is.
