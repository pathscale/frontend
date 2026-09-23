# Lowering as a stage, and a corpus that reaches analysis

Two pieces of the parallel analysis work on `feat/speed-01`. Neither has been built or run by
the author: every claim below about what compiles or what the numbers are is for the main
session to confirm with the command at the end. Line numbers are as of this change.

## 1. AST to HIR lowering as one stage

### What changed

- `src/rustc_ast_lowering/mod.rs:784`, new `pub fn lower_every_owner(tcx)`: one
  `run_stage((), index_ast.len(), ..)` whose item `i` forces `lower_to_hir(LocalDefId i)`,
  skipping definitions whose `def_kind` is never a HIR owner.
- `src/rustc_interface/passes.rs:938`, a `hir_crate_items` provider that calls
  `lower_every_owner` and then the plain `rustc_middle::hir::map::hir_crate_items` walk,
  installed at `passes.rs:958`, right after `rustc_middle::hir::provide` sets the plain one.

### Why there

`hir_crate_items` is the gateway. Every HIR consumer on both entry points passes it before it
reads an owner: `analysis` reaches it through `hir_module_ids` in the HIR id validator
(`rustc_passes/hir_id_validator.rs:15`) or its own `ensure_done` (`passes.rs`,
`run_required_analyses`), and `frontend_facts::extract` forces it first thing
(`frontend_facts/mod.rs:401`). The walk inside it used to lower each owner serially the first
time it touched it, so lowering already happened inside this query; the stage only moves it to
the front and spreads it. An owner something read before `hir_crate_items` (a single `def_span`,
say) is already a finished query and the stage's item for it is a cache hit.

A provider override is the one place the owned files can reach it; the alternative is a line
at the top of `rustc_middle/hir/map.rs:1296`, which is outside this task's files.

### The input: the index, by position, and nothing read out of it

Item `i` is `LocalDefId` `i`, input `()`, length `index_ast(()).len()`, the same shape
`extract` uses over the definitions table. No owner list is built.

Which items lower is decided by `def_kind`, not by the index entry's `AstOwner`, because an
entry cannot be looked at safely: it is a `Steal`, and `Steal::steal`
(`rustc_data_structures/steal.rs:67`) takes the write lock with `try_write().expect(..)`, so a
concurrent `borrow()` or `is_stolen()` on the same entry makes the stealing thread panic.
`def_kind` was fed by `TyCtxt::create_def` (`rustc_middle/ty/context.rs:1333`) when the
resolver created the definition, before this query can run, and any thread may read it.
Skipped kinds: `Variant`, `Field`, `Ctor`, `TyParam`, `ConstParam`, `LifetimeParam`,
`AnonConst`, `OpaqueTy`, `Closure`, `SyntheticCoroutineBody`. Their `lower_to_hir` only falls
back to the parent owner (`mod.rs:679`), which has an item of its own.

### Safety checks, one owner's lowering against another's

