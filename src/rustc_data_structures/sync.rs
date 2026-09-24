//! This module defines various operations and types that are implemented in
//! one way for the serial compiler, and another way the parallel compiler.
//!
//! Operations
//! ----------
//! There is one: the stage (`stage.rs`, [`stages`] and [`run_stage`]), a function over frozen
//! input fanned out per index into fill-once, per-item-awaitable output slots. In a parallel
//! session its items run on nagoya's pool; in a serial one it degenerates straightforwardly to
//! the loop, in order, on the calling thread.
//!
//! Whether a session is parallel is decided per session, not per process: see `mode` below.
//!
//! Types
//! -----
//! The parallel versions of types provide various kinds of synchronization,
//! while the serial compiler versions do not.
//!
//! The following table shows how the types are implemented internally. Except
//! where noted otherwise, the type in column one is defined as a
//! newtype around the type from column two or three.
//!
//! | Type                    | Serial version           | Parallel version                |
//! | ----------------------- | ------------------------ | ------------------------------- |
//! | `Lock<T>`               | `RefCell<T>`             | `RefCell<T>` or                 |
//! |                         |                          | `parking_lot::Mutex<T>`         |
//! | `RwLock<T>`             | `parking_lot::RwLock<T>` | `parking_lot::RwLock<T>`        |

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use hashbrown::HashMap;
use core::hash::{BuildHasher, Hash};

pub use parking_lot::{
    MappedRwLockReadGuard as MappedReadGuard, MappedRwLockWriteGuard as MappedWriteGuard,
    RwLockReadGuard as ReadGuard, RwLockWriteGuard as WriteGuard,
};

pub use self::atomic::AtomicU64;
pub use self::freeze::{FreezeLock, FreezeReadGuard, FreezeWriteGuard};
#[doc(no_inline)]
pub use self::lock::{Lock, LockGuard, Mode};
pub use self::mode::{
    FromDyn, SessionMode, check_dyn_thread_safe, enter_session_width, is_dyn_thread_safe,
    is_parallel_here,
    set_dyn_thread_safe_mode,
};
#[cfg(feature = "parallel")]
pub use self::pool::set_parallel_executor;
pub use self::stage::{ReadySlot, Slots, StageScope, cost, run_stage, run_stage_weighted, stages};
pub use self::vec::{AppendOnlyIndexVec, AppendOnlyVec};
pub use self::worker_local::{Registry, RegistryScope, RegistrySlot, WorkerLocal};
pub use crate::rustc_data_structures::marker::*;

// The `par_*` shims (`par_for_each_in`, `try_par_for_each_in`, `par_map`, `par_join`,
// `par_fns`, `par_for_each_slice`, `broadcast`, `spawn`) and the `ParallelGuard` they needed are
// gone. Every call site uses `stage` directly now, or is a plain serial loop where the work is
// not on this crate's analysis path.
mod freeze;
mod lock;
// The only module that names `std` or `nagoya`, and only with the `parallel` feature.
#[cfg(feature = "parallel")]
mod pool;
// The one module here that names the compiler above it: a stage's items run in the compiler's
// context (`rustc_middle::ty::tls::ItemContext`) and hand it their diagnostics
// (`rustc_errors::OrderedReplay`). Its header says why that is a direct call and not a hook.
mod stage;
mod vec;
mod worker_local;

/// Keep the conditional imports together in a submodule, so that import-sorting
/// doesn't split them up.
mod atomic {
    // Most hosts can just use a regular AtomicU64.
    #[cfg(target_has_atomic = "64")]
    pub use core::sync::atomic::AtomicU64;

    // Some 32-bit hosts don't have AtomicU64, so use a fallback.
    #[cfg(not(target_has_atomic = "64"))]
    pub use portable_atomic::AtomicU64;
}

