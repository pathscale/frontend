# A no_std rustc frontend, with no LLVM

rust-lang/rust's compiler front half, with two things removed:

- **the codegen backends.** No LLVM, no cranelift backend, no object writer, no linker driver, no
  rlink files, no compiled-module model. This produces MIR and stops.
- **the standard library.** Every crate here is `#![no_std]` and none names `extern crate std`.
  The operating system arrives through [`ekostd`](https://crates.io/crates/ekostd), a `no_std`
  wrapper over `libc`.

What is left is parsing, expansion, name resolution, type checking, trait solving, THIR, MIR
construction, MIR optimization, borrow checking and monomorphization collection. It is upstream's,
substantively unchanged: the MIR data model, the MIR that gets built, and the MIR pass pipeline are
all identical to the commit named in [UPSTREAM.md](UPSTREAM.md).

## Why you might want it

A frontend you can call as a library, that hands you MIR, that links no `libstd`, and that you can
run inside a resident process rather than a batch invocation.

If you want to build a compiler backend, a static analyser, or anything else that needs Rust's
semantics without wanting Rust's code generation, this is that half on its own.

## Building it

```sh
cargo check --workspace
```

That is the whole build. There is no bootstrap, no `x.py`, no Python.

## Using it

Add the crates you want as ordinary dependencies. **Nothing else is required**: no environment
variables, no `.cargo/config.toml`, no build wrapper.

```toml
[dependencies]
rustc_interface = { git = "...", rev = "..." }
rustc_middle    = { git = "...", rev = "..." }
```

Upstream's build wants a set of `CFG_*` variables that only bootstrap sets, and upstream's
`rustc_macros` refuses outright to compile without `RUSTC_BOOTSTRAP`. Both would have made every
consumer discover, through a build-script panic in a crate they never named, that they were
supposed to know a variable name. So the defaults live in this repository instead - in three build
scripts and one proc macro - and every one of them still yields to a value you set yourself.

The `.cargo/config.toml` here configures *this* workspace's own build. Cargo does not apply it to
dependents, and dependents do not need it.

## Running it needs a sysroot, and this is the part that surprises people

**A matching vintage is mandatory, and no published nightly can supply one.** The sysroot has to
have been built from the exact upstream commit in [UPSTREAM.md](UPSTREAM.md). That pin sits
*between two nightlies*, so both directions fail, and no failure says "wrong sysroot":

| sysroot relative to this fork | what you get |
| --- | --- |
| older | an assertion inside `rustc_serialize`, which reads as a corrupt file |
| newer | an `ExplicitBug` in `rustc_hir_typeck`, usually "expected associated item for operator trait" |
| any other commit, version check silenced | a bare `error[E0463]: can't find crate for `std`` |

The version string is the smaller half of this. The larger half is that crate metadata encodes
every preinterned symbol as a bare index into the `symbols!` list in `rustc_span::symbol`
(`SYMBOL_PREDEFINED`). Upstream adds and removes entries in that list continuously, and an index
written by one commit means a different string under another. When the crate name recorded in
`libstd`'s metadata decodes to the wrong symbol, `crate_matches` rejects the file without
recording a rejection, so the diagnostic is a bare `E0463` with no note about versions at all.
Crates whose names sit before the first divergence still load, so the failure is partial: `core`
and `alloc` can come up while `std`, `test` and `unwind` do not.

`rustc_version_of_sysroot` therefore does **not** make a published nightly usable. It silences the
version check and leaves the symbol table wrong, turning an `E0514` that names the problem into an
`E0463` that names nothing. Enable the `force_pinned_sysroot` cargo feature to keep the
compiled-in `CFG_VERSION` and refuse any other vintage.

So the library has to be built from the same upstream commit:

```sh
./scripts/build-sysroot.sh
```

It clones nothing you have not already got and takes about twenty seconds once the upstream
checkout is present. Read the script before running it; it says what it needs and why.

`CFG_VERSION` in `.cargo/config.toml` is the string that has to match, and the script prints the
one your sysroot actually carries and tells you whether they agree.

If you build that sysroot somewhere the compiled-in default does not describe, you can read the
string out of it instead of writing it down twice:

```rust
config.rustc_version = rustc_interface::util::rustc_version_of_sysroot(&sysroot);
```

That is for a sysroot built from the pinned commit under a different `CFG_VERSION`. It is not a
way to accept a sysroot from a different commit: see above, the version string is not the thing
that has to match.

## Deliberate differences from upstream

Beyond the removals, three behaviours differ and are worth knowing before you file a bug:

- `TargetUintError` replaces `io::Error` in `read_target_uint`/`write_target_uint`. The slice
  `Read`/`Write` impls are std-only.
- `RUSTC_CTFE_BACKTRACE` is inert. It needed `std::backtrace`.
- The double-panic guard in the metadata encoder is gone. It was `std::thread::panicking()`, which
  is always `false` under `panic = "abort"`.
- `-Znll-facts` and `-Znll-facts-dir` are removed. The writer they fed was deleted, so the flag
  bought a full fact-gathering pass and then dropped the result.

`-Zdump-mir` and friends write into an in-memory sink on the `Session` rather than to files, since
the intended caller is a program rather than a person at a terminal.

## Licence

Apache-2.0 OR MIT, upstream's. See `COPYRIGHT`, `LICENSE-APACHE` and `LICENSE-MIT`.