| Shared thing | What lowering does with it | Where | Verdict |
|---|---|---|---|
| AST index entry | `lower_to_hir(i)` steals entry `i` only; the query runs once per key, so one steal | `mod.rs:677` | own slot only |
| Resolver | shared `Arc<ResolverAstLowering>`, read through `&` only (`LoweringContext.resolver: &'a`) | `mod.rs:163-165`, `item.rs:37-40` | read only |
| Disambiguators | `resolver.disambiguators` is a map of `Steal`s; a context steals its own owner's (`LoweringContext::new`, `mod.rs:244-248`) and, in `with_hir_id_owner`, each nested `use` tree's (`mod.rs:930`, called from `item.rs:707`). A nested tree's own query never makes a context (`AstOwner::NestedUseTree` goes to `fallback_to_ancestor`), so each disambiguator has one stealer | `mod.rs:244`, `mod.rs:930` | own slot only |
| `next_node_id` | a field of the context, started from `resolver.next_node_id` per context (`mod.rs:273`); new `NodeId`s are mapped only in the context's own `node_id_to_def_id` | `mod.rs:868`, `mod.rs:213-215` | per owner |
| `children`, `bodies`, `attrs`, `trait_map`, `delayed_lints`, `impl_trait_*` | fields of the context, moved into the owner's `OwnerInfo` by `make_owner_info` | `mod.rs:990` | per owner |
| HIR arena | `tcx.hir_arena` is `&WorkerLocal<hir::Arena>` (`rustc_middle/ty/context.rs:741`, built in `passes.rs` `create_and_enter_global_ctxt`); the context derefs it once, on the thread running the query, and a query runs on one thread | `mod.rs:255` | per thread |
| Definitions table | `create_def` (`mod.rs:839`) takes `definitions.write()` in `TyCtxt::create_def`; used for elided lifetimes (`lifetime_res_to_generic_param`, `mod.rs:1178`, every `&self` method), and anonymous consts (`mod.rs` const-arg path, `pat.rs:537`, `expr.rs:584`, `delegation/generics.rs:566`) | `context.rs:1324` | locked, but see "numbering" |
| Hygiene | `mark_span_with_reason` (`mod.rs:1148`) makes a fresh `ExpnId` for every desugaring (`while`, `?`, ranges, `for`) under `HygieneData`'s lock | `rustc_span/hygiene.rs:956` | locked, but see "numbering" |
| `AttrId`s | `sess.psess.attr_id_generator` is an atomic counter | `expr.rs:919`, `expr.rs:2314` | atomic, see "numbering" |
| Delayed lints | per owner in `OwnerInfo.delayed_lints`, emitted later by `emit_delayed_lints` walking `hir_crate_items().owners()` in order | `passes.rs` `emit_delayed_lints` | ordered |
| Diagnostics | emitted inside a stage item, collected by the item hook and forwarded in item order; `has_errors()` is counted at collection, so the `has_errors().unwrap()` sites (`expr.rs:317`, `item.rs:1442`) still see errors emitted earlier (they rely on parse and feature-gate errors, which precede lowering) | `rustc_errors/mod.rs` `print_or_capture` | serial order, see below |
| Crate-wide queries read by every context | `registered_attr_tools` (`mod.rs` in `LoweringContext::new`), `features`, `index_ast`: forced on the owner thread before the stage, so the first item does not compute them inside its own slot | `mod.rs:784` | ordered |
| `lint_buffer` | a `Steal` in the resolver, stolen by `early_lint_checks`, which `index_ast` forces first; lowering never touches it | `rustc_middle/ty/mod.rs:293` | untouched |

### Owner to owner dependencies, and order

Owners do not need each other's lowering, with two exceptions, both ordinary query
dependencies the query system already orders (a thread waits for a query another thread is
running, and runs one nobody has started):

1. A nested `use` tree's query lowers its parent `use` item (`fallback_to_ancestor`, `mod.rs:679`,
   with `AstOwner::NestedUseTree(owner_id)`).
2. A delegation (`reuse`) item reads the signature and generics of its target
   (`delegation/mod.rs:178`, `delegation/resolution.rs:137`, `delegation/generics.rs:302`),
   which lowers the target.

So no item has to wait for a slot, and the stage does not need `Slots::wait`. What order can
change is where a dependency's diagnostics land. Case 1 cannot move anything: if a nested
tree's item runs before its parent's, it lowers the parent inside its own slot, but the only
definitions between a `use` item and its nested trees are its other nested trees (the def
collector creates them right after the item), which emit nothing, so the printed order is the
serial one. Case 2 can move a target's lowering errors to the delegation's slot; it needs the
unstable `fn_delegation` feature and a target that fails to lower.

### What is not the same as before

- **Serial order.** At one worker the stage is a loop in index order on the calling thread.
  Index order is the def collector's pre-order walk; the old order was the HIR walk's, which is
  the same pre-order for items. Lowering diagnostics can come out in a slightly different order
  than before this change, identically at every worker count.
- **Numbering.** `LocalDefId`s made during lowering (one per elided lifetime), `ExpnId`s and
  `SyntaxContext`s from desugarings, and `AttrId`s are handed out from shared counters, so in a
  parallel run their numbers depend on which owner got there first. Their hashes are
  content-derived and their names in messages do not carry the numbers, so no answer should
  change; but anything that orders by the raw index (span `Ord` breaks ties on the syntax
  context index; `LocalDefIdMap` iteration) could. The identical-answers check in
  `examples/parallel_timing.rs` is the verifier; if it trips, this is the first place to look.
- **An item inside an attribute's value.** The def collector walks attributes
  (`rustc_resolve/def_collector.rs:573`) and gives such an item a definition; the indexer does
  not (`mod.rs` `Indexer::visit_attribute`), so its entry is `NonOwner`, and its query falls
  back to a parent whose HIR does not list it and panics (`mod.rs:692-698`). The serial walk
  never asked for it; the stage does. The attribute has already been refused with an error
  (`rustc_attr_parsing/validate_attr.rs:122`, or "cannot find attribute" for an unknown one),
  and `extract` read `def_span` of every definition before this stage existed, so it is not new
  for `analyze_source`; it is new for `check_source`. Not fixed here: telling that entry apart
  needs the entry, and the entry cannot be read race-free (above). A fix would be the indexer
  recording, in `index_ast`, which entries are owners, which means a change to the query's type
  in `rustc_middle/queries.rs`.
