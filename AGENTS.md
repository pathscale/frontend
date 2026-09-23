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
  through `ekostd`.
- No threads and no thread pool: a compile runs on the caller's thread.
- No Python. No em dashes. No AI attribution. Never `git stash`. Stage your own paths only,
  with `git commit --only <paths>`.
