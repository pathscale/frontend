# Root source digest normalization follow-up

## Change

`input_file(tcx)` returns a `SourceFile` whose `src` already contains rustc-normalized text.
The queued producer patch passed those bytes to `source_capture_digest`, normalizing them a second
time. For input with multiple leading BOMs, the second normalization removes another BOM, so the
digest no longer represents the normalized root input.

The fix hashes `input.0.src` directly with the same BLAKE3 `SourceFileHash` encoding. It leaves
`source_capture_digest(&str)` as the raw-input helper for callers that have not already constructed
a normalized `SourceFile`.

## Patch artifacts

- Predecessor, unchanged: `impl-language-deref-identity-20261013.patch`, SHA-256
  `604cbb2bd4ef66ddbde4e882e38d4699c244fd413bbca7f71ccaeaec85956abc`.
- Layered companion: `impl-language-deref-source-digest-normalization-20261013.patch`, SHA-256
  `7ed4a335ed298f2ab4bb92d2a171c2ff63e0cae68c1cd0c83d4184450d3a610b`. Its context exists only
after the predecessor is applied.
- Full successor: `impl-language-deref-identity-20261013-source-digest-successor.patch`, SHA-256
  `5d086a4238e4181ffea00dbd2d36092b2cb68f824d5ddc74ad72df7c88afcfb8`. It preserves the
  predecessor's patch context and includes the direct hash change inline, so it can be checked as
  a single patch against the base tree.

Applicability hashes:

- Repository: `/Users/revenge/code/frontend`
- Base HEAD: `5bb739921e1607fa958c24230e715fd13c671077`
- Dirty base `src/frontend_facts/mod.rs` SHA-256:
  `e577fe6ce7e488f58885101cb545f45dc79dcd5e7b9ceee873baceb09cea5b3c`
- `src/rustc_span/mod.rs` SHA-256:
  `485c2e69e9ac01261a138f62a2916d8ca8b76c5c49b5a5da07a8e6564a6dd0dd`

## Verification

The layered companion is expected to fail when checked alone against the base tree because its
old context is introduced by the predecessor:

```text
git apply --check --verbose patches/impl-language-deref-source-digest-normalization-20261013.patch
exit code: 1
```

Passing predecessor and companion as two patch arguments also exits 1 at the companion hunk; that
command checks both against the existing worktree and does not stage the predecessor's result for
the next patch:

```text
git apply --check --verbose patches/impl-language-deref-identity-20261013.patch patches/impl-language-deref-source-digest-normalization-20261013.patch
exit code: 1
```

The full successor passes as one patch:

```text
git apply --check --verbose patches/impl-language-deref-identity-20261013-source-digest-successor.patch
exit code: 0
```

No source files were changed. No Cargo, rustc, or tests were run.
