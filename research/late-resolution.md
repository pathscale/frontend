# Late name resolution as a per-item stage

Status: the walk is cut into per-item units with owned outputs and an in-order merge, and it
runs serially, one unit after another, through the same per-unit function a stage would call.
It is not a stage yet: every unit still resolves through `&mut Resolver`. This note records
what landed, every write late resolution makes into shared resolver state (with file and line),
what each write has to become before the unit loop can be `run_stage`, and what cannot be made
parallel and why.

Nothing here has been built or measured by the agent that wrote it (building is the main
session's job). The section "Checking it" says what to run.

## Why

Profiling `check_source` over this crate's own 1,423 files at one worker put about 62% of
samples in `Resolver::resolve_crate` and about 37% in `late_resolve_crate`, a single
`LateResolutionVisitor` walking the whole crate with `&mut Resolver`. A large share of that is
error-suggestion search (`edit_distance`, `lookup_import_candidates_from_module`,
`try_lookup_name_relaxed`), because the corpus, read with no sysroot, has many unresolved
names.

One finding matters for the whole plan: the expensive suggestion searches are already `&self`
on the resolver. `lookup_import_candidates` (`src/rustc_resolve/diagnostics/impls.rs:1866`),
`lookup_import_candidates_from_module` (`:1629`), `add_scope_set_candidates` (`:1497`),
`add_module_candidates` (`:777`), `add_typo_suggestion` (`:2243`), `into_struct_error` (`:804`)
and `report_error` (`:796`) take `&self`. What makes late resolution need `&mut Resolver` is
the bookkeeping around resolution, not the searching: recording uses, errors and resolutions,
and a handful of lazily filled tables inside the module graph. That bookkeeping is small in
count and every piece of it is listed below.

## What landed

All in `src/rustc_resolve/late.rs`.

- `LateUnit` (`late.rs:5810`), `collect_late_units` / `collect_late_units_in` (`:5882`,
  `:5903`): the unit list, built once before any unit runs.
- `resolve_late_unit` (`:5956`): one unit, with a `LateResolutionVisitor` of its own, started
  from state rebuilt out of the module graph.
- `LateUnitOutput` (`:5834`) and `merge_late_units` (`:5999`): what a unit produces into tables
  nothing reads during resolution, owned by the unit and merged in unit order.
- `late_resolve_crate` (`:5853`) runs `ItemInfoCollector`, builds the units, runs them in a
  plain loop, and merges.
- `LateResolutionVisitor::new` now takes the `ParentScope` to start at; the visitor gains `out`
  and `defer_children_of`, and `into_output`.
- The `ItemKind::Mod` arm of `resolve_item` (`:2989`) stops at a unit `mod`'s items and visits
  only its visibility; `leaks_macro_rules` (`:5785`) is the `#[macro_use]` rule, shared by that
  arm and the unit builder.

### The unit, and why that granularity

A unit is every item directly in a module (the crate root's and every `mod` item's, found by
descending through `mod` items and nothing else), plus unit 0 for the crate root's own
attributes (their doc links). Units are in the order the old walk reached them: pre-order, a
`mod` item followed by its items.

- A `mod` item is a unit for what it resolves itself (doc links and visibility), and each of its
  items is a unit after it. This crate is a tree of modules, so stopping at the crate root's
  items would give a handful of huge units.
- An `impl` or `trait` is one unit with its associated items: they sit inside its generic
  parameter ribs, its `Self` and its trait reference, which a per-associated-item unit would
  have to rebuild (possible, but it is resolution work, not a lookup; see "Finer units").
- A function is one unit with its body and every item nested in the body, which sit in the
  body's anonymous block modules and ribs.

That is the finest cut whose starting state is nothing but the module graph. Between two items
of a module the old visitor held only:

| state                         | at a module-level item                     | rebuilt from            |
|-------------------------------|--------------------------------------------|-------------------------|
| value and type ribs           | crate root rib, then one `Module` rib per enclosing `mod` | the unit's `parent` chain (`opens`) |
| macro ribs                    | crate root rib only                        | `new`                   |
| lifetime ribs                 | one `LifetimeRibKind::Item` per enclosing `mod` (pushed by its `visit_item`) | the `parent` chain |
| `parent_scope.module`         | the innermost enclosing module             | the `parent` chain      |
| `parent_scope.macro_rules`    | depends on the items before it             | `LateUnit::macro_rules`, computed by `collect_late_units_in` |
| `parent_scope.expansion`, `derives` | root, empty                          | `ParentScope::module`   |
| label ribs, `current_trait_ref`, `lifetime_elision_candidates`, `in_func_body` | empty / `None` / `false` | `new` |
| `diag_metadata` (all but `unused_labels`) | as `Default`, every field set and restored by the item that set it | `new` |
| `lifetime_uses`               | only entries of earlier items' parameters, never read again | `new` |
| `last_block_rib`              | the last block of an earlier item          | not carried: see below  |
| `current_owner`               | the crate's tables (inside `with_owner(CRATE_NODE_ID)`) | unchanged: the loop runs inside the same `with_owner` |

The `macro_rules` scope is the one piece of walk state that depends on earlier items, and it
depends only on `macro_rules!` items and `#[macro_use]` modules, reading
`Resolver::macro_rules_scopes`, which early resolution finished. So the whole sequence is known
before any unit runs: `collect_late_units_in` applies the two rules `resolve_item` applies (a
`macro_rules!` item moves the scope to `macro_rules_scopes[its def id]`, `late.rs` MacroDef arm;
a module restores the scope from before it unless it is `#[macro_use]`/`#[macro_escape]`).

Owner tables: the old walk kept every enclosing `mod`'s `PerOwnerResolverData` on the stack
(as `current_owner`, swapped out by the child's `with_owner`) while its items ran. Now each
`mod` unit's tables are back in `owners` before its items run. Nothing a child does reads
`owners[&enclosing mod]` (it would have panicked before, `Resolver::owner_def_id`,
`mod.rs:1667`), and nothing writes an enclosing owner's tables (writes go to `current_owner`,
the child's own), so this changes nothing observable.

### Moved into `LateUnitOutput`

Writes that happen during late resolution, are only ever read after it, and are therefore owned
by the unit and merged afterwards:

| write (now)                                   | was                                        | merge rule |
|-----------------------------------------------|--------------------------------------------|------------|
| `out.use_injections.push` `late.rs:4679`, `:4783` | visitor field `use_injections`          | concatenate in unit order (consumed by `report_with_use_injections`, `diagnostics/impls.rs:406`) |
| `diag_metadata.unused_labels` (`:5123` insert, `:5282` and `late/diagnostics.rs:1424` `swap_remove`) | one crate-wide map | moved out by `into_output`, buffered as `UNUSED_LABELS` in unit order after every unit ran |
| `out.confused_type_with_std_module.push` `:4863`, `:4864` | `Resolver::confused_type_with_std_module.insert` | replay the inserts in unit order: an `IndexMap` insert of an existing key keeps its position and takes the new value, so the replay ends where the walk ended |
| `out.potentially_unnecessary_qualifications.push` `:5619` | `Resolver::...push` (read by `check_unused.rs:601`) | concatenate in unit order |
| `out.delegation_infos.push` `:4018`             | `Resolver::delegation_infos.insert` (read by lowering) | insert in unit order; keyed by the delegation's own owner, so no two units share a key |

The unused-label order: the old map was crate-wide with `swap_remove`, so its iteration order
depended on history across items. The per-unit maps concatenate to the same set in a different
order. That order does not reach output: `LintBuffer` (`rustc_errors/decorate_diag.rs:62`) is
keyed by node, the early lint pass takes lints per node, and a label's node gets exactly one
unused-label lint, buffered after every other lint on it, as before.

### The one intended output change

`last_block_rib` is the last block rib the walk popped; `smart_resolve_report_errors`
(`late/diagnostics.rs:1153`) uses it for "the binding `x` is available in a different scope in
the same function". A function resets it on entry to its body (`late.rs:1204`, in `visit_fn`), but
nothing reset it at item boundaries, so in

```rust
fn f() { let x = 1; }
static S: i32 = x;
```

the error on `x` in `S` got that help, naming a block of `f`, and calling it the same function.
Each unit now starts with `last_block_rib: None`, so that help no longer appears across items.
Carrying it from unit to unit would make every unit depend on the one before it, which is
exactly what the split removes. This is the only serial output difference I know of. If byte
identity with the old compiler is required even there, the unit loop can pass the previous
unit's `last_block_rib` in (serial only), at the price of that chain.

## What late resolution writes, all of it

Everything a unit does to state outside its own visitor. "Unit-local key" means the key is a
`NodeId` or owner inside the unit, so units never write the same entry.

### A. Owner tables (`PerOwnerResolverData`, swapped in by `with_owner`)

Already per owner, and each owner belongs to exactly one unit:

- `current_owner.lifetimes_res_map.insert` `late.rs:2482`
- `current_owner.lifetime_elision_allowed = true` `late.rs:2511`
- `current_owner.extra_lifetime_params_map` `late.rs:2220`
- `current_owner.label_res_map.insert` `late.rs:5281`
- `current_owner.trait_map.insert` `late.rs:5474` (via `record_traits_in_scope`)
- `owners.remove` / `owners.insert` in `with_owner_tables` `mod.rs:2754`, `:2771`, entered from
  `late.rs:893` (items), `:1109` (foreign items), `:3421` (trait items), `:3629` (impl items)

Read back during the unit only for the unit's own owners. Also read through Resolver methods:
`opt_local_def_id` / `local_def_id` (`mod.rs:1679`, `:1684`) read `self.current_owner`, and
`MaybeExported::eval` (`late.rs:723`).

To become a stage: take the unit's owners' tables out of `owners` before the stage (they are
the unit's input), let the visitor hold its current owner by value instead of the resolver, and
hand the tables back as output; `merge` puts them back into `owners`. Merge order does not
matter (disjoint keys). `owner_def_id` (`mod.rs:1667`) must then read a frozen
`NodeId -> LocalDefId` map of owners rather than the tables themselves, since other units'
tables are out.

### B. Resolution maps keyed by unit-local nodes, still written into `Resolver`

- `partial_res_map` via `record_partial_res` (`mod.rs:2489`): `late.rs:1001`, `:3239`, `:3316`,
  `:3925`, `:4314`, `:4884`; direct insert `late.rs:5081`; from name lookup
  `ident.rs:1906` (`record_segment_res`, which also reads `contains_key` first).
  Read during late resolution at `late.rs:971`, `:2720`, `:2753`, `:4003`, `:4118`, `:4395`,
  `:5613`, `late/diagnostics.rs:263`, `:311`, `:1221`, `:1968`, `:1976`, `:1996`, `:2827`, and
  `Resolver::legacy_const_generic_args` (`mod.rs:2691`). Every one of those reads a node of the
  item being resolved (its own paths, its `Self` type, its bounds), so the reads can go to the
  unit's own map.
- `pat_span_map` via `record_pat_span` (`mod.rs:2496`): `late.rs:4315`. Read by
  `diagnostics/impls.rs:3221` while reporting an error in the same unit.

To become a stage: a per-unit `NodeMap<PartialRes>` and `NodeMap<Span>` in the output, read
through the unit during the unit, extended into the resolver after. Order does not matter
(disjoint keys); the "resolved twice" panic in `record_partial_res` stays per unit.

### C. Order-dependent sinks

These are where serial order is observable, and each needs a stated merge rule.

- `ambiguity_errors`: pushed by `maybe_push_glob_vs_glob_vis_ambiguity` (`ident.rs:824`, push at
  `:835`) and `maybe_push_ambiguity` (`ident.rs:848`, push at `:967`), reached from
  `ident.rs:515`, `:524`; and by `record_use` (`mod.rs:2336`), which first scans the whole
  vector for an equal error (`matches_previous_ambiguity_error`, `mod.rs:2321`) and skips it.
  Rule: per-unit vector with the same dedup against the unit's own entries, then at merge the
  same dedup against everything merged so far, in unit order. Equality-based "first wins"
  applied in serial order gives the same vector the walk built. Reported after the walk, in
  vector order (`report_errors`), so the order is the serial one.
- `privacy_errors`: pushed by `finalize_module_binding` (`ident.rs:1393`); then
  `resolve_path_with_ribs` remembers `privacy_errors.len()` on entry and rewrites every error
  pushed since (`ident.rs:2089`). Rule: per-unit vector (the rewrite only touches the unit's own
  tail), concatenated in unit order; deduplicated at report time as today.
- `macro_expanded_macro_export_errors.insert` (`ident.rs:1412`): a `BTreeSet`, so merge by union;
  order is the set's.
- `lint_buffer`: `ident.rs:666`, `:717` (derive fallback), `record_use` (`mod.rs`, private
  `macro_use`), `lint_if_path_starts_with_module` (`diagnostics/impls.rs:701`, buffered at
  `:745`, called from `ident.rs:2122`, `:2230`), `late.rs:2427`, `late/diagnostics.rs:3706`,
  `:3758`, and the unused labels. Rule: per-unit `LintBuffer`, merged in unit order by appending
  each node's vector. Only the order within one node's vector reaches output, and a node's lints
  all come from its own unit, so appending in unit order is the serial order.
- `import_use_map` (entry, max of `Used`), `used_imports` (insert), `glob_map` (entry, insert),
  all in `record_use` (`mod.rs:2336`, called from `ident.rs` `finalize_module_binding`,
  `late.rs:4494`, `:4509`); `maybe_unused_trait_imports.insert` and `glob_map` in
  `find_transitive_imports` (`mod.rs:2256`, from `traits_in_scope`). Rule: per-unit sets and
  maps, merged by union (and max for `import_use_map`). All are monotone and commutative, so
  merge order does not matter for their contents; `check_unused` reads them only after
  resolution. `IndexSet`/`IndexMap` insertion order can differ from the walk's if merged in a
  different order, so merge in unit order anyway.
- `doc_link_resolutions` / `doc_link_traits_in_scope` (`late.rs:5481`..`:5498`,
  `:5566`..`:5583`): per-module maps used as a cache across items of one module (the code's own
  FIXME says the caching "may be incorrect" with shadowing `macro_rules`). Rule: each unit
  resolves its own links and returns its entries; merge with first-wins in unit order, which is
  the value the walk kept. The difference is only in the FIXME case: a later item that the walk
  would have answered from an earlier item's entry resolves itself, and the prefixes it then
  goes on to resolve can differ. Off by default (`ResolveDocLinks::None` returns at once), and
  that case is the upstream bug the FIXME names; recorded, not fixed here.

### D. A global counter

- `next_node_id` / `next_node_ids` (`mod.rs:2001`, `:2008`), from `late.rs:2194`, `:2216`
  (fresh lifetime parameters) and `:2281` (elided lifetimes in paths). The ids end up in
  `lifetimes_res_map` (`LifetimeRes::Fresh { param, .. }`), `extra_lifetime_params_map`, and
  `LifetimeElisionCandidate`s, and lowering creates definitions for them.

Rule, and it keeps serial numbering identical: each unit allocates from its own counter
starting at zero and returns how many it used; the merge gives unit `i` the base
`next_node_id + sum of counts of units before i` and adds it to every fresh id in that unit's
tables. Since nothing else allocates node ids during late resolution, the ids come out exactly
as the single walk numbered them. The remap has to touch every place a fresh id is stored,
which are the three maps above; that list is what to check when adding a new one.

### E. Lazily filled state inside the module graph

These are the writes that make the "frozen" module graph not frozen. Each is a memo of a value
that is fully determined once expansion and import resolution are done.

- Tracked `RefCell` borrows. `CmRefCell::borrow_checked` (`mod.rs:3131`) takes a real
  `RefCell::borrow()` when the resolver is not speculative, which increments and decrements a
  non-atomic counter on every `NameResolution` read. In a parallel stage that is a data race
  even though nothing is written. Rule: late resolution runs in a mode where `borrow_checked`
  returns `CmRef::Untracked` (what speculative mode already does) and every `borrow_mut` asserts
  it is not in that mode. The existing `SpeculativeFlag` has the right semantics for reads but
  `cm_mut()` asserts it is off, so a distinct "frozen" state is needed.
- `ModuleData::traits`, filled by `ensure_traits` (`mod.rs:845`, `borrow_mut_checked`), from
  `traits_in_module` (`mod.rs:2218`) from `traits_in_scope` (`mod.rs:2170`), which late calls
  at `late.rs:5468` and `:5571`. Rule: compute it for every module before the stage (a stage of
  its own, one item per module, reading only resolutions). That is eager work for modules no
  path asks about; for local modules it is one pass over each module's children.
- `macro_rules` path compression in `visit_scopes` (`ident.rs:161`, `Cell::set` on a
  `MacroRulesScopeRef`): rewrites an invocation scope to what it expanded to. Rule: compress
  every scope once, after expansion, before the stage; or have frozen mode skip the `set` and
  walk the chain. Both give the same answer.
- `extern_module_map` (`build_reduced_graph.rs:124`, `borrow_mut` at `:137`) and
  `extern_macro_map` (`build_reduced_graph.rs:216`): external crates' modules and macros are
  materialised on first lookup; `Resolutions::Extern` is a `OnceLock` (`mod.rs:671`), already
  thread-safe. With no sysroot named (this crate's default, see `AGENTS.md`) there are no
  external crates and these are never touched. With a sysroot they are, and there is no bound
  on what to materialise up front. See "What cannot be parallel".
- `hir_arena.alloc_slice` in `traits_in_scope` and `find_transitive_imports`: a `WorkerLocal`
  arena (`rustc_middle/ty/context.rs:741`), so allocation from a worker is fine; the slices are
  referenced from the owner tables that come back as output.

### F. Diagnostics

`report_error` (`late.rs:4907`, `diagnostics/impls.rs:796`), `report_path_resolution_error`
(`diagnostics/impls.rs:2988`), `span_delayed_bug`, and every `emit()` in `late/diagnostics.rs`
go to the session `DiagCtxt`. Inside a stage item they are collected by the item hook
(`rustc_interface/util.rs` `DIAGNOSTICS_HOOK`, `rustc_errors/item_scope.rs`) and replayed in
item order, so they need nothing here. Stashed diagnostics (`StashKey`) are keyed globally;
a unit that stashes and a later unit that steals would be a cross-unit dependency. I found none
in late resolution, but the stash is not per unit, so it is worth a check when this becomes a
stage.

### G. `&mut` that writes nothing

Several `&mut self` receivers are there for the type, not a write, and would simply become
`&self` in a frozen mode: `resolve_ident_in_lexical_scope` (`ident.rs:321`, it calls
`cm_mut()` at `:363` and `:379` even with no `Finalize`), `report_path_resolution_error`
(`diagnostics/impls.rs:2988`, which calls the former), `legacy_const_generic_args`
(`mod.rs:2691`, reads only), and `resolve_path_with_ribs`, which calls `get_mut()` for the
lexical lookup at `ident.rs:2046` whatever `finalize` is.

## What late resolution reads (the frozen view)

Everything below is written by the time `late_resolve_crate` starts and not written by it
(except as listed above): the module arenas and every `ModuleData` (resolutions, parents,
`no_implicit_prelude`, glob importers), `graph_root`, `empty_module`, `local_module_map`,
`block_map`, `prelude`, `extern_prelude`, `macro_use_prelude`, `builtin_type_decls`,
`builtin_attr_decls`, `registered_attr_tool_decls`, `macro_rules_scopes`,
`output_macro_rules_scopes`, `local_macro_map`, `field_names`, `field_defaults`,
`field_visibility_spans`, `struct_ctors`, `struct_generics`, `item_generics_num_lifetimes`,
`item_required_generic_args_suggestions`, `delegation_fn_sigs` (the last three written by
`ItemInfoCollector` just before the units), `effective_visibilities`, `mods_with_parse_errors`,
`glob_error`, `proc_macros`, `stripped_cfg_items`, `on_unknown_data`, `features`, `arenas`, and
`tcx` (queries, which are parallel-safe on their own terms).

## Turning the loop into a stage

In order; each step keeps the serial output unchanged and is checkable on its own.

1. **Owner tables into the unit** (section A). The visitor owns its current owner's tables
   instead of `Resolver::current_owner`; `with_owner` becomes a visitor method; tables come out
   of `owners` into the units before the loop and back in the merge. Resolver methods that read
   `current_owner` (`opt_local_def_id`, `local_def_id`, `MaybeExported::eval`) take the tables
   as an argument when called from late resolution.
2. **A late sink.** A `LateSink` holding the unit's parts of section B and C (partial
   resolutions, pattern spans, privacy and ambiguity errors, lint buffer, use maps, glob map,
   trait imports, export errors), added to `LateUnitOutput`, with the merge rules above.
   `CmResolver` (`mod.rs:2933`, today `RefOrMut<Resolver>`) grows a third case, a shared
   resolver plus `&mut LateSink`, and each write site in `ident.rs` (the `get_mut()` calls at
   `:515`, `:524`, `:666`, `:717`, `:1147`, `:1189`, `:1255`, `:1906`, `:2046`, `:2089`,
   `:2122`, `:2210`, `:2230`), `record_use` and `find_transitive_imports` writes to whichever
   the case names. The reads of sink state (`partial_res_map.contains_key`,
   `privacy_errors.len()`, the ambiguity dedup) read the same place. Early resolution keeps
   the resolver case and is untouched.
3. **Per-unit node ids** (section D), renumbered in the merge.
4. **Frozen mode** (section E): untracked `CmRefCell` reads, `ensure_traits` for every module
   and `macro_rules` compression done before the stage, `debug_assert`s on every lazy write
   path that the mode is off.
5. **The stage.** `LateResolutionVisitor::r` becomes `&'a Resolver`; the unit function becomes
   `|units, index| resolve_late_unit(&resolver, krate, root_scope, units, index)`; the loop in
   `late_resolve_crate` becomes `run_stage(&units[..], units.len(), ...)`, whose serial mode is
   this loop, and the merge is unchanged. `ItemInfoCollector`, `collect_late_units` and
   `merge_late_units` stay serial before and after it.

Step 2 is where most of the edits are, and it is mechanical: about 25 sites in `late.rs` and
`late/diagnostics.rs`, the 13 in `ident.rs`, `record_use`, `find_transitive_imports`, and
`lint_if_path_starts_with_module`. It was not attempted in this pass because none of it can be
checked without a build, and a half-routed sink (some writes in the sink, their reads still on
the resolver) is wrong in ways the type checker does not see.

## What cannot be parallel, and why

- **`ItemInfoCollector`**: writes per-item facts every unit reads (lifetime counts of other
  items, delegation signatures). A whole-crate pre-pass by design, and cheap (no lookups). It
  could itself be a stage over the units, with its writes as outputs, but its cost is small.
- **Building the units and the `macro_rules` sequence**: a serial prefix scan, inherently; it
  touches only module-level items and one hash lookup per `macro_rules!` item.
- **The merge, `report_errors`, `check_unused`**: they consume the merged, ordered result.
  `report_with_use_injections` deduplicates `use` suggestions across the crate and
  `report_privacy_error` across errors, which is cross-unit by definition. They run once.
- **External modules with a sysroot**: materialised on demand, so a frozen view either
  materialises everything reachable up front (unbounded, it is the whole of `std`'s module
  tree) or keeps a synchronised on-demand table, which is a cache with a lock and against the
  house rules. With no sysroot, the default, there is nothing external and the view is
  complete. With one, late resolution stays serial until there is an answer to that.
- **Within a unit**: a function body is resolved in order, each `let` changing the ribs of what
  follows; nothing inside a unit is parallel.

### Finer units

The balance of the stage depends on the largest unit. On this crate the big ones are large
`impl` blocks and long functions. Per associated item of an `impl` or `trait` is possible: the
state to rebuild is the impl's generic parameter ribs, `Self` rib, and trait reference, which
means resolving the impl header once per associated item (or once, as its own unit, with the
header's ribs as its output, which the associated items' units then read: a two-stage chain,
exactly what `StageScope::stage` with `Slots::wait` is for). Not attempted here.

## Checking it

Serial, which is what exists now:

1. The corpus diagnostics before and after this change should be identical except for the
   `last_block_rib` case above. Run `check_source` over the crate's own 1,423 files at one
   worker on the parent commit and on this tree, and diff the rendered diagnostics.
2. The `late_resolve_crate` timer (`mod.rs`, `resolve_crate`) should be unchanged within noise:
   the units add a visitor construction and a few rib pushes per module-level item, and the
   merge moves the moved outputs once.

When it is a stage: the same diff at one worker and at N workers must be empty, and the timer
is the measurement of what the split bought.

## Unsure it compiles

- `into_output` moves `unused_labels` out of `*diag_metadata` (a `Box<DiagMetadata>`); a
  partial move out of a box by destructuring `*box` is allowed, but it is the first place in the
  file that does it.
- `ParentScope { module, macro_rules: unit.macro_rules, ..root_scope }` in `late.rs` builds a
  struct defined in the parent module with private fields; allowed from a child module.
- `MacroRulesScopeRef` is imported into `late.rs` through the private `use macros::{..}` in
  `mod.rs`; a child module may name its parent's private imports.
- The `ItemKind::Mod` arm returns early from inside the `with_rib` closures (`return;`), which
  returns `()` from the closure, as intended.