/// Whether the code running on this thread is in a thread-safe (parallel) session.
///
/// # Per session, not per process
///
/// This was one process-wide value, set once by the first session and asserted by every later
/// one, so a process that ran a serial session and then asked for a parallel one panicked, and
/// which lock type a session got depended on which session came first. It is now two things:
///
/// - **A per-session latch**, a thread-local width set by [`enter_session_width`] for the
///   duration of a session (`run_compiler` does it first thing, before any `Lock`, `Sharded` or
///   `SessionGlobals` exists) and installed on every pool thread for as long as it runs that
///   session's items. `0` means no session on this thread, `1` a serial session, `2` or more a
///   parallel session of that many threads. Everything a session builds is built under its own
///   latch, so its `Lock`s pick the synchronised or the unsynchronised kind to match how the
///   session runs, whatever any other session in the process does. A serial session keeps its
///   `Cell`-backed locks and pays nothing for a parallel one elsewhere.
/// - **A process-wide fallback** for threads with no latch (a caller that parses outside any
///   session, a `'static` detached job). It is sticky: once any caller asks for thread safety it
///   stays on, and a later request for serial does not turn it off. Code on an unlatched thread
///   therefore builds real locks once anything parallel has happened, which costs a little and
///   is never unsound. Before anything has asked, it is uninitialised and
///   [`is_dyn_thread_safe`] panics as it always did.
///
/// # Why mixing is safe
///
/// A `Lock` records at construction which kind it is (`lock.rs`), and a `Sharded` records it in
/// its variant, so each object is internally consistent forever. The hazard would be an object
/// built unsynchronised being shared into a parallel session's items. Nothing in the compiler is
/// shared across sessions except process-wide statics, none of which holds a `Lock` or a
/// `Sharded` (checked: only atomics, `OnceLock`/`LazyLock` tables and `eko` mutexes). A caller
/// that builds compiler state outside a session and hands it to a parallel one would have to set
/// the fallback first; nothing in this crate does that.
mod mode {
    use core::sync::atomic::{AtomicU8, Ordering};

    use eko::thread::ThreadLocal;

    const UNINITIALIZED: u8 = 0;
    const DYN_NOT_THREAD_SAFE: u8 = 1;
    const DYN_THREAD_SAFE: u8 = 2;

    /// The process-wide fallback, for threads with no session latched. Sticky once thread-safe.
    static DYN_THREAD_SAFE_MODE: AtomicU8 = AtomicU8::new(UNINITIALIZED);

    /// The latched session width on this thread: `0` none, `1` serial, `n >= 2` parallel.
    ///
    /// State, not a cache: it is the session's own setting, written on entry and put back on
    /// exit, and read where a decision depends on it. An `eko::thread::ThreadLocal`; a thread
    /// out of thread-local keys reads as unlatched and falls back to the process value.
    static SESSION_WIDTH: ThreadLocal<usize> = ThreadLocal::new();

    #[inline]
    fn latched() -> usize {
        SESSION_WIDTH.with(|| 0, |width| *width).unwrap_or(0)
    }

    // Whether thread safety is enabled (due to running under multiple threads).
    #[inline]
    pub fn check_dyn_thread_safe() -> Option<FromDyn<()>> {
        is_dyn_thread_safe().then_some(FromDyn(()))
    }

    // Whether thread safety is enabled (due to running under multiple threads).
    #[inline]
    pub fn is_dyn_thread_safe() -> bool {
        match latched() {
            0 => match DYN_THREAD_SAFE_MODE.load(Ordering::Relaxed) {
                DYN_NOT_THREAD_SAFE => false,
                DYN_THREAD_SAFE => true,
                _ => panic!("uninitialized dyn_thread_safe mode!"),
            },
            1 => false,
            _ => true,
        }
    }

    // Whether thread safety might be enabled.
    #[inline]
    pub(super) fn might_be_dyn_thread_safe() -> bool {
        match latched() {
            0 => DYN_THREAD_SAFE_MODE.load(Ordering::Relaxed) != DYN_NOT_THREAD_SAFE,
            1 => false,
            _ => true,
        }
    }

    /// Whether this thread should hand work to the pool: [`is_dyn_thread_safe`], except that it
    /// answers `false` instead of panicking when nothing has chosen a mode yet.
    #[cfg_attr(not(feature = "parallel"), allow(dead_code))]
    #[inline]
    pub fn is_parallel_here() -> bool {
        match latched() {
            0 => DYN_THREAD_SAFE_MODE.load(Ordering::Relaxed) == DYN_THREAD_SAFE,
            1 => false,
            _ => true,
        }
    }

    /// The latched session width, or `0` on a thread with no session.
    #[cfg_attr(not(feature = "parallel"), allow(dead_code))]
    #[inline]
    pub(super) fn session_width() -> usize {
        latched()
    }

