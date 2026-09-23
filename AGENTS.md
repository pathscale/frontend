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
- frontend creates no threads and owns no pool. A compile runs on the caller's thread. When a
  session asks for parallelism (`jobs.frontend` of two or more, with the `parallel` cargo
  feature, which is off by default) its internal stages (`rustc_data_structures::sync::stages`
  and `run_stage`) also run items on nagoya's pool, whose threads are nagoya's or the embedding
  program's; otherwise every stage runs serially, in order, on the caller's thread. Never call
  `eko::thread::spawn`, `std::thread` or anything else that starts a thread from this crate, and
  never bring back the `par_*` shims: new parallel work is a stage over frozen input.
- No Python. No em dashes. No AI attribution. Never `git stash`. Stage your own paths only,
  with `git commit --only <paths>`.
