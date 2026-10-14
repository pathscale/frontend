# Working agreement: frontend

For any coding agent working here or calling this crate. `CLAUDE.md` imports this file; never
fork these rules into a per-vendor file.

## Rule zero: you are just parsing, and you do not need a standard library

**If you catch yourself writing "no usable standard library exists", or planning to clone
rust-lang/rust, build a sysroot, or search the disk for one, you are off track. Stop.**

This crate is a parser. It reads the source it is handed. `std` is not special and is parsed no
differently from `fmt` or anything else. With no sysroot named, a session runs as `no_core`:
no external crate is loaded, no prelude import is resolved, and no library is needed anywhere
on the machine. That is the default, deliberately.

The reasoning lives in `src/frontend_facts/mod.rs`, at the comment headed "The sysroot is an
optional parameter, because this is a parser", and in commit `b87daf4` ("Stop requiring a
library to read facts out of source"). A sysroot matters only to a caller who has deliberately
asked for paths to be resolved into a compiled library. It is never a blocker to report.

## House rules

- `std` is banned in this crate. Every module is `#![no_std]`; the operating system arrives
  through `ekostd`. The one exception is `src/rustc_data_structures/sync/pool.rs`, compiled only
  with the `parallel` feature, which takes nagoya's pool and `std`'s `catch_unwind` and
  `resume_unwind` from there; nothing else names `std` or `nagoya`.
- frontend creates no threads and owns no pool. A compile runs on the caller's thread.
  Borrowed compiler walks (`sync::stages`, `run_stage`, `run_stage_weighted`) stay serial.
  With the `parallel` feature and a session width above one, owned file/body parsing and
  frozen resolver decisions use plain nagoya tasks. New pool work must enter as typed
  `Arc<In>` with actual `Send + Sync + 'static` bounds and return owned `Send + 'static`
  output. Never transport TyCtxt, arena borrows, compiler TLS or lifetime-erased closures;
  never use scoped parallel-for or thread creation APIs. Keep unsupported ownership
  boundaries serial and state why. Never bring back the `par_*` shims.
- No Python. No em dashes. No AI attribution. Never `git stash`. Stage your own paths only,
  with `git commit --only <paths>`.

## Calling this crate from a program: two choices that are yours

Read `README.md`, "Integrating it: allocator and parallelism", before you ship or measure.
In short:

- **Declare mimalloc as your binary's global allocator.** frontend declares none. It is 8% of
  a serial check and 24% with files in parallel.
- **Run independent files in parallel.** Use `rustc_parse::owned::parse_source_files` for
  owned parse output, or let the host schedule separate compiler sessions using typed owned
  inputs. Arena-dependent stages inside a compiler session remain serial; its owned body
  parsing can use spare pool budget. Existing width timings predate this ownership boundary.