    /// Set the process-wide fallback. `true` turns it on for good; `false` only initialises it
    /// if nothing has yet, and never turns it off. Never panics.
    pub fn set_dyn_thread_safe_mode(mode: bool) {
        if mode {
            DYN_THREAD_SAFE_MODE.store(DYN_THREAD_SAFE, Ordering::Relaxed);
        } else {
            let _ = DYN_THREAD_SAFE_MODE.compare_exchange(
                UNINITIALIZED,
                DYN_NOT_THREAD_SAFE,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    /// Run this thread as part of a session of `width` threads until the guard drops: `1` for a
    /// serial session, `2` or more for a parallel one. Also initialises (or, for a parallel
    /// session, turns on) the process-wide fallback, so code the session's caller runs outside
    /// it afterwards finds a mode chosen, as it did when this was one process-wide setting.
    ///
    /// # Panics
    ///
    /// If `width` is `0`.
    #[must_use = "the session's mode ends when this is dropped"]
    pub fn enter_session_width(width: usize) -> SessionMode {
        assert!(width > 0, "a session runs on at least one thread");
        set_dyn_thread_safe_mode(width > 1);
        let previous = SESSION_WIDTH
            .with(|| 0, |slot| core::mem::replace(slot, width))
            .expect("out of thread-local keys: cannot latch the session's mode");
        SessionMode { previous }
    }

    /// Puts back the width [`enter_session_width`] replaced.
    pub struct SessionMode {
        previous: usize,
    }

    impl Drop for SessionMode {
        fn drop(&mut self) {
            let previous = self.previous;
            let _ = SESSION_WIDTH.with(|| 0, |slot| *slot = previous);
        }
    }

    #[derive(Copy, Clone)]
    pub struct FromDyn<T>(T);

    impl<T> FromDyn<T> {
        #[inline(always)]
        pub fn derive<O>(&self, val: O) -> FromDyn<O> {
            // We already did the check for `sync::is_dyn_thread_safe()` when creating `Self`
            FromDyn(val)
        }

        #[inline(always)]
        pub fn into_inner(self) -> T {
            self.0
        }
    }

    // Upstream: `unsafe impl<T: DynSend> Send for FromDyn<T>` and the `DynSync`/`Sync`
    // counterpart. `DynSend`/`DynSync` are now implemented for every type (see
    // `marker.rs`), so those impls would make every `FromDyn<T>` `Send + Sync`, which is
    // unsound for a public type. Without them `FromDyn<T>` gets the ordinary auto impls,
    // `Send` iff `T: Send`; nothing in this single-threaded crate relied on more.

    impl<T> core::ops::Deref for FromDyn<T> {
        type Target = T;

        #[inline(always)]
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T> core::ops::DerefMut for FromDyn<T> {
        #[inline(always)]
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
}

/// This makes locks panic if they are already held.
/// It is only useful when you are running in a single thread
const ERROR_CHECKING: bool = false;

#[derive(Default)]
#[repr(align(64))]
pub struct CacheAligned<T>(pub T);

pub trait HashMapExt<K, V> {
    /// Same as HashMap::insert, but it may panic if there's already an
    /// entry for `key` with a value not equal to `value`
    fn insert_same(&mut self, key: K, value: V);
}

impl<K: Eq + Hash, V: Eq, S: BuildHasher> HashMapExt<K, V> for HashMap<K, V, S> {
    fn insert_same(&mut self, key: K, value: V) {
        self.entry(key).and_modify(|old| assert!(*old == value)).or_insert(value);
    }
}

#[derive(Debug, Default)]
pub struct RwLock<T>(parking_lot::RwLock<T>);

impl<T> RwLock<T> {
    #[inline(always)]
    pub fn new(inner: T) -> Self {
        RwLock(parking_lot::RwLock::new(inner))
    }

    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.0.into_inner()
    }

    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }

    #[inline(always)]
    pub fn read(&self) -> ReadGuard<'_, T> {
        if ERROR_CHECKING {
            self.0.try_read().expect("lock was already held")
        } else {
            self.0.read()
        }
    }

    #[inline(always)]
    pub fn try_write(&self) -> Result<WriteGuard<'_, T>, ()> {
        self.0.try_write().ok_or(())
    }

    #[inline(always)]
    pub fn write(&self) -> WriteGuard<'_, T> {
        if ERROR_CHECKING {
            self.0.try_write().expect("lock was already held")
        } else {
            self.0.write()
        }
    }

    #[inline(always)]
    #[track_caller]
    pub fn borrow(&self) -> ReadGuard<'_, T> {
        self.read()
    }

    #[inline(always)]
    #[track_caller]
    pub fn borrow_mut(&self) -> WriteGuard<'_, T> {
        self.write()
    }
}
