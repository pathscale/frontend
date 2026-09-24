# Tools for working on frontend

Measurement and reproduction tools, used to find where the time goes. None of them is part of
the crate, and `cargo publish` leaves this directory out.

**The rules still apply.** POSIX `sh` and bun only: no Python, perl, awk or node. Nothing here
runs in the build.

## `bounded.sh`

`bounded.sh SECONDS CMD...` runs a command and kills it after SECONDS, since macOS has no
`timeout`. Every tool below is run under it. It runs the command in the background, where stdin
is `/dev/null`, so to feed stdin wrap the command in `sh -c "... < file"`.

## `profile/`: macOS `sample` profiles, folded with bun

Take a profile of a running `parallel_timing` with `sample PID SECONDS -mayDie -f out.txt`, then:

| Script | What it prints |
| --- | --- |
| `fold.ts FILE [filter]` | Inclusive and self samples per function |
| `kids.ts FILE NAME` | The direct callees of every frame whose name contains NAME |
| `parents.ts FILE NAME [levels]` | Caller chains of those frames |
| `subself.ts FILE... ROOT` | Self samples of the frames under ROOT, summed over files |
| `phase.ts FILE...` | The main thread's own time, charged to the innermost compiler pass on its stack. `PHASES="borrow check;type check bodies"` adds each pass's top functions |
| `share.ts FILE...` | The main thread's own time by kind: allocator, query machinery, hash tables, memory copies, interning, obligation processing |
| `agg.ts FILE` | Minimum and median of `LABEL ... check N ms` lines, per label, for A/B runs |
| `map.ts A B` | Per-pass seconds from two `-Z time-passes` JSON runs, side by side |

`name.ts` decodes v0-mangled symbols for the others.

## `parse-ref/`: frontend's parser against rust-analyzer's and syn's

A crate of its own (not in the workspace). It times the three parsers over the same sources:
`cargo run --release --manifest-path tools/parse-ref/Cargo.toml -- DIR [ROUNDS]` (3 rounds by default; `ONLY_FRONTEND=1` times frontend alone).

## `deps/`: reading a crate with its dependencies loaded, from source

- **`chain.sh`:** reads a dependency chain with `frontend-facts`, in dependency order, each
  crate once, `--items`, and writes each crate's metadata for the crates after it. It is the
  exact recipe (`--cfg`, `--env`, `--extern`, `--standard-library`, `--proc-macro`) for std's
  closure and for anyhow, serde_json and tokio with their closures.
  - `chain.sh std` reads std's closure; `chain.sh reg` reads the three registry crates (run
    `std` first). `chain.sh anyhow`, `serde_json` or `tokio` reads one.
  - Set `LIBRARY` (a rust-src `library/` of frontend's upstream era, with its `vendor/`),
    `REGISTRY` (a cargo registry source directory holding the named crate versions), `OUT` (the
    output directory) and optionally `FACTS` (the `frontend-facts` binary) and `ARCH`.
- **`check.sh ok e0599 e0061`:** checks the fixtures beside it against the tokio chain.
  - `ok.rs` checks clean.
  - `e0599.rs` calls a method no type has (E0599).
  - `e0061.rs` passes a wrong argument count (E0061).
