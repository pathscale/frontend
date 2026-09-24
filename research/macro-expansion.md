# Macro expansion and macro finalization

Source-only pass; nothing here is built or measured yet.

## `finalize_macro_resolutions`: a stage

Each recorded macro resolution (multi-segment paths, then single-segment names, then built-in
attribute names, in recording order) is one unit, `CmResolver::finalize_macro_resolution`
(`src/rustc_resolve/macros.rs`). Through `CmResolver::Mut` it is the old loop body; through
`CmResolver::Late(&Resolver, &mut LateSink)` every write goes to the unit's sink and the sinks
are merged with late resolution's `merge_late_sink`, in unit order. Diagnostics are emitted from
the unit and replayed in unit order by the stage.

What the units write: `record_use` (import use, glob map, `PRIVATE_MACRO_USE`, deduplicated
ambiguity errors), the finalizing lookups' ambiguity and privacy errors and
`macro_expanded_macro_export_errors`, the `LEGACY_DERIVE_HELPERS` lint. All are `LateSink`
fields, so the sink is reused unchanged. `unresolved_macro_suggestions` is now a `CmResolver`
method (its `record_use` goes to the sink); its lazy `register_macros_for_all_crates` moves to
the caller (in the stage, before freezing; with no crate loaded it only sets its flag).

Before freezing: `prepare_frozen_late_resolution` (trait lists, `macro_rules` compression) plus
`compress_macro_rules_scopes_from` over every unit's parent scope.

`resolve_macro_or_delegation_path` no longer records into the two resolution lists through a
`Late` resolver: only passes after finalization took the lists use one, so those pushes were
dead (a unit reaches it through a derive helper's `DeriveHelpersCompat` scope).

### When it stays serial (`macro_finalization_can_freeze`)

- An external crate is loaded or an `--extern` flag is pending (as late resolution).
- No error counted yet, or `-Zeagerly-emit-delayed-bugs`. The consistency check reads
  `has_errors`, `ambiguity_errors` and `privacy_errors`, which earlier units change (an error
  emitted inside another unit's lookup included), so it is order dependent. With an error
  already counted it is a no-op: no stash, and its delayed bug is dropped on arrival. Most
  corpus files have errors by this point (unresolved imports are reported in
  `finalize_imports`).

## `finalize_imports`: left serial

Its loop calls `finalize_import` then `import_dummy_binding` per import; the dummy binding is a
write into the module graph that later imports' finalization reads. Not the late shape, and
not in the measured hot list.

## Macro expansion (`rustc_expand::expand::fully_expand_fragment`): nothing proven independent

Nothing was parallelised. Per piece:

- **The fixpoint loop.** Invocation `i + 1` is resolved (`resolve_macro_invocation`, a resolver
  write) after invocation `i`'s output was collected and integrated
  (`visit_ast_fragment_with_placeholders`: def collection and the reduced graph, which define
  names and `macro_rules` scopes that `i + 1` may resolve to). Undetermined invocations retry
  on what earlier ones defined. Inherently serial.
- **Expanding a batch of resolved invocations.** The transcriber applies hygiene marks
  (`apply_mark`), interning `SyntaxContext`s into the global `HygieneData` in first-use order.
  Those ids are observable (they are part of `Ident` hashing and equality in `FxHash` maps,
  and so of iteration order downstream). Reordering them needs a renumbering pass over every
  span the fragment carries: not a stage over frozen input.
- **`InvocationCollector`'s walk** (the whole crate on the first call, the bulk of the pass on
  real code). It assigns `NodeId`s from one counter in pre-order (`visit_id`, `assign_id!`),
  allocates `LocalExpnId`s (`fresh_empty`), strips `cfg`s (which deletes nodes, so a subtree's
  id count is known only after its walk), buffers lints into the shared `psess`, and calls the
  resolver (`insert_impl_trait_name`, `register_glob_delegation`, `append_stripped_cfg_item`).
  Late resolution's per-unit id offsets would work for the `NodeId`s alone, but `ExpnId`s are
  numbered by the global hygiene table in allocation order and the resolver calls are writes.
  A per-item split would need owned per-item output for all four plus an ordered merge into
  the hygiene table; not attempted.
- **`visit_ast_fragment_with_placeholders`**: def collection assigns `LocalDefId`s in walk
  order (observable in every later table) and defines names in modules. Serial.
- **Derives of independent items**: each is an invocation in the loop above, with the same
  resolver and hygiene writes.

The candidate for later: the first `InvocationCollector` walk over the crate, split per
top-level item, if `LocalExpnId` allocation and the resolver calls it makes can become owned
per-item output renumbered in the merge (the late `NodeId` scheme, extended to `ExpnId`).
Measure its share of the 756 ms first.
