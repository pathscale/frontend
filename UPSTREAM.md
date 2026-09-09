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
- the trait solver, the type system, and borrow checking, apart from the dumpers.

## Rebasing onto a newer upstream

The `no_std` port is the expensive part and it touches most files, so a rebase is real work rather
than a merge. The order that worked was: take upstream's tree, re-apply `#![no_std]` crate by
crate and **let rustc enumerate the breakage** rather than grepping for `std::`. The prelude is
what a grep cannot see - `Vec`, `String`, `Box`, `format!`, `vec!`, `println!` name no path, and a
crate can read as clean and stop compiling the moment the attribute lands.

Whatever you rebase onto, rebuild the sysroot from the same commit and update `CFG_VERSION`
together with it. They are one decision in two files.
