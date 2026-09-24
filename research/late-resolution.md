# Late name resolution as a per-item stage

Status: late resolution runs its units as a stage (`run_stage`) over a frozen, shared
`&Resolver`, each unit writing only into state it owns, merged serially in unit order. Steps 1
to 5 of "Turning the loop into a stage" below are all in the tree. When external crates are
reachable or doc links are on, the same units run through the same function in a serial loop
instead (see "When it stays serial").

Nothing here has been built or measured by the agent that wrote it (building is the main
session's job, and this pass was source only). "Unsure it compiles" lists every place worth a
first look when the build fails; "Checking it" says what to run.

## Why

Profiling `check_source` over this crate's own 1,423 files at one worker put about 62% of
samples in `Resolver::resolve_crate` and about 37% in `late_resolve_crate`, a single
`LateResolutionVisitor` walking the whole crate with `&mut Resolver`. A large share of that is
error-suggestion search (`edit_distance`, `lookup_import_candidates_from_module`,
`try_lookup_name_relaxed`), because the corpus, read with no sysroot, has many unresolved
names.

The expensive suggestion searches were already `&self` on the resolver
(`lookup_import_candidates`, `lookup_import_candidates_from_module`, `add_scope_set_candidates`,
`add_module_candidates`, `add_typo_suggestion`, `into_struct_error`, `report_error`, all in
`diagnostics/impls.rs`). What made late resolution need `&mut Resolver` was the bookkeeping
around resolution: recording uses, errors and resolutions, and a handful of lazily filled tables
inside the module graph. Every piece of it is listed below with where it goes now.

## The shape

`Resolver::late_resolve_crate` (`late.rs`):

1. `ItemInfoCollector`, serial, inside `with_owner(CRATE_NODE_ID)` as before.
2. `collect_late_units`: the unit list and each unit's `macro_rules` scope (unchanged).
3. If `late_resolution_can_freeze()`: `prepare_frozen_late_resolution()` (every lazy write the
   units could make, done up front), set `FrozenFlag`, `run_stage(&units[..], units.len(),
   |units, i| this.resolve_late_unit(.., units, i, LateDocLinks::default()))` with
   `this: &Resolver`, clear the flag. Otherwise the plain loop, with the same
   `resolve_late_unit`, moving the doc link tables from each unit to the next.
4. `merge_late_units`: serial, in unit order.

A unit (`resolve_late_unit`, `&self`) builds a `LateResolutionVisitor` whose `r` is
`&'a Resolver`. Everything the visitor writes goes to three places it owns:

- `sink: LateSink<'ra>` (`mod.rs`): every write the name lookups in `ident.rs`, `mod.rs` and
  `diagnostics/impls.rs` make, reached through `CmResolver::Late(&Resolver, &mut LateSink)`.
  The visitor builds that with `late_cm!(self)` (`late.rs`, a macro so it borrows only the `r`
  and `sink` fields and the ribs and scope can be passed alongside).
- `current_owner: LateOwner` (`late.rs`): the owner tables (section A).
- `out: LateUnitOutput` (`late.rs`): the visitor's own writes (sections C and D, and what was
  already moved there).

`LateResolutionVisitor::r` being `&'a Resolver` is what makes this checkable: any write that was
missed is a type error, not a silent race.

## The unit, and why that granularity

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
| current owner                 | the crate's tables                         | `new` enters `owners[CRATE_NODE_ID]` |

The `macro_rules` scope is the one piece of walk state that depends on earlier items, and it
depends only on `macro_rules!` items and `#[macro_use]` modules, reading
`Resolver::macro_rules_scopes`, which early resolution finished. So the whole sequence is known
before any unit runs.

### The one intended output change (from the previous pass)

`last_block_rib` is the last block rib the walk popped; `smart_resolve_report_errors`
(`late/diagnostics.rs`) uses it for "the binding `x` is available in a different scope in the
same function". A function resets it on entry to its body, but nothing reset it at item
boundaries, so in

```rust
fn f() { let x = 1; }
static S: i32 = x;
```

the error on `x` in `S` got that help, naming a block of `f`, and calling it the same function.
Each unit starts with `last_block_rib: None`. This pass changes nothing further: the stage's
output is meant to be byte-identical to the serial unit loop, which was already the reference.

## Every write, and where it goes now

