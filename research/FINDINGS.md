# Parallel frontend: findings

Where the time goes, what runs in parallel now, and what is still serial. Every number comes
from `examples/parallel_timing.rs` on a shared 16-core machine (12 performance cores), so
compare numbers only within one run.

## State at the end of 2026-09-24

Clean corpus (200 files, 4.66 MB) and large corpus (6 files, 0.98 MB), `check_source`, best of
back-to-back runs at load 4 to 16. "Start" is the earliest measurement on record (`6774475`);
the master this work began from (`630da40`) was never timed on these corpora.

| | start | now |
| --- | ---: | ---: |
| clean, width 1 | 2,491 ms (1.87 MB/s) | 2,312 to 2,423 ms (about 2.0 MB/s) |
| clean, best width inside one file | about 1,325 ms (1.88x) | 1,020 to 1,049 ms at 12 (2.2 to 2.4x) |
| clean, 200 files in parallel on 12 workers | not measured | 280 to 290 ms (8.7x, 16.5 MB/s) |
| same, on mimalloc | | 207 to 225 ms (10.3x, 21 to 22 MB/s) |
| large, width 1 | 488 ms | 471 to 478 ms |
| large, best width inside one file | about 203 ms (2.4x) | 143 to 148 ms at 12 (3.3x) |
| large, 6 files in parallel, each at width 2, 12 workers | not measured | 78 ms (6.3x) |
| parse only | about 43 MB/s | 41.8 to 43.1 MB/s |
| allocations, clean check at width 1 | 13.9 M (4.72 GB) | 11.79 M (2.80 GB) |
| median peak live memory per file, width 1 / 12 | 12.7 / 16.6 MB | 5.2 / 8.3 MB |

**Inside one file the sweet spot is width 4** (1.7x clean, 2.1x large). Past 8 the wall clock
stops improving. About 45% of a file's check is serial on the session thread (parse,
expansion, resolution, session setup and teardown), and the parallel stages cost about 1.9x
their serial CPU at width 12 (allocator, pool search, query bookkeeping, shared-cache
pressure).

**Across files it scales almost linearly** (`parallel_timing files`): sessions share no state
the answers can see, and one file's serial start overlaps other files' work. For a caller
with many files this is the parallelism to use, with the inner width at 1, or 2 when there
are fewer files than workers.

**The allocator is the caller's choice and is worth 8 to 24%** (`--cfg bench_mimalloc`), the
most when files run in parallel.

Where the serial 2.3 s goes (width 1, the session thread's own time charged to the innermost
pass): borrow check 20%, body type check 16%, signatures and collection 12%, MIR build 9%,
well-formedness 7%, MIR passes 7%, setup and teardown 5%, resolution 5%, lints 4%, parse 4%,
lowering 3%. By kind of work: allocator 11%, query machinery 5%, hash tables 5%, memory copy
and zeroing 5%, interning 3%, obligation processing 3%, the rest compiler logic. No pass has a
hot spot: each is a flat profile.

Memory: most of a small session's peak was empty first buckets of query caches (4,096 slots
each) and interner tables (64 KiB each); both now start small (`vec_cache.rs`, `sharded.rs`).
`parallel_timing mem-held` says what a check holds at its peak.

## What is timed

Only valid source: every file must type check with no error, or the run stops. The two corpora
are generated from `tests/corpus/` (a lang item prelude plus unit templates):

- `clean`: 200 files of 300 to 1,500 lines, 4.7 MB.
- `large`: 6 files of 5,000 to 8,000 lines, 1 MB.

This crate's own `src/` was a corpus until `9893acb`. Each of its files compiled on its own,
without its crate or `core`, is not a valid program: 1,421 of 1,423 ended fatal, 341 on a
missing lang item and the rest at the `abort_if_errors` that ends every run with an error. Its
time was error recovery (typo and import suggestions for every unresolved `Option`, `Vec`,
`String`) and the unwind out of the refused run, so it measured neither the compiler nor its
parallelism. Several changes below were found on it and are kept because they are correct
and cost nothing on valid code.

