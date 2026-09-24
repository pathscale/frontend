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

## Read this first: you do not need a standard library

**If you are about to say "no usable standard library exists", you are off track. Stop.**

This is a parser. Hand it source and it reads that source. `std` is not special: it is parsed
the same way `fmt` or anything else is, and nothing here needs a prebuilt library to exist
before it can read your code. With no sysroot named, a session runs as `no_core`: no external
crate is loaded, no prelude import is resolved, and nothing reaches for a library you did not
ask for. That is the default on purpose.

Do not clone rust-lang/rust, do not build a sysroot, do not go looking for one on disk, and do
not report a missing one as a blocker. The reasoning is in `src/frontend_facts/mod.rs`, at the
comment headed "The sysroot is an optional parameter, because this is a parser", and in commit
`b87daf4`. The sysroot section further down applies only to a caller who has deliberately
asked for paths to be resolved into a compiled library, which is rare.

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

## Integrating it: allocator and parallelism

Two choices belong to the program that links frontend, not to frontend. Both are large, and both
are easy to miss. Make them before you measure anything.

### 1. Use mimalloc as your global allocator

frontend declares no allocator: whatever your binary declares serves it. The compiler allocates
heavily (about 12 million allocations for 200 small files), so the allocator is a large share of
its time, and a larger one the more threads run.

```rust
// In your binary, once:
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

```toml
[dependencies]
mimalloc = { version = "^0.1", default-features = false }
```

Measured on the repository's clean corpus (200 files), checking every file, against the system
allocator:

| | system allocator | mimalloc |
| --- | ---: | ---: |
| one file after another | about 2,450 ms | about 2,250 ms (8% less) |
| 200 files on 12 workers | about 285 ms | about 215 ms (24% less) |

`mimalloc`'s Rust crates are `#![no_std]`; the library underneath is C, built with `cc`, and
needs an operating system for pages and thread-local storage, as `malloc` does. A `no_std`
binary on an OS (one whose allocator is `ekostd::heap::Malloc`) can swap it in the same way.

To measure your own build: `RUSTFLAGS="--cfg bench_mimalloc" cargo run --release --features
parallel --example parallel_timing -- files clean 12`, against the same without the flag.

### 2. Run files in parallel, not one file wide

A check of one file is about 45% serial (parse, macro expansion, name resolution, session setup
and teardown), so running one file's stages on many workers stops paying at about width 4:

| clean corpus, one file at a time | width 1 | width 2 | width 4 | width 8 | width 12 |
| --- | ---: | ---: | ---: | ---: | ---: |
| wall clock | 2,400 ms | 1,800 ms | 1,380 ms | 1,140 ms | 1,030 ms |

Files share nothing, so checking several at once scales almost linearly: the same 200 files on
12 workers, each file serial, take about 285 ms (8.7x), about 215 ms with mimalloc.

- **Many files:** one call per file (`check_shared_source_with_width`,
  `analyze_shared_source_with_width`, `read_crate`), each at width 1, spread over a pool, for
  example nagoya's `par_for`. Answers are identical to running them one after another.
- **Fewer files than workers** (a few large files): the same, each at width 2 to 4.
- **One large file:** width 4 is the sweet spot; wider costs CPU for little.

Width above 1 needs the `parallel` cargo feature. Workers that run sessions need a large stack
(the timing example uses 16 MiB), and every entry point needs a panic catcher installed first
(`unwind_janky::install_catcher`) and `panic = "unwind"`.

## Syntax-level diagnostics

Behind the `diagnostics` cargo feature, which is off by default:

```toml
frontend = { version = "...", features = ["diagnostics"] }
```

`frontend_facts::diagnostics::diagnose(source)` parses one crate with `rustc_parse` and runs
checks ported from rust-analyzer's `ide-diagnostics` that the syntax alone decides: `break`
outside a loop, undeclared and unreachable labels, `.await` outside `async`, `return` outside a
function body, naming conventions, unnecessary braces in `use`, a trailing `return`, an
unnecessary `else`, redundant field names, missing bodies, duplicate fields and union literals.
There is no expansion, no name resolution, no type checking and no sysroot, so it answers in one
parse and a few linear passes, which suits editors and other tooling.

It returns the parser's own errors as `Err` when the source does not parse, and otherwise
diagnostics with rust-analyzer's codes, severities and messages and byte offsets into the source.
`diagnose_with(source, &Options { codes, min_severity })` runs only the listed codes, and a check
that is not selected does not run. Nothing else in the crate calls into it. Like `analyze_source`,
it needs a panic catcher installed through `unwind_janky::install_catcher`. Provenance is in
[UPSTREAM.md](UPSTREAM.md).

## Only if you deliberately name a sysroot

You almost certainly do not need this section; see "Read this first" above. It applies only
when a caller names a sysroot on purpose so paths resolve into a compiled library.

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