"Unit-local key" means the key is a `NodeId` or owner inside the unit, so units never write the
same entry.

### A. Owner tables (step 1)

Late resolution writes five fields of `PerOwnerResolverData` and nothing else writes them:
`lifetimes_res_map`, `lifetime_elision_allowed`, `label_res_map`, `trait_map`,
`extra_lifetime_params_map`. The visitor now holds `current_owner: LateOwner<'a, 'tcx>`: the
owner's `id`, `def_id` and `&node_id_to_def_id`, read in place from the frozen
`Resolver::owners`, plus those five tables, owned. `LateResolutionVisitor::with_owner` replaces
the free `with_owner` for every late call site (items, foreign items, trait items, impl items):
it enters `LateOwner::enter(&r.owners[&owner])`, runs the work, and pushes
`(owner, LateOwnerTables)` onto `out.owners`. Each unit starts in the crate's owner, whose
tables go out last from `into_output`.

Every former `r.current_owner.*` read and write in `late.rs` now names `current_owner`, and
the two readers outside the visitor take it as an argument: `MaybeExported::eval(r,
&current_owner)`, and `FindReferenceVisitor` holds `&current_owner.lifetimes_res_map`.
`Resolver::local_def_id` from late resolution is `LateResolutionVisitor::local_def_id`.

