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

## 0.7.3 patch release

- Ordinary nested closure bodies are visited with their own type-check results. Unsupported
  nested owners and unresolved identities retain explicit coverage gaps.
- `frontend_facts::analyze_bodies_type_identity_with_loaded` returns exact body reports in
  request order. Each body uses an isolated compiler session, so diagnostics, symbolic constant
  losses and fatal status agree with the corresponding single-body observation. Its second
  return value is the stable crate ID from the first session that entered a compiler context.
- Eligible serial source parsers use a replayable on-demand lexer. Inputs requiring recovery,
  normalization or symbol-gallery records, and active parallel parsing, retain eager lexing.
- The existing `frontend-facts` binary accepts an optional `--max-input-bytes N` budget for
  stdin. N is a positive decimal byte count; exceeding it emits
  `CAP_INPUT_BYTES_EXCEEDED` on stderr and exits 78. It cannot be used with `--root` and does
  not bound module, metadata or dependency reads. Calls without it keep their existing behavior.

The existing public enum variants and JSON vocabulary are preserved. Macro-expanded expression
types remain explicit coverage gaps. See [CHANGELOG.md](CHANGELOG.md).

## Read this first: you do not need a standard library

**If you are about to say "no usable standard library exists", you are off track. Stop.**

This is a parser. Hand it source and it reads that source. `std` is not special: it is parsed
the same way `fmt` or anything else is, and nothing here needs a prebuilt library to exist
before it can read your code. With no sysroot named, a session runs as `no_core`: no external
crate is loaded, no prelude import is resolved, and nothing reaches for a library you did not
ask for. That is the default on purpose.

Do not clone rust-lang/rust, do not build a sysroot, do not go looking for one on disk, and do
not report a missing one as a blocker. The reasoning is in `src/frontend_facts/mod.rs`, at the
comment headed "The sysroot is an optional parameter, because this is a parser".
The sysroot section further down applies only to a caller who has deliberately
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

Add frontend as an ordinary dependency. **Nothing else is required**: no environment
variables, no `.cargo/config.toml`, no build wrapper. Compiler modules are available through
paths such as `frontend::rustc_interface` and `frontend::rustc_middle`.

```toml
[dependencies]
frontend = "^0.7.3"
```

Source parsing needs no sysroot, compiler version, or release channel. A caller may provide a
sysroot when it wants paths resolved through compiled library metadata.

The `.cargo/config.toml` here configures *this* workspace's own build. Cargo does not apply it to
dependents, and dependents do not need it.

## Candidate-bound expression type facts

`frontend_facts::effects::enclosing_function_body_span(source, candidate_range)`
selects the unique innermost function body containing the entire candidate byte
range. The range must be nonempty and lie on UTF-8 boundaries in the original
source. Selection is structural, not a function-name lookup, and parses Rust 2024.

Pass that full body span to `frontend_facts::analyze_body_type_facts`, or to
`analyze_body_type_facts_with_loaded` with an explicit edition and metadata,
cfg and environment already loaded by the caller. The loaded variant does not
search for dependencies. The default uses a synthetic no-core lang-item context
with the feature gates required by those declarations; it is not the original
crate's feature policy. The default type query also uses Rust 2024. For another
edition, supply an exact full body range directly to the loaded-context variant;
the structural selector currently has no edition parameter.

The result contains actual expression types and adjustments, type-check taint,
diagnostics, uncomputed symbolic constants and explicit coverage gaps. Reached
closure bodies and their binding, reference and tuple patterns participate in
the strict type gate. Supported `for`, `while` and source-written range
lowerings retain generated HIR types separately, with their source callsite.
The desugaring chain lists compiler syntax lowerings in hygiene order.
`type_extraction_complete` describes HIR type extraction; `projection_gaps`
records where one source range cannot select one expression uniquely. Ordinary
nested closures are visited with their own type-check results. Unsupported
nested owners, macro-expanded expressions, unsupported lowerings, unavailable
types and uncomputed symbolic constants remain extraction gaps. Closure type
identities can still be unknown even when expression types are covered.
Call facts name a type-check-selected item or report an unknown target; they do
not identify a runtime implementation.

`analyze_bodies_type_identity_with_loaded` accepts exact function-body ranges
in one source and returns identity reports in request order. Each report uses
an isolated single-body compiler session, including its diagnostics and const
observations; consume the reports directly without a single-body retry. The
second return value is the local stable crate ID from the first session that
entered a compiler context, or `None` if no session did. An empty request creates
no compiler context. This ID is contextual identity, not a source digest or a
proof that alternative source bodies preserve lexical resolution.
These are bounded observations, not ownership or trait-obligation proof or a compiler
pass/fail verdict. No supplied function body or external procedural macro is
executed. As with the other entry points, install
the panic catcher first and use `panic = "unwind"`.

## Performance: before and after

`check_source` on the repository's two generated corpora of valid source
(`examples/parallel_timing.rs`). The clean corpus is 200 files, 4.66 MB; the large one is 6
files, 0.98 MB. "Before" is the earliest measurement on record (commit `6774475`); "after" is
the former scoped stage executor, on the same 16-core machine, best of back-to-back runs.
These historical timings predate the owned-task boundary described below; current stage scaling
is not established by this table.