- **Memory.** Every owner's AST is dropped and the last `Arc` of the resolver goes at the end
  of the stage, rather than as the walk passes.

## 2. A corpus that reaches analysis

### The problem

`check_source` and `analyze_source` run as `no_core`. This crate's own files use `core`,
`alloc` and `eko` names on nearly every line, so a session stops at the first missing lang item
or unresolved path and the per-body stages after type checking (`MIR_borrow_checking`, the
module checks, lints) barely run. Timings over `src/**/*.rs` measure the early passes.

### The corpus

`tests/corpus/` holds hand-written fixture sources, not generated files:

- `prelude.rs`: one `#![allow(..)]` line (case lints, `dead_code`, `internal_features`), then
  `tests/parallel.rs`'s `LANG` prelude verbatim, then what integer code needs beyond it:
  `LegacyReceiver for &mut T`, `Copy for u64`, and the `add`, `sub`, `mul`, `eq` and
  `partial_ord` lang item traits with impls for `u32` and `u64`, written with the operators
  themselves, as rustc's `minicore` does. `PartialEq` has `ne` as a required method (a default
  needs `!`), and `PartialOrd` has only `lt`, `le`, `gt`, `ge` (`partial_cmp` needs `Option`).
- Five units, each a block of items whose every name carries the tag `Q0`:
  `unit_geometry.rs` (structs, tuple struct, enum of every variant shape, trait with defaults,
  generic fn, `&mut self`, `while`), `unit_ledger.rs` (`u64` via `as`, range `match`, `loop`
  with `break`, tuples), `unit_machine.rs` (enums moved through `self`, `match` on a tuple,
  `break value`), `unit_numeric.rs` (consts, recursion, `else if` chains), `unit_cells.rs`
  (associated type and const, generic struct, bounded impl, `where`, nested module with
  `use super::{..}`). About 60 to 110 lines and 6 to 12 bodies each.

Deliberately absent: `!` (no `Not`), `/` and `%`, statics (need `Sync`), closures, arrays and
slices, `dyn`, `for` loops (need `IntoIterator`), unconstrained integer literals (fall back to
`i32`, which has no impls here).

`examples/parallel_timing.rs` builds the corpora in memory: a file is a comment line, the
prelude, then units picked by a 64-bit LCG with a fixed seed until the file reaches a target
line count drawn from the same generator, each copy with `Q0` replaced by `Q<corpus><file>x<unit>`.

- `clean`: 200 files, 300 to 1,500 lines, seed `0x5eed_0001`. About 180,000 lines.
- `large`: 6 files, 5,000 to 8,000 lines (60 to 95 units, 500 to 800 bodies each), seed
  `0x5eed_0002`.
- `src`: this crate's `src/**/*.rs`, now sorted by path so a file index names the same file
  every run.

Each corpus is timed at 1, 2, 4, 8 and 12 workers, `check_source` then `analyze_source`, with
ms and speedup against one worker. Every setting is held to the one-worker answers; on a
difference the first differing file index and both values are printed for each entry point,
then it panics. For `clean` and `large` the one-worker pass prints how many files were clean
(`Checked::is_clean`: no error and not fatal) and, if any were not, the first such file's index
and first five errors.

## Unsure, until built and run

- The corpus being clean. It has not been type checked. The likeliest failures are in the
  prelude: a lang item this frontend wants that `minicore` does not declare the same way, or an
  operator lookup that needs more than the methods declared. The clean count and the first
  errors printed are there to make that a one-iteration fix.
- `lower_every_owner` compiling as written: `LocalDefId::new` (via `rustc_index::Idx`, already
  imported in `mod.rs`), `DefKind::Ctor(..)`, and `run_stage` with a `()` input.
- The `hir_crate_items` provider: `rustc_middle::hir::map::hir_crate_items` is `pub(crate)` and
  returns `ModuleItems`, which is what an `arena_cache` provider returns; `map` is `pub mod`.
- Whether the numbering note above shows up as a differing answer at 2 or more workers.

## Run

```
cargo run --release --features parallel --example parallel_timing
```

Without `--features parallel` every setting is the serial path; the corpus clean counts are
still meaningful.