## How the execution graph was read

- Per pass: rustc's `-Z time-passes` (JSON) switched on in the session options of a local
  build, summed by pass name over a corpus. A pass inside a stage item is CPU summed over the
  pool, not elapsed time.
- Per thread: macOS `sample` on a running `parallel_timing`, folded by function, with the
  session's own thread separated from the pool's. What the session thread does while no
  helper can is the serial part.

## Whole-call speedup (`check_source`, width 1 against the best width)

| corpus | width 1 | best | speedup |
| --- | ---: | ---: | ---: |
| large (6 files) | 465 ms | 160 ms | 2.90x |
| clean (200 files) | 2,357 ms | 1,088 ms | 2.17x |

`analyze_source` (facts) reaches 1.92x and 1.47x. Answers are identical at widths 1 to 24.

## Per pass, clean corpus, check plus analyze, width 1 against width 12

| pass | width 1 | width 12 | |
| --- | ---: | ---: | ---: |
| `MIR_borrow_checking` | 935 ms | 194 ms | 4.83x |
| `type_check_crate` | 824 ms | 278 ms | 2.96x |
| `coherence_checking` | 385 ms | 170 ms | 2.27x |
| `parse_crate` | 279 ms | 286 ms | serial |
| `resolve_crate` | 232 ms | 152 ms | 1.52x |
| of which `late_resolve_crate` | 195 ms | 113 ms | 1.73x |
| `misc_checking_3` | 149 ms | 113 ms | 1.33x |
| `lint_checking` | 85 ms | 36 ms | 2.34x |
| `macro_expand_crate` | 84 ms | 84 ms | serial |

## What changed in this round, and why

- **The stage gate measures instead of estimating.** Weights are relative sizes. `run_stage`
  runs its first items serially and times them, and hands the rest to the pool once their
  measured cost pays for helpers; a scope's owner measures the chunks it drains and wakes
  helpers from that rate. The earlier per-byte constants came from `src/`, where most bodies
  fail to resolve, and kept every stage of clean code serial (0.93x).
- **No HIR hashing.** `needs_hir_hash` was true because sessions are `Rlib` and
  `needs_metadata()` said so, but this crate encodes no metadata. Lowering hashed every owner,
  reading def path hashes under the definitions lock while other items created defs. Large
  went from 2.49x to 3.09x.
- **`drop_ast` off the pool.** The AST was freed inside lowering items on every worker at
  once, into the allocating thread's zone; it is now freed once, at session end, on the thread
  that allocated it.
- **Lock-free reads**: symbol strings (an append-only table published under the interner's
  lock), root-context hygiene, and the definitions during the frozen resolver stages
  (`FreezeLock::read_phase`).
- **Macro finalization is a stage** over the frozen resolver, like late resolution.
- **Zero copy of the source**: `Input::Str` carries `Arc<String>` to the `SourceFile`, which
  copies only to normalise a BOM or CRLF.
- **The parallel parse is removed**: on valid source it gained 1.04x on `parse_crate` (clean)
  and 1.33x (large), under 1% of either run. `research/parallel-parse.md`.

## Still serial, as shares of the session thread at width 12 (clean corpus)

1. **Lexing and parsing**, about 16%. Lexing builds every token tree before parsing starts.
2. **Macro expansion and early resolution**, about 14%. The fixed-point loop mutates the
   resolver and numbers hygiene contexts and ids in walk order
   (`research/macro-expansion.md`).
3. **Teardown**, about 12%: dropping the query system, mostly the per-slot query arenas the
   workers filled (one `QueryArenas` per registry slot), freed on the session thread.
4. Session setup and early lints, a few percent each.

With about 42% serial, 12 workers cannot give more than about 2.2x on the clean corpus.
