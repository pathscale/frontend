# Candidate macro provenance source facts

This is a source-only review patch for the public `frontend` checkout. It adds compiler-resolved identities for macro invocations observed while walking one selected function body's HIR expansions, plus metadata-verified source digests for external declarative macro definitions that can be uniquely anchored as an AST `MacroDef`. It does not change the existing `MacroExpansion` type/projection gaps.

## Patch base

- Repository HEAD: `5bb739921e1607fa958c24230e715fd13c671077`
- This checkout has active, uncommitted callable source-attestation and trait-evidence work. The patch is based on those working bytes, not the HEAD tree alone.
- `src/frontend_facts/mod.rs` SHA-256: `e577fe6ce7e488f58885101cb545f45dc79dcd5e7b9ceee873baceb09cea5b3c`
- `src/frontend_facts/callable_source_attestation.rs` SHA-256: `a468ae7986e550ecf68e68e10cd2c4c6e20a0dbaedc763a5f447e5bf7833c49a`
- Artifact: `macro-provenance.patch`

The patch passed `git apply --check` against the current frontend working tree. No source file was modified; no Cargo, rustc, build, or test command was run.

## Evidence semantics

Each emitted fact joins a direct macro call site whose span is unexpanded and maps exactly to source bytes in the selected body, with `StableDefPathIdentity` and the defining crate's stable ID and metadata hash. `compiler_cfg_version` is rustc's active `cfg_version` string; its contents are not guaranteed to identify the exact compiler binary or commit. `target_triple` is the active analysis session target, not an independently attested target for an external macro crate. Where an external definition is a uniquely anchored AST `MacroDef`, `definition_source_attestation` hashes the exact item bytes from rustc's metadata-verified `SourceMap`. The producer consults only the current `TyCtxt` and source map; it does not search for or initialize a sysroot. Missing `macro_def_id`, non-macro definitions, unavailable or unsupported definition source, unmappable call sites, out-of-body spans, conflicting expansions at one source span, and call sites that still carry expansion context remain explicit gaps or `Unknown` facts. Expansion marks are visited root-to-leaf and emitted facts are deduplicated by mapped source span.

Coverage is deliberately narrower than “all source-written nested invocations”: a source-authored nested macro invocation forwarded through an outer macro can still have expansion context, so this implementation records a gap instead of claiming an independently recovered exact span for it. The old macro type gap remains intact.

Coverage is intentionally limited: local macro definitions, procedural-macro function definitions, and external definitions that are unavailable or cannot be uniquely anchored as an AST `MacroDef` remain `Unknown` for source attestation. Other builtins are Unknown unless they also have a uniquely anchored external `MacroDef` source. A source-authored nested macro invocation forwarded through an outer macro can retain expansion context, so the walk records a gap rather than claiming an independently recovered span. The reported invocation list is an observation set, not a complete inventory; absence means unknown.

The context does not provide Cargo package ID, the complete effective cfg set, the selected dependency graph, the exact compiler binary identity beyond `cfg_version`, or an external proc-macro's distinct host target. It cannot prove target-specific `core` identity or that a resolved `core` macro is the selected target's sysroot `core`. It does not add AgentCode protocol projection, Karen consumption, or rust-analyzer parity. Those remain separate acceptance blockers.
