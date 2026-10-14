# Changelog

## 0.7.4 - 2026-10-14

This is a patch release following 0.7.3.

- Build as a registry dependency: `build.rs` now sets `CFG_RELEASE`, `CFG_VERSION`,
  `CFG_RELEASE_CHANNEL` and `RUSTC_INSTALL_BINDIR` (the caller's value, else this checkout's
  default). They were set only by `.cargo/config.toml`, which is not packaged, so 0.7.3 did not
  compile as a dependency.
- Callable source attestation no longer panics in release builds. `crate_metadata_hash` is
  empty for the session's own crate when the session writes no metadata, in debug and release
  alike; dependencies keep the hash from their metadata.

## 0.7.3 - 2026-10-14

This is a patch release following 0.7.2.

- Extract expression and constructor-pattern observations from ordinary nested closures using
  each body's type-check results. Unsupported nested owners and type identities retain gaps.
- Add `analyze_bodies_type_identity_with_loaded` for exact body requests in input order.
  Compiler observations are isolated per body, including diagnostics, uncomputed constants,
  coverage and fatal status. Empty batches create no compiler context.
- Use replayable on-demand lexing for eligible serial source parsing, including ASCII raw
  identifiers. Preserve eager fallback for recovery, normalization, symbol recording and active
  parallel parsing. Structural lookahead can skip groups without building their token trees.
- Add an explicit `frontend-facts --max-input-bytes N` stdin budget. N is a positive decimal
  byte count. Exceeding it reports `CAP_INPUT_BYTES_EXCEEDED` and exits 78. The flag rejects
  disk mode and does not claim limits on module or dependency reads.
- Preserve the 0.7.2 exhaustive desugaring enum and JSON variant vocabulary. Macro-expanded
  expression types retain coverage gaps rather than introducing a new variant.
- Keep the published `frontend-facts` binary, allocator feature and public dependency type
  identities. Calls without the new stdin option retain their existing behavior.
- Correct the dependency example and remove workflow wording from public audit notes.
