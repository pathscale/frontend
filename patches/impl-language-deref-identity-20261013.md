# Frontend Deref nominal provenance producer

## Integrated checkpoint, 13 October 2026

Root integrated the full source-digest successor (SHA256
`5d086a4238e4181ffea00dbd2d36092b2cb68f824d5ddc74ad72df7c88afcfb8`)
and normalized root-span repair (SHA256
`ed9deea8fc2ed07251d8f5fb164715552abde62fa664be6868c2cdc701e745ee`).
Reverse application checks pass against the current source. These incorporated
patches and their superseded producer drafts are swept from the queue.
AgentCode consumer R5 build/source guards pass; one MCP direct-local field
query returns count, usize and documentation in0.17s. Evidence lives at
`/Users/revenge/code/codegen-int/one/frontend-receipt-consumer-20261013-r5`.
This is no Deref, snapshot/module or Rust Analyzer parity acceptance.
The queued-only paragraphs below describe the original review state.

## Scope

This queued producer patch adds compiler-backed, optional facts for a conservative local Deref
join. It provides:

- `Impl.is_language_deref: Option<bool>`, set only by comparing the resolved trait `DefId` with
  `tcx.lang_items().deref_trait()` in the same `TyCtxt`. `None` remains unknown.
- `Impl.resolved_self_nominal_type_source_attestation` and
  `Impl.resolved_deref_target_nominal_type_source_attestation`, only when rustc resolves the
  respective type directly to an untainted, source-attested ADT. The `Deref::Target` mapping is
  checked against the exact trait item and impl container. Generic arguments are not attested.
- `Definition.nominal_type_source_attestation` and
  `ModuleName.target_nominal_type_source_attestation`, plus
  `ModuleName.target_is_local: Option<bool>` from the exact resolved target `DefId.is_local()`.
  Only `Some(true)` may support a same-response local-definition join.
- `CrateFacts.source_context`, `source_digest_scheme`, and `source_content_digest`. The context
  reuses the current callable source-attestation receipt. The digest is for the normalized root
  input file only.
- Public `frontend_facts::source_capture_digest(&str) -> Option<String>`, using rustc
  `SourceFile` normalization (BOM removal and CRLF normalization) and the existing BLAKE3 digest
  representation.

Every new serialized field is `Option` with serde defaulting; unavailable, synthetic, non-ADT,
error, or tainted evidence remains `None`. The fields do not prove Cargo package or target
selection, cfg activation, rustc/toolchain compatibility, or complete crate identity. A source
digest does not prove metadata selection. A consumer should require its own matching candidate
source/context receipt and keep imported, cross-file, or context-mismatched joins unknown. Existing
consumer APIs without a receipt must not emit fact-backed field offers. The current narrow source
binding supports only an exact candidate source matching the root input; it does not provide
per-file receipts for module files.

## Applicability

- Repository: `/Users/revenge/code/frontend`
- Base HEAD: `5bb739921e1607fa958c24230e715fd13c671077`
- Dirty producer source SHA-256 at review: `src/frontend_facts/mod.rs`:
  `e577fe6ce7e488f58885101cb545f45dc79dcd5e7b9ceee873baceb09cea5b3c`
- Dirty attestation source SHA-256 at review: `src/frontend_facts/callable_source_attestation.rs`:
  `a468ae7986e550ecf68e68e10cd2c4c6e20a0dbaedc763a5f447e5bf7833c49a`
- Patch SHA-256: `604cbb2bd4ef66ddbde4e882e38d4699c244fd413bbca7f71ccaeaec85956abc`
- Apply only to the matching current dirty source state; root retains integration ownership.

## Review status

`git apply --check --verbose patches/impl-language-deref-identity-20261013.patch` passes against
the current working tree. This is a queued patch only: no source integration, compile, lint, run,
or tests were performed. The nominal attestations reuse existing source-attestation machinery;
no sysroot search or metadata setup is introduced.
