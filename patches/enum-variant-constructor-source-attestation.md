# External enum-variant constructor source attestation

## Review basis

- Frontend HEAD: `5bb739921e1607fa958c24230e715fd13c671077`.
- AgentCode HEAD: `10ac6b0aa9449f3e5cf4e4cfb1674b142f1b3479`.
- Karen HEAD: `323178910aed492872c19e9a3d73c0e505d9e386`.
- The frontend callable proof modules are currently untracked working-tree inputs. The patch is based on the exact current bytes of `src/frontend_facts/callable_source_attestation.rs`, SHA256 `a468ae7986e550ecf68e68e10cd2c4c6e20a0dbaedc763a5f447e5bf7833c49a`; apply after that source is admitted to the working tree. No source file was edited by this review.
- Other reviewed current source hashes: `callable_trait_evidence.rs` `2268d812576bc0e03368493d6efddb2ab5d17481001f9cfbc2824345af02a81d`; `frontend_facts/mod.rs` `e577fe6ce7e488f58885101cb545f45dc79dcd5e7b9ceee873baceb09cea5b3c`; `tests/body_type_facts.rs` `d307db2a7c8af5179d07bf132eff07931eed1f1076999b4c2c4817ad0f663d6f`; AgentCode `source_analysis.rs` `ae582bc64de531f5badc49e5bcd1d72d9602fb3709fe3458a81fc4f6bb4d8e2e`; AgentCode protocol `analysis.rs` `799c813f6bc0d6980a3b604fd773795a0531b811f053a1c18c344b9c5e1326e2`; Karen `typecheck_gate.rs` `8b39de4b94b6ebc62d0c89e0bb71cf09b52a05ba65100fbf5b9665f956a52d50`; Karen `string_split_consumer.rs` `0bd228a22f48447400e63bdaf7e0972907c0e566ae3377506ea04ae33fa81f54`.

## Concrete gap

`attest_definition` accepts `DefKind::Ctor(..)` as a callable. For external definitions it then calls `external_item_range`, which only offers candidates from top-level `Item` and `AssocItem` visits. `DefinitionFinder::push_item` sees an enum's `ItemKind::Enum` name, not an individual variant's identifier; the visitor has no `visit_variant` override. A compiler-resolved external tuple or unit variant constructor such as `Option::Some` therefore has a matching compiler identifier span but no parser-AST candidate, and its source attestation is reported `Unknown`.

That Unknown is observable downstream: Karen's `CallableSourceCompleteness` is a whole-candidate gate (`string_split_consumer.rs:423-432`). Thus an external variant constructor anywhere in the analyzed candidate can block a proof even when that constructor is not the contract's callable. The existing frontend body-facts fixture already exercises an external `Option::Some` constructor at `tests/body_type_facts.rs:373-394`, but it checks variant/type identity, not callable source attestation.

## Bounded correction

The patch adds an AST `visit_variant` hook. It sends the variant's exact identifier span and enclosing variant span through the existing `item_matches` checks, then delegates traversal to `walk_variant`. It does not select by spelling: the candidate must match the compiler identifier span and be contained by the compiler definition anchor. It is restricted to `DefinitionSourceKind::Callable`; trait and associated-item evidence is unchanged. No parent enum is inferred from names.

## Identity DTO review

AgentCode's candidate-body path preserves the frontend identity DTO as JSON: `source_analysis.rs:1082-1084` serializes `report.identity_facts`, and `:1126-1127` forwards it unchanged as `body_type_identity_facts`. The separate `SourceFactsDefinition.identity_complete` flag is documented as freedom from injected-placeholder taint, not package/global identity (`agentcoder-protocol/src/analysis.rs:831-839`); I found no code path in this reviewed DTO that promotes it into a stable compiler identity. The strong callable identity schema remains the frontend `StableDefPathIdentity` and Karen validates its scheme/hash encoding.

## Verification status

Static code-path review only. No build, compiler, test, or source mutation was performed. The review patch is not integrated or accepted. Root should run the existing narrowly scoped external constructor-attestation evidence after integrating the source change; no broad suite is called for by this finding.
