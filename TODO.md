# frontend: the ordered work

Read `AGENTS.md` first. Rule zero: this is a parser and a plain library. It needs no
sysroot, `std` is parsed like any other code, and nothing depends on nightly.

## 1. Build on stable 1.97.1 with no unstable features: done (`feat/stable-toolchain`)

Owner ruling, 2026-09-23: nothing may depend on nightly. `RUSTC_BOOTSTRAP`, every
`#![feature]` and the nightly pin are gone, and `rust-toolchain.toml` is 1.97.1.
`cargo check --workspace` exits 0 on it (`6ab46df`, then `a36ccc3`). Never add a gate back
to make an error go away: port the code.

- The global context and both arenas are freed, not leaked. `FreeOnDrop` in
  `rustc_interface/passes.rs` makes upstream's `Anchor` promise on stable (see the
  comment there).
- `frontend_panic_runtime` is deleted. It needed nightly (the `eh_personality` lang item and
  `alloc::panicking`), and stable gives a `no_std` binary no way to unwind. A binary on stable
  links `std`, which brings unwinding and `catch_unwind`, or sets `panic = "abort"` and supplies
  a `#[panic_handler]`, as `link-probe/` does. The published 0.1.0 stays on crates.io for
  EKOPathRS, which is still on a nightly toolchain.
- About 3,700 warnings remain, nearly all unused imports. The one real defect they
  pointed at, `Allocation::as_mut_ptr` recursing, is fixed. Treat
  `unconditional_recursion` and `unreachable` warnings as bugs.

What the port changed, which matters to callers:

- **Panics.** `unwind_janky` uses no nightly intrinsic. A consumer that needs a refused
  program to come back as `Err`, not an abort, calls `unwind_janky::install_catcher`
  once at startup with a wrapper around `std::panic::catch_unwind`.
  `frontend_facts::analyze_source` asserts that one is installed.
- **`newtype_index!`** emits a plain `u32`. The niche is gone, so `Option<Idx>` is 8
  bytes, and index types no longer implement `Step`: iterate over `a.index()..b.index()`.
- **`TypedArena`** is `Vec<Vec<T>>` chunks with no `Drop` impl.
- **Derive diagnostics** from `frontend_macros` fold notes and helps into the error text.

## 2. `check_source`: type check, borrow check and lints on a source string

Done: `check_source` in `src/frontend_facts/mod.rs`, and `frontend-facts --check`. It
establishes `compiles` and `linted` for a program by reading it, in process: it runs
`tcx.analysis(())` (typeck, borrowck, then the builtin lints, which are skipped if either
failed) and returns every error and warning, captured through `PlainEmitter`.

Open: sema needs the definitions the program uses. Under `no_core` nothing defines `Sized`
or `println!`, so every body is refused ("requires `sized` lang_item"). The library crates
are code like any other and get parsed and analysed like any other; that is the next
piece of work.

Proof when done: a small program that uses `str`, iterators and `println!` returns zero
errors, and a planted type error returns exactly that error.

## 3. More checks

rust-analyzer began as a fork of rustc. Its diagnostics, lints and assists can be ported
in as source with some refactoring. The `ra_ap_*` crates stay ruled out as dependencies.
