# Parallel frontend: findings

Where the time goes, what runs in parallel now, and what is still serial. Every number comes
from `examples/parallel_timing.rs` on a shared 16-core machine (12 performance cores), so
compare numbers only within one run.

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
