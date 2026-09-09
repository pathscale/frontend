//! Memory-mapped files.
//!
//! This was a wrapper over `memmap2`, with a `Vec<u8>` fallback for miri and wasm32. Both are
//! gone: the mapping is now `libc-wrapper`'s, which is two `mmap` calls, and `memmap2` was the
//! last reason this crate pulled in a std dependency tree for a file view.
//!
//! The miri and wasm32 fallbacks went with it. Neither is a target this compiler builds for, and
//! a `Vec<u8>` standing in for a mapping is a branch nobody here can exercise.

pub use eko::mmap::{Mmap, MmapMut};
