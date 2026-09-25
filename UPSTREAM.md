# Where this came from

```
rust-lang/rust
commit 4b7e3a76d8df78960dc7c65cad43f5da1dac8ade
       2026-08-30, "Auto merge of #162028"
```

The exact commit is what matters here, more than it usually does: this frontend can only read
crate metadata written by a compiler built from the same source. `.cargo/config.toml` carries the
version string that pins the match, and `scripts/build-sysroot.sh` builds a library from this
commit so there is one to read.

Verify the pin rather than trusting this file:

```sh
grep CFG_VERSION .cargo/config.toml
```

## What was changed

Three things, and nothing else of substance.

**Every crate became `#![no_std]`.** `std` paths became `core` and `alloc` paths; the parts with
no `core` equivalent - files, paths, environment, threads, locks, time, sockets, printing - go
through [`ekostd`](https://crates.io/crates/ekostd), a `no_std` wrapper over `libc`. On macOS this
is not even a compromise: `std` itself goes through libSystem, because Apple provides no stable
syscall ABI. Where a dependency had no `no_std` path, it was replaced or vendored and ported;
those live in `vendors/`.

**The codegen backends were removed**, along with everything only they needed: the object writer,
the linker driver, rlink files, the compiled-module model, ThinLTO, `-Zprint-type-sizes`,
`-Zdump-mono-stats`, the polonius fact writer, the graphviz dumpers, the snippet emitter and the
`--explain` corpus. 372 target specifications went with them; four remain.

**Internationalised diagnostics were replaced.** `fluent-bundle` and its dependency cluster were
18 of the 42 crates forcing a `libstd` link, to run a translation system with nothing left to
translate. `compiler/rustc_diag_template` is a `no_std` reimplementation of the Fluent subset this
tree actually uses, deliberately matching `fluent-syntax` 0.12 and `fluent-bundle` 0.16 so
diagnostic output does not move.

## What was not changed

Worth stating explicitly, because it is the interesting part:

- the MIR data model. `syntax.rs`, `statement.rs`, `terminator.rs`, `visit.rs`, `basic_blocks.rs`,
  `traversal.rs`, `query.rs` and `thir.rs` carry no semantic change at all.
- the MIR that gets built. Nothing under `rustc_mir_build`'s `builder/` or `thir/` differs beyond
  import rewrites.
- the MIR pass pipeline. No pass added, removed, reordered or re-levelled; no inlining threshold or
  cost weight touched.
- `layout_of`, and the four surviving target specifications.
- the trait solver, the type system, and borrow checking, apart from the dumpers and one thing
  put back: `#[rustc_reservation_impl]` (`TyCtxt::impl_is_reservation`). Upstream removed it
  with the impl that used it when `!` was stabilized, but a library read of an older `rust-src`
  (stable 1.97's core, `impl<T> From<!> for T`) still meets it. It is read as every rustc that
  had it read it: no impl outside coherence (both solvers' impl candidates, projection, `const`
  impls), ambiguity inside it, and no overlap with anything. Without it `!: From<!>` is ambiguous,
  so every body converting out of `!` (`Result::into_ok`, alloc's in-place collect) was built with
  an error.
- the MIR interpreter, `rustc_const_eval::interpret`. `frontend_facts::evaluate` runs calls on it
  through a `Machine` of its own (`src/frontend_facts/interpreter.rs`, additive), as Miri does;
  every rule of what a program does stays the interpreter's.

## Rebasing onto a newer upstream

The `no_std` port is the expensive part and it touches most files, so a rebase is real work rather
than a merge. The order that worked was: take upstream's tree, re-apply `#![no_std]` crate by
crate and **let rustc enumerate the breakage** rather than grepping for `std::`. The prelude is
what a grep cannot see - `Vec`, `String`, `Box`, `format!`, `vec!`, `println!` name no path, and a
crate can read as clean and stop compiling the moment the attribute lands.

Whatever you rebase onto, rebuild the sysroot from the same commit and update `CFG_VERSION`
together with it. They are one decision in two files.

## rust-analyzer

```
rust-lang/rust-analyzer
commit e8f7e90aa3
licence MIT OR Apache-2.0
```

`src/frontend_facts/diagnostics/structure.rs` ports the syntax-level checks of
rust-analyzer's `ide-diagnostics` onto `rustc_ast`, with their codes, severities, messages and
tests. From `crates/ide-diagnostics/src/handlers`: `break_outside_of_loop.rs`,
`return_outside_function.rs`, `await_outside_of_async.rs`, `undeclared_label.rs`,
`unreachable_label.rs`, `incorrect_case.rs`, `useless_braces.rs`, `remove_trailing_return.rs`,
`remove_unnecessary_else.rs`, `field_shorthand.rs`, `missing_body.rs`, `duplicate_field.rs` and
`union_expr_must_have_exactly_one_field.rs`. The detection logic behind them comes from
`crates/hir-def/src/expr_store/lower.rs` (label ribs and `await` contexts), `crates/hir-ty/src/infer`
(breakable contexts, `return` outside a body, duplicate and union fields),
`crates/hir-ty/src/diagnostics/expr.rs` (trailing `return`, unnecessary `else`),
`crates/hir-ty/src/diagnostics/decl_check.rs` and `decl_check/case_conv.rs` (naming rules and case
conversion), and `crates/ide-diagnostics/src/lib.rs` (lint levels and groups). The annotation
reader in its tests follows `crates/test-utils/src/lib.rs`.

Where a check needs name resolution or types in rust-analyzer, the port keeps the part the syntax
decides; the module header of `structure.rs` lists each narrowing.
