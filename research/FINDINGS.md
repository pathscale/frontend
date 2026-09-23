# Parallel frontend: findings

Where the time goes, what runs in parallel now, and what is still serial. Every number comes
from `examples/parallel_timing.rs` on a shared 16-core machine, so compare numbers only within
one run.

## How to read the execution graph

`FRONTEND_TIME_PASSES=1` turns on rustc's own `-Z time-passes` inside `check_source` and
`analyze_source`, as one JSON line per pass on stderr. `PROFILE=<src|clean|large>:<workers>:<passes>`
runs one setting of the timing example on its own. The two together give the pass tree of a
whole corpus:

    FRONTEND_TIME_PASSES=1 PROFILE=large:1:1 cargo run --release --features diagnostics,parallel --example parallel_timing 2> passes.txt

A pass that runs inside a stage item (such as `drop_ast`, which runs inside lowering) is summed
across workers. That sum is thread time, not wall time.

## Bottlenecks at 1 worker (check plus analyze, summed over every file)

| pass | this crate's `src/` (1,424 files) | large clean files (6) |
| --- | ---: | ---: |
| `resolve_crate` | 6,003 ms | 36 ms |
| of which `late_resolve_crate` | 3,523 ms | 32 ms |
| of which `finalize_macro_resolutions` | 1,951 ms | 0 ms |
| `parse_crate` | 1,121 ms | 54 ms |
| `macro_expand_crate` | 611 ms | 13 ms |
| `MIR_borrow_checking` | 0 ms | 203 ms |
| `type_check_crate` | 79 ms | 174 ms |
| `coherence_checking` | 76 ms | 66 ms |

The crate's own sources run as `no_core`, so most files stop at a missing lang item before
analysis. Their time is almost all name resolution, much of it error-suggestion search for
names that are unresolved without `core`.

## What runs in parallel now, and its effect (large files, 1 against 8 workers)

| pass | 1 worker | 8 workers |
| --- | ---: | ---: |
| `MIR_borrow_checking` | 203 ms | 46 ms |
| `type_check_crate` | 174 ms | 61 ms |
| `coherence_checking` | 66 ms | 34 ms |
| `parse_crate` (serial) | 54 ms | 56 ms |
| `resolve_crate` (serial) | 39 ms | 36 ms |

The stages are type checking, borrow checking, well-formedness checks, coherence, lints, AST to
HIR lowering, and `frontend_facts::extract`. Whole-call speedup on `check_source`:

- large clean files: 2.24x at 12 workers
- clean corpus: 1.53x at 4 workers
- this crate's sources: none, because they are bound by resolution

Answers are identical at 1 to 12 workers on every corpus.

## Still serial

1. **Name resolution.** Late resolution and macro finalization both resolve paths through
   `&mut Resolver` (`cm_mut`), which writes used-import marks, borrow counts, per-module trait
   lists and diagnostics state. Late resolution is already split into per-item units with owned
   output (`research/late-resolution.md`). What remains is a per-unit write sink plus a frozen
   read mode, so the units can run as a stage. The unfinished attempt is
   `research/late-resolution-wip.patch`. Macro finalization needs the same sink.
2. **Parsing** one file. Not split.
3. **Macro expansion.** Its fixed-point loop mutates the resolver.