| | before | after |
| --- | ---: | ---: |
| clean, one file at a time, serial | 2,491 ms (1.9 MB/s) | 2,310 to 2,420 ms (2.0 MB/s) |
| clean, one file at a time, best width | 1,325 ms (3.5 MB/s) | 1,020 to 1,050 ms (4.5 MB/s) |
| clean, 200 files in parallel, 12 workers | not measured | 280 to 290 ms (16.5 MB/s) |
| same, with mimalloc | not measured | 207 to 225 ms (21 to 22 MB/s) |
| large, serial | 488 ms (2.0 MB/s) | 471 to 478 ms (2.1 MB/s) |
| large, best width | 203 ms (4.8 MB/s) | 143 to 148 ms (6.7 MB/s) |
| large, 6 files in parallel, each at width 2 | not measured | 78 ms (12.6 MB/s) |
| allocations, clean, serial | 13.9 million (4.72 GB) | 11.8 million (2.80 GB) |
| peak memory per file, clean, serial | 12.7 MB | 5.2 MB |
| parse alone | about 43 MB/s | about 43 MB/s |

Answers (every diagnostic and fact) are identical at every width and in every row. The largest
gains come from running files in parallel and from the allocator, both of which are the calling
program's choice: see the next section.

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

Borrowed compiler state remains on its caller. Expansion, late resolution, lowering, type
checking, borrow checking, facts and compiler lints run serially even at a wider session setting.
Their inputs and query results borrow the compiler arena and cannot be made owned by wrapping
owner IDs in Arc. Frozen glob visibility decisions are an owned-input/owned-output exception.

With the `parallel` feature, `rustc_parse::owned::parse_source_files` accepts an owned slice of
`ParseFileInput` values built from `Arc<SourceFile>`, their `Arc<SourceMap>`, edition and
token-stripping policy. It uses plain nagoya tasks and returns one `ParsedFile` in input order:
an owned AST, structured diagnostics, deferred lints and parser effects. Each file retains its
own symbol, span and hygiene identities; inspect or render its output through `ParsedFile::enter`.
Files are independent parse sessions, and their ASTs are not a combined compilation. Source
bytes and AST nodes are not copied for task transfer.

Owned function-body parsing also uses the pool. It keeps the existing transaction validation
and retries serially when a speculative diagnostic or disallowed identity insertion prevents
commit. File tasks and nested body tasks share the application pool budget; owners claim queued
work before waiting, so a nested parse does not wait for queued work behind blocked workers.

For complete semantic analysis, the host can schedule independent compiler sessions using typed
owned source inputs and collect owned reports. A session width above one permits its owned parser
work to use spare pool budget; it does not make the borrowed query context parallel. The old
width-4 recommendation and scoped-stage timings above do not describe this executor.

Workers need large stacks (the default nagoya runtime uses 16 MiB). Programs doing complete
semantic analysis still install `unwind_janky::install_catcher` first and use `panic = "unwind"`.
Frontend starts no OS threads of its own.

## Future work

Each of these is measured, not guessed; the sizes are shares of a serial check of the clean
corpus unless they say otherwise. `research/FINDINGS.md` has the profiles.

| Work | What it would save | Why it is not done |
| --- | --- | --- |
| Owned semantic-analysis sessions, reports in input order | Host-level parallelism for independent files | The parse-only batch API owns its data; a borrowed GlobalCtxt is still not a task input |
| Borrowck's MIR type check without a fulfillment context for operations that register no obligations | Part of MIR type check, which is 12% (279 ms of 2.3 s) | Needs a prototype to size |
| Borrowck without cloning each body (renumber regions into a side table) | About 3% (clone, renumber, copy) | Other passes read the unrenumbered MIR after borrowck |
| One shard lock per query run instead of two | About 2%, more at higher widths | Contained, not yet done |
| Free the AST once lowering finishes | About 13% of peak memory per file | The AST sits in the `index_ast` query result, which one fallback path still reads |
| Parse: fewer allocations per node (a `Box` per expression, a `ThinVec` per path), and no separate `Vec` and `Arc` per delimited group | Parse is about 4% of a check; it sits at about 43 MB/s | Changes AST types every later pass uses |
| A NEON or SSE table-driven lexer core (on stable, through `core::arch`) | Lexing is about a quarter of parse, so about 1% of a check | Small next to the rest |
| Serial front of a file: macro expansion and definition collection in one walk, then per item | Expansion, resolution and lowering are about 13% of a serial check, the serial start of every file | Definition order must stay exactly as it is |
| Per-body arenas for inference and obligation vectors | Part of the allocator's 11% | Stable Rust cannot give the standard collections another allocator |

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

## Optional sysroot

Parsing source and producing syntax-level diagnostics require no sysroot. A caller may provide
one when it wants paths resolved through compiled library metadata. The frontend does not query
the sysroot for a rustc version or require a matching version setting.

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
