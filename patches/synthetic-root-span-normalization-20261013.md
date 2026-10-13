# Synthetic root span normalization boundary

## Scope

This independent patch corrects `Names::definition_span` for the crate root when analyzer-only
lang-item declarations are appended to the input. `synthetic_start` is the original input's raw
UTF-8 byte length, while the `ByteSpan` offsets returned by `Names::span` are relative to rustc's
normalized `SourceFile`. The patch validates the raw boundary against
`SourceFile::unnormalized_source_len`, maps it through `normalized_byte_pos`, and checks that the
result is contained by the file and round-trips through `original_relative_byte_pos` before
assigning the normalized relative offset. If the mapping cannot be proven, the existing root span
is retained. `synthetic_start` itself remains unchanged for `synthetic_span` taint checks.

A leading BOM and ordinary CRLF sequences are mapped to their normalized boundaries. If the
original input ends in a lone carriage return, appending the separator newline makes a CRLF across
the raw boundary; that boundary is not representable in the normalized file, so the round-trip
check declines to clip the root span.

## Applicability

- Repository: `/Users/revenge/code/frontend`
- Base HEAD: `5bb739921e1607fa958c24230e715fd13c671077`
- Exact dirty base SHA-256 for `src/frontend_facts/mod.rs`:
  `e577fe6ce7e488f58885101cb545f45dc79dcd5e7b9ceee873baceb09cea5b3c`
- SourceFile API SHA-256 for `src/rustc_span/mod.rs`:
  `485c2e69e9ac01261a138f62a2916d8ca8b76c5c49b5a5da07a8e6564a6dd0dd`
- Patch SHA-256:
  `ed9deea8fc2ed07251d8f5fb164715552abde62fa664be6868c2cdc701e745ee`
- Artifact: `synthetic-root-span-normalization-20261013.patch`

## Verification

`git apply --check --verbose patches/synthetic-root-span-normalization-20261013.patch` passes
against the exact dirty source hash above. No source file was changed, and no build, compiler, or
test was run.