**Deviation from the plan, and why.** The plan said "take the unit's owners' tables out of
`owners` before the stage and put them back in the merge". A stage item gets its input by
shared reference (`Fn(&In, usize)`), so moving a table out of the input into the unit needs a
take-once cell per table, which is shared mutable state with a synchronisation primitive on it,
against the house rules. Nothing needs the move: the fields def collection wrote are only read,
and the fields late resolution writes start empty. So the frozen entry stays in `owners` and
is read in place, and the unit owns the five written tables outright; the merge
(`merge_late_owner_tables`) extends the entry's maps with them. Merge order does not matter
(disjoint keys; the crate owner's tables come from several units, still with disjoint keys).

One diagnostic observed which owners were out of `owners`: `late/diagnostics.rs`, the
delegation case of the associated item suggestions, read `r.owners.get(&assoc_item.id)`, which
returned `None` for the owner being resolved and every owner around it (and the crate, which
`late_resolve_crate` had out). `LateResolutionVisitor::open_owner_tables` reproduces that
exactly: `None` for `CRATE_NODE_ID`, `current_owner.id` and every id on `enclosing_owners`
(the stack `with_owner` keeps), the frozen entry otherwise.

### B. Resolution maps keyed by unit-local nodes (step 2)

- `partial_res_map`: `LateSink::partial_res_map`. Writes: `LateSink::record_partial_res` (with
  the "resolved multiple times" panic checked against the sink and the frozen resolver), from
  the visitor's `record_partial_res` (six sites in `late.rs`) and from
  `CmResolver::record_partial_res` (`ident.rs`, `record_segment_res`, whose `contains_key` is
  now `CmResolver::partial_res(id).is_none()`); the one overwriting insert (`resolve_qpath`,
  primitive type fix-up) goes to `sink.partial_res_map.insert`. Reads: the visitor's
  `partial_res(id)` (sink, then frozen resolver) at every former `r.partial_res_map` read in
  `late.rs` and `late/diagnostics.rs`, `SelfVisitor` through `LateSink::partial_res`, and
  `Resolver::legacy_const_generic_args`, which now takes the sink. Merge: extend (disjoint
  keys; the overwrite wins there too, as it did).
- `pat_span_map`: `LateSink::pat_span_map`, written by `LateSink::record_pat_span` (the only
  writer was late resolution, so `Resolver::record_pat_span` is gone). Read by
  `report_path_resolution_error`, now `CmResolver::report_path_resolution_error(&self)`, as
  `self.pat_span(id)` (sink, then resolver). Merge: extend.

### C. Order-dependent sinks (step 2)

Every one of these is a `LateSink` field with a `CmResolver` accessor that names the resolver
in the `Mut` case and the sink in the `Late` case (`lint_buffer_mut`, `privacy_errors_mut`,
`privacy_errors_len`, `macro_expanded_macro_export_errors_mut`, `import_use_map_mut`,
`used_imports_mut`, `glob_map_mut`, `maybe_unused_trait_imports_mut`,
`set_issue_145575_hack_applied`, `push_ambiguity_error`, `record_partial_res`, `partial_res`,
`pat_span`). `Ref` panics on a write, as `get_mut` did.

- `ambiguity_errors`: `LateSink::ambiguity_errors: Vec<(AmbiguityError, bool)>`, the flag saying
  whether the push was deduplicated. `record_use` deduplicates (`push_ambiguity_error(e, true)`:
  skipped if equal to one in the frozen resolver or already in the sink);
  `maybe_push_glob_vs_glob_vis_ambiguity` and `maybe_push_ambiguity` (now `CmResolver` methods)
  do not (`push_ambiguity_error(e, false)`). Merge: in unit order, a deduplicated entry is
  dropped if equal to anything merged so far, the others are pushed. Equality
  (`same_ambiguity_error`) is an equivalence, so "keep the first of its kind" applied inside a
  unit and again across units keeps exactly what the walk kept, and the raw pushes stay where
  they were.
- `issue_145575_hack_applied`: a write the old inventory missed (`maybe_push_ambiguity`, the
  #145575 and #149681 cases, reachable in late resolution through `ModuleGlobs`). Only ever set:
  merged by `|=`. Nothing reads it after import resolution.
- `privacy_errors`: sink vector. `resolve_path_with_ribs` takes `privacy_errors_len()` on entry
  and rewrites `privacy_errors_mut()[len..]`, the unit's own tail; `resolve_qpath` takes the
  sink's length and truncates the sink. Merge: concatenate in unit order.
- `macro_expanded_macro_export_errors` (`finalize_module_binding`): a `BTreeSet`, merged by
  union.
- `lint_buffer`: every late lint (`ident.rs` derive fallback lints, `record_use`'s
  `PRIVATE_MACRO_USE`, `lint_if_path_starts_with_module`, `ELIDED_LIFETIMES_IN_PATHS`,
  `SINGLE_USE_LIFETIMES`, `UNUSED_LIFETIMES`) goes to `sink.lint_buffer`. Merge: in unit order,
  each node's vector appended to the resolver's (`LintBuffer::map` is public). The key order of
  the `IndexMap` is first-insertion order, and replaying units in order reproduces it; so does
  each node's vector, including a node in another unit (`PRIVATE_MACRO_USE` is buffered on the
  import's `root_id`). `UNUSED_LABELS` are buffered after every unit's sink, as the previous
  loop did (it buffered every other lint while the units ran and the unused labels in its
  merge).
- `import_use_map` (entry, keep the larger `Used`), `used_imports`, `glob_map`,
  `maybe_unused_trait_imports`: written by `CmResolver::record_use`, `add_to_glob_map` and
  `find_transitive_imports`. Merge: the same entry-and-max, union, and in-order replay into the
  `IndexMap`/`IndexSet`s, which keeps their first-insertion order. The two hash tables are only
  probed afterwards (`get`, `contains`), never iterated.
- `trait_impls`: another write the old inventory missed (`late.rs`, impl with a trait). Now
  `out.trait_impls.push((trait, impl))`, replayed in unit order.
- `doc_link_resolutions` / `doc_link_traits_in_scope`: see "When it stays serial". The visitor
  owns `doc_links: LateDocLinks`; the merge unions first-wins in unit order (a no-op in both
  modes today: serially the tables travel from unit to unit and go back to the resolver after
  the loop, and the stage only runs with doc links off).
- Already in `LateUnitOutput` from the previous pass: `use_injections`, `unused_labels`,
  `confused_type_with_std_module`, `potentially_unnecessary_qualifications`,
  `delegation_infos`.

### D. The node id counter (step 3)

`LateResolutionVisitor::next_node_id` / `next_node_ids` replace the resolver's for the three
late sites (`resolve_elided_lifetime`, `create_fresh_lifetime`,
`resolve_elided_lifetimes_in_path`). Every unit numbers from the resolver's frozen
`next_node_id`, which is above every existing id, so a unit's ids never collide with real ones,
and returns how many it used (`LateUnitOutput::node_ids`). `merge_late_units` gives unit `i`
the offset `sum of counts before i`, advances the resolver's counter by the unit's count
(`Resolver::next_node_ids`, same overflow check), and `merge_late_owner_tables` adds the offset
to every id at or above the frozen start in the three places a conjured id is stored: keys of
`lifetimes_res_map`, `LifetimeRes::Fresh { param }` and `ElidedAnchor { start, end }` in its
values (and `Param { binder }`, a no-op since binders are real ids), and the parameter id in
`extra_lifetime_params_map`. Unit 0 has offset 0 and its maps move as they are. Nothing else
allocates node ids during late resolution, so the ids come out as the single walk numbered them.
A new place a conjured id is stored has to be added to `merge_late_owner_tables`.

### E. Lazily filled state inside the module graph (step 4)

- Tracked `RefCell` borrows. `FrozenFlag` (`mod.rs`, `ref_mut::frozen`) is a flag of its own,
  not the speculative one: `CmRefCell::borrow_checked` returns `CmRef::Untracked` when either is
  set, and `writes_allowed` (neither set) is asserted by `CmRefCell::try_borrow_mut_checked`
  (and so `borrow_mut_checked`), `CmRefCell::take` and `CmCell::set_checked`. The `&mut Resolver`
  write paths (`borrow_mut`, `borrow`, `CmCell::set`) cannot be reached from a `&Resolver`.
  Late resolution's reads in `late/diagnostics.rs` that used `CmRefCell::borrow(&mut Resolver)`
  now use `borrow_checked`.
- `ModuleData::traits`: `ensure_traits` now reads first and takes the mutable borrow only when
  empty; `prepare_frozen_late_resolution` calls it for every module in `local_modules` (blocks
  included) before the stage, so `CmResolver::traits_in_module` writes nothing there.
- `macro_rules` path compression: `compress_macro_rules_scopes` (`late.rs`) compresses every
  scope late resolution can reach before the stage, walking from `macro_rules_scopes`,
  `output_macro_rules_scopes`, `invocation_parent_scopes` and every import's parent scope by the
  steps `visit_scopes` takes (definition to its parent scope, unexpanded invocation to its
  invocation scope), each scope once. `visit_scopes` keeps compressing lazily everywhere else and
  asserts (a hard `assert!`, not a debug one) that it never rewrites a scope while frozen:
  skipping the rewrite would change what the rest of the walk reads, and doing it would be a
  write other threads race with.
- External modules and macros (`get_module`, `get_macro_by_def_id`,
  `extern_prelude_get_flag`): `debug_assert!`s that none of them is reached while frozen. They
  cannot be: the stage only runs when no crate is loaded and no `--extern` flag is pending.
- `hir_arena.alloc_slice` (`traits_in_scope`, `find_transitive_imports`) and the resolver's
  `arenas`: both `WorkerLocal`, so allocation from a worker is fine.

### F. Diagnostics

Emitted through the session `DiagCtxt`; inside a stage item they are collected by the item hook
and replayed in item order. Stashed diagnostics (`late.rs`, `StashKey::CallAssocMethod` and
`StashKey::AssociatedTypeSuggestion`) are only stolen by later passes, not by another unit.

### G. `&mut` that wrote nothing

`resolve_ident_in_lexical_scope` is a `CmResolver` method now (`ident.rs`); with no `Finalize`
it writes nothing, so `report_path_resolution_error` calls it through `cm()`.
`report_path_resolution_error` is `CmResolver::report_path_resolution_error(&self)`, only for
the `pat_span` read. `legacy_const_generic_args` is `&self` and takes the sink.

## What late resolution reads (the frozen view)

Everything below is written by the time the stage starts and not written by it: the module
arenas and every `ModuleData` (resolutions, parents, `no_implicit_prelude`, glob importers,
`traits` after the prepare step), `graph_root`, `empty_module`, `local_modules`,
`local_module_map`, `block_map`, `prelude`, `extern_prelude`, `macro_use_prelude`,
`builtin_type_decls`, `builtin_attr_decls`, `registered_attr_tool_decls`, `macro_rules_scopes`
(and every scope cell, compressed), `output_macro_rules_scopes`, `invocation_parent_scopes`,
`local_macro_map`, `field_names`, `field_defaults`, `field_visibility_spans`, `struct_ctors`,
`struct_generics`, `item_generics_num_lifetimes`, `item_required_generic_args_suggestions`,
`delegation_fn_sigs`, `owners` (read in place), `partial_res_map`, `pat_span_map` and
`ambiguity_errors` as early resolution left them (read after the unit's own), `next_node_id`,
`effective_visibilities`, `mods_with_parse_errors`, `glob_error`, `proc_macros`,
`stripped_cfg_items`, `on_unknown_data`, `features`, `arenas`, and `tcx`.

## When it stays serial

`Resolver::late_resolution_can_freeze` is true when all of:

- no external crate is loaded (`CStore::iter_crate_data` is empty) and no `--extern` flag is in
  the extern prelude: external modules and macros are materialised on first use, and a frozen
  view would have to materialise everything reachable up front (unbounded, it is the whole of
  `std`) or keep a synchronised on-demand table (a cache with a lock). With no sysroot named,
  the default, there is nothing external.
- doc links are off (`ResolveDocLinks::None`, the default): `resolve_doc_links` uses
  `doc_link_resolutions` and `doc_link_traits_in_scope` as a cache across the items of a module,
  and in the case its own FIXME names (shadowing `macro_rules`) a later item answers from an
  earlier item's entry where resolving it again could differ, which changes which prefixes get
  resolved and so the tables' contents. To stay byte-identical, the serial loop moves the tables
  from each unit into the next, which is exactly the single walk's cache.

Otherwise the loop runs the same `resolve_late_unit` in order, with the resolver shared, not
frozen (tracked borrows, lazy writes allowed), writing into the same sinks, merged the same way.

## Turning the loop into a stage: the five steps, done

1. **Owner tables into the unit** (A): `LateOwner`, `LateOwnerTables`, the visitor's
   `with_owner`, `merge_late_owner_tables`.
2. **A late sink** (B, C): `LateSink`, `CmResolver::Late`, the accessors, `merge_late_sink`.
3. **Per-unit node ids** (D): the visitor's counter, `LateUnitOutput::node_ids`, the renumbering.
4. **Frozen mode** (E): `FrozenFlag`, `prepare_frozen_late_resolution`, the asserts.
5. **The stage**: `LateResolutionVisitor::r: &'a Resolver`, `resolve_late_unit(&self, ..)`,
   `run_stage(&units[..], units.len(), ..)` in `late_resolve_crate`, the merge serial and in
   unit order.

## What cannot be parallel, and why

- **`ItemInfoCollector`**: writes per-item facts every unit reads (lifetime counts of other
  items, delegation signatures). A whole-crate pre-pass by design, and cheap (no lookups).
- **Building the units and the `macro_rules` sequence**: a serial prefix scan.
- **The prepare step, the merge, `report_errors`, `check_unused`**: they produce the frozen view
  or consume the merged, ordered result. `report_with_use_injections` deduplicates `use`
  suggestions across the crate and `report_privacy_error` across errors, which is cross-unit by
  definition.
- **External crates and doc links**: see "When it stays serial".
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

1. Build (`cargo check`, and with the `parallel` feature).
2. The corpus diagnostics must be identical to the parent commit's (the serial unit loop): run
   `check_source` over the crate's own 1,423 files at one worker on the parent and on this tree,
   and diff the rendered diagnostics. Then the same at N workers against one worker: the diff
   must be empty.
3. The same diff with a sysroot named (serial fallback) and with `-Zunstable-options
   --resolve-doc-links=all` or rustdoc (serial fallback, doc links).
4. A debug build of the corpus run exercises every frozen-mode `debug_assert!`; the
   `macro_rules` compression assert is a hard one and runs in release too.
5. The `late_resolve_crate` timer (`resolve_crate`) at one worker should be within noise of the
   parent (the prepare step fills every module's trait list and compresses every scope, eagerly;
   the merge moves each table once), and at N workers is the measurement of what the stage
   bought.

## Unsure it compiles

Places the type checker has to confirm, most likely first:

- `run_stage(&units[..], units.len(), |units, index| this.resolve_late_unit(krate, root_scope,
  units, index, LateDocLinks::default()))` in `late_resolve_crate`: `units` is `&&[LateUnit]`
  there and relies on deref coercion to `&[LateUnit]`; the `'env` inference over a borrowed
  slice, `&Resolver` and `&Crate` captures. Also whether `stages` needs a query context
  (`ItemContext::capture`) that `resolve_crate` runs in; it should, as it runs under the
  resolver query, but it is the first `run_stage` in `rustc_resolve`.
- `late_cm!(self).method(.., &self.parent_scope, &self.ribs[ns], .., Some(&self.diag_metadata))`
  (`late.rs`, `maybe_resolve_ident_in_lexical_scope`, `resolve_ident_in_lexical_scope`,
  `resolve_path`, `record_traits_in_scope`): relies on the macro expanding to field borrows
  (`&mut self.sink`, `self.r`) disjoint from the argument borrows.
- The doc link traits closure in `resolve_doc_links`: `late_cm!(self)` and
  `self.is_invalid_proc_macro_item_for_doc` inside one `or_insert_with` closure; the
  `CmResolver` temporary is moved into `traits_in_scope` before the second borrow.
- `CmResolver::record_use` (`mod.rs`): the `found_in_stdlib_prelude` closure reads `*self`
  through `Deref` while `self` is `&mut CmResolver`, then `self.lint_buffer_mut()` after it.
- `CmResolver::traits_in_module`: `module.traits.borrow_checked(&**self)` kept alive across
  `self.find_transitive_imports` (`&mut self`); the `CmRef` borrows the module, not `self`.
- `merge_late_owner_tables`: `UnordItems::map` with a `move` closure calling a `Copy`
  `renumber` closure, and `ExtendUnord::extend_unord` inferring `(NodeId, V)`.
- Method resolution where a `CmResolver` method shares a name with a `Resolver` one
  (`record_use`, `record_partial_res`, `resolve_ident_in_lexical_scope`): inherent methods of
  `CmResolver` are found before auto-deref reaches `Resolver`'s.
- `CmResolver` and `LateSink` are private items of `rustc_resolve` with `pub(crate)` methods,
  used from `late`, `late::diagnostics`, `ident`, `macros` and `diagnostics::impls`; the
  previous `CmResolver` was a private alias of a `pub(crate)` enum.
- From the previous pass, still true: `into_output` moves `unused_labels` out of
  `*diag_metadata` (a `Box`), and `ParentScope { .., ..root_scope }` builds a parent-module
  struct from a child module.

## Why it did not scale on `src/` (speed-01 item 1)

Static check, for the `no_core` sessions of `check_source_with_width` and `parallel_timing`:
the stage engages. `-Zcrate-attr=no_core` is injected at parse (`rustc_interface/passes.rs`,
`cmdline_attrs::inject`), before the resolver is built, so the extern prelude gets no `core` or
`std` flag entry; no `--extern` is set; doc links are `ResolveDocLinks::None`; nothing loads a
crate except an `extern crate alloc;`/`extern crate std;` item (one file each in `src/`, and the
load fails against a mismatched or absent sysroot). The width is latched by `run_compiler` on the
session thread, which is the thread `resolve_crate` runs on, so `is_parallel_here` is true there
and `run_stage` opens a parallel scope.

So the units did run on the pool, and the time went into locks every unit shares:

- `Symbol::as_str` took the session's symbol interner `Lock` (a mutex in a parallel session)
  on every call. Typo suggestion calls it once per candidate name per unresolved name, and a
  `no_core` file has an unresolved name at every `Option`, `Some`, `Vec`, `String`. Every worker
  took one mutex on one cache line per candidate. Reads are now lock-free (`SymbolStrs`,
  `rustc_span/symbol.rs`): an append-only table of doubling buckets, published by a `Release`
  store of its length under the interner's lock and read after an `Acquire` load.
- `SyntaxContext::adjust`, `normalize_to_macros_2_0_and_adjust`, `outer_expn`, `edition` and
  `hygienic_eq` took the hygiene `Lock` even for the root context. `visit_scopes` calls `adjust`
  at the end of every module chain, which every unresolved name reaches, and `edition` is behind
  every `is_rust_2015`/`at_least_rust_2018`. Each now answers the root context without the lock,
  with the answer the locked path gives (the root's data is never mutated but for
  `dollar_crate_name`; its edition is kept in `SessionGlobals::root_edition`).

Left, and worth measuring next: the largest unit (a big `impl` is one unit; see "Finer units"),
`SourceMap::lookup_source_file`, which takes the files `RwLock` and clones the file's `Arc` (an
atomic on one shared line) per span lookup in the suggestion code, and `definitions` reads
(`def_key`, a `FreezeLock` not frozen until after lowering) in `is_accessible_from`.
