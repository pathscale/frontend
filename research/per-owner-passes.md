# Per-owner stages for the per-module analysis passes

The problem: `run_required_analyses` and `analysis` (src/rustc_interface/passes.rs:1203-1212,
1312-1353) run six checks as a stage over `hir_module_ids()`. A source file is usually one
module, so each of those stages has one item and runs serially: about 55 ms of the ~200 ms that
stays serial at width 12 on the large clean corpus.

The change: every per-module query now runs its own work as a stage (or two stages in one
scope) over the module's owners, so a crate of one module spreads. The outer per-module stages
in passes.rs are unchanged; a nested stage inside a query is the shape
`check_private_in_public` already had.

Order is kept by construction. A stage replays its items' diagnostics in item order, and two
stages in one `stages` scope replay in stage order then item order, with the same serial cut-off
at a fatal error. Every stage below uses the serial walk's order as its item order, and
everything a pass carried between items was either absent or shown not to cross an item
boundary. Where a pass has a pre- or post-walk step (crate-root checks, `abort_if_errors`, the
module's own lint callbacks), that step stays on the query's thread, before or after the stage,
where the serial code ran it.

## The shared input: `rustc_passes::item_likes` (new, src/rustc_passes/item_likes.rs)

`hir_visit_item_likes_in_module` (src/rustc_middle/hir/map.rs:493-511) walks free items, trait
items, impl items, foreign items, each list in `hir_module_items` order. `item_like_count`,
`visit_item_like` and `item_like_def_id` index that walk: item `i` of a stage over
`tcx.hir_module_items(m)` (an arena reference, read in place) is the `i`th item-like, visited by
exactly the `visit_*` call the walk makes. For a module `ModuleItems::definitions` is the same
sequence, because `hir_module_items` never sets `add_root` (src/rustc_middle/hir/map.rs:1282;
only `hir_crate_items` does, :1320, and `owners()` is root + the same four lists,
src/rustc_middle/hir/mod.rs:91-99). No copy, no allocation.

## check_mod_attrs: split

Verdict: independent per item-like.

Evidence: `CheckAttrVisitor` (src/rustc_passes/check_attr.rs:121-126) holds `tcx` and
`abort: Cell<bool>`, nothing else. Its visitor uses `nested_filter::OnlyBodies`, so nested
items are not reached from their parent; they are item-likes of their own. Every
`check_attributes` call reads `tcx.hir_attrs(hir_id)` of the node in hand and, at most, the
`Item` it was called with; no check compares with another item. `abort` is only set (the two
`self.abort.set(true)` sites after `note_type_err` / `report_fulfillment_errors`) and read once,
at the end of the module.

What changed (check_attr.rs:1784-1810): a stage over item-likes, each with its own visitor,
returning its `abort` flag. The crate-root `check_attributes(CRATE_HIR_ID, ..)` runs after the
stage, as before after the walk. `abort_if_errors` runs if any item or the root asked, after the
stage has replayed every item, exactly where the serial code ran it. Raising it inside an item
would have cut the module short, so it is not done there.

## check_mod_unstable_api_usage: split

Verdict: independent per item-like.

Evidence: `Checker` (src/rustc_passes/stability.rs:578) is `{ tcx }`;
`MissingStabilityAnnotations` (:319) is `{ tcx, &'tcx EffectiveVisibilities }`. Both use
`OnlyBodies`. Neither writes anything but diagnostics. (The `fully_stable` writes in the file
belong to a local visitor used inside one impl check.)

What changed (stability.rs:541-575): the `Checker` walk is a stage; when staged API is on, the
crate root's `check_missing_stability(CRATE_DEF_ID)` runs next, then the
`MissingStabilityAnnotations` walk as a second stage; `check_unused_or_stable_features` stays
last for the root module. Same order as the three serial steps. The two walks stay separate
stages in sequence, not one scope: the root check sits between them.

## check_private_in_public: already split, now one scope

It was already two `run_stage`s (free items, then foreign items) over the module's id lists
(src/rustc_privacy/mod.rs:1957). `PrivateItemsInPublicInterfacesChecker` only reads. Changed to
two stages in one `stages` scope, so foreign items do not wait for the last free item; replay is
stage order then item order, the same as before.

## check_mod_privacy: split

Verdict: independent per item-like, for both visitors.

Evidence:
- `NamePrivacyVisitor` (privacy/mod.rs:948) holds `maybe_typeck_results`, which
  `visit_nested_body` (:1078-1086) replaces and restores, so it is `None` between item-likes.
  `OnlyBodies` again.
- `TypePrivacyVisitor` (:1159-1168) holds `maybe_typeck_results` (restored the same way,
  :1223-1227), `span` and `accessible_tys`.
  - `span` is written before every check that can emit: `walk_types` only calls
    `SpannedTypeVisitor::visit(span, ..)` (src/rustc_ty_walk), which sets it (:1212-1216);
    `check_expr_pat_type`, `visit_ty`, `visit_infer`, the method-call arm of `visit_expr` and
    the trait-impl step all set it first; `visit_qpath` emits at its own `span` argument. So the
    span left over from the previous item is never printed.
  - `accessible_tys` records only types whose walk found no error ("Errored walks are never
    cached", :1164-1167, and `check_ty` inserts only after `self.visit(ty)?` returned
    `Continue`, :1175-1182). It is keyed on the type, with `mod_id` fixed for the module. A
    fresh set per item only re-walks a type that walks clean again and emits nothing. Any query
    such a re-walk touches was already consumed by the earlier item, which is the first serial
    consumer, and replay prints a query's diagnostics at its first serial consumer.

What changed (privacy/mod.rs:1810-1853): two stages in one scope, name privacy then type
privacy, over the item-likes. Item `i` of the type stage is `item_like_def_id(module, i)`, the
`i`th of `module.definitions()`, which is the loop the serial code ran. Each item builds its own
visitor with the initial values the serial one started with (`span = def_span(mod_id)`, empty
`accessible_tys`).

## Late lints (lint_mod): split per top-level item

Verdict: independent per top-level item of the module (`Mod::item_ids`). Not per owner: nested
owners stay inside their parent's walk.

Evidence:
- `late_lint_crate` (src/rustc_lint/late.rs:459) runs only `late_lint_passes`, and nothing
  registers one. The store is `new_lint_store` (src/rustc_interface/interface.rs:440), which
  only registers `BuiltinCombinedLateLintModPass` (via `lint_mod`, src/rustc_lint/mod.rs:179)
  and, with internal lints, `InternalCombinedLateLintModPass` as a mod pass (mod.rs:748). So
  the crate-lint stage returns at once and all the work is in `lint_mod`.
- The serial walk (`late_lint_mod_inner`, before): `with_lint_attrs(module)` (check_attributes,
  check_attribute), `check_crate` for the root, `process_mod` = `check_mod` then `walk_mod`,
  which is `visit_nested_item` over `Mod::item_ids` (src/rustc_hir/intravisit.rs:670-674); the
  visitor is `nested_filter::All`, so each item is walked deeply (nested items, impl/trait
  items, bodies), but `visit_mod` does nothing when `only_module` (late.rs:234), so nested
  modules are left to their own `lint_mod`. Then `check_crate_post` and `check_attributes_post`.
- Context state between top-level items: `visit_item` (late.rs:133) restores `generics`,
  `typeck_results`, `enclosing_body`; `with_param_env` (:78) restores `param_env`;
  `with_lint_attrs` (:60) restores `last_node_with_lint_attrs` to the module's `HirId`. So every
  top-level item starts in the same context, which is the initial context
  (`tcx.local_def_id_to_hir_id(mod_id)` equals `hir_get_module`'s `HirId::make_owner`).
- Pass state: every pass in `BuiltinCombinedLateLintModPass` (mod.rs:236-305) and
  `InternalCombinedLateLintModPass` (mod.rs:311-327) is a unit struct from `declare_lint_pass!`
  or a fieldless `impl_lint_pass!`, except three:
  - `TypeLimits::last_visited_negation` (src/rustc_lint/types.rs:208) is set on a negation and
    only matched when `negated_id == hir_id` of a literal (types.rs:569). The operand of a
    negation is in the negation's owner; an earlier top-level item's value can never equal a
    later item's literal `HirId`. Starting at `None` changes nothing.
  - `NonLocalDefinitions::body_depth` (src/rustc_lint/non_local_def.rs:60) is `+= 1` in
    `check_body` and `-= 1` in `check_body_post` (:72, :76): balanced, 0 between top-level
    items, which is `default()`.
  - `IfLetRescope::skip` (src/rustc_lint/if_let_rescope.rs:104) holds `HirId`s of if-cascade
    expressions already covered and is asked only about the expression being checked
    (:132, :155, :300); an earlier item's ids never belong to a later item's owner.
  None of the three touches `check_mod`, `check_crate` or the attribute callbacks either.
- No pass holds a `Cell`, lock, atomic or `thread_local!` (grep over src/rustc_lint; the
  `LazyCell`s in impl_trait_overcaptures.rs are fields of a local visitor, not of the pass).

What changed (late.rs:344-457, mod.rs:180): `late_lint_mod` takes the builtin pass's
constructor (`fn() -> T`) instead of an instance. Which registered factories are required is
decided once per module; pass objects are built once for the module-level callbacks and once per
item, in the same combined order (registered, then builtin). The module-level callbacks run on
the query's thread with the module's pass object, and `Mod::item_ids` is a stage whose item `i`
calls `visit_nested_item(item_ids[i])` with its own `LateContextAndPass`. Replay order: module
prelude, items in `item_ids` order, module postlude, which is the serial order.

Why not per owner: a nested owner (an impl item, a fn-local item) is visited inside its parent's
`with_lint_attrs` / `with_param_env` / `check_item` .. `check_item_post` bracket, and
`NonLocalDefinitions` reads `body_depth` there. Splitting below the top-level item would have to
rebuild that bracket for every nested owner and would change what `check_item_post` sees. A
module whose single top-level item is a large `impl` still runs that impl on one thread.

A pass registered later with cross-item state would break this. The contract is written in the
doc comment on `late_lint_mod`; any new stateful mod pass needs the same check.

## check_mod_deathness: reporting split, liveness not

Verdict: split what is per module; the liveness analysis stays one whole-crate query.

- Cannot split: `live_symbols_and_ignored_derived_traits` (src/rustc_passes/dead.rs:1067) is a
  worklist fixpoint over the whole crate (`MarkSymbolVisitor`: `worklist`, `scanned`,
  `live_symbols`, `ignored_derived_traits`, `unsolved_items`, then two deferred re-seedings). An
  item's liveness depends on every other item, so there is no per-owner or per-module split of
  it. It runs once, as a query, at the first module's `check_mod_deathness`, and is still serial
  on that path. Parallelising it would need a different algorithm (for example a per-owner edge
  pass as a stage and then a serial fixpoint over the frozen edges). That is out of scope here.
- Can split: the reporting. `DeadVisitor` (dead.rs:1117) is `tcx`, the lint, and two `&'tcx`
  references into the frozen liveness result. Its `&mut self` methods write none of them
  (checked: no `self.field =` in the impl). Each free item's report reads only frozen data.

What changed (dead.rs:1398-1559): `check_mod_deathness` opens one scope. `lint_dead_codes` is a
stage over the module's free items then its foreign items (the order the two loops ran in), with
a filter deciding whether item `i` reports. The loop body is `lint_dead_free_item`, the same code
with `continue` turned into `return`. `DEAD_CODE_PUB_IN_BINARY` (executables only, filtered to
unused reachable pub items) is the first stage and `DEAD_CODE` the second, so every item's
first-lint output still comes before any item's second-lint output.

## Files changed

- src/rustc_passes/item_likes.rs (new), src/rustc_passes/mod.rs (`pub mod item_likes`)
- src/rustc_passes/check_attr.rs, src/rustc_passes/stability.rs, src/rustc_passes/dead.rs
- src/rustc_privacy/mod.rs
- src/rustc_lint/late.rs, src/rustc_lint/mod.rs
- src/rustc_interface/passes.rs (comments only)

Not compiled here. Points to watch when building:
- dead.rs `lint_dead_codes<'scope, 'tcx: 'scope>` takes `&'scope StageScope<'scope, '_>`;
  inside `stages(|scope| ..)` the `'tcx: 'scope` bound should follow from `'env: 'scope`, as it
  does for the closures in `rustc_lint::late::check_crate`.
- late.rs filters factories with `mk_pass(tcx)` on a `&&Box<dyn Fn>` (a call through autoderef)
  and calls `hir_visit::Visitor::visit_nested_item` by path.
