//! Parallel dispatch, with the pool arm removed.
//!
//! # Why every one of these is now the serial path
//!
//! These functions had two arms: a `rustc_thread_pool` (vendored rayon-core) arm when
//! `is_dyn_thread_safe()`, and a serial one otherwise. The pool arm is gone and the serial one is
//! all that is left.
//!
//! **The pool's own design is the reason.** rustc's parallel queries let a worker *block* on
//! another query - `mark_blocked_and_wait` releases the thread, arms a deadlock handler, runs the
//! wait, then re-acquires. A work-stealing pool whose workers block needs a deadlock detector to
//! be correct at all, and that detector is the shape of the problem rather than a solution to it.
//!
//! Keeping it working under a task runtime would have needed borrowed task scopes, cancellation
//! of borrowed work, and compiler-context installation on every worker - three capabilities
//! demanded only so that rustc-internal parallelism could survive. That is a large amount of
//! machinery for something that should be smaller and faster.
//!
//! So the frontend is serial *inside one request*, and parallelism lives where the work is
//! already owned: whole files and whole requests, which need no borrowed scope, no cancellation
//! of borrowed state and no context installation. Owned `Send + 'static` jobs are what
//! `nagoya` supports today, with no new capability.
//!
//! `mode::is_dyn_thread_safe` and the `DynSend`/`DynSync` bounds stay. They are what stopped
//! non-thread-safe data crossing into parallel work, they cost nothing here, and deleting them
//! would be a second change hiding inside this one.

//! This module defines parallel operations that are implemented in
//! one way for the serial compiler, and another way the parallel compiler.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::any::Any;

use parking_lot::Mutex;

use crate::rustc_data_structures::FatalErrorMarker;
use crate::rustc_data_structures::sync::{DynSend, DynSync, FromDyn, IntoDynSyncSend, mode};

/// A guard used to hold panics that occur during a parallel section to later by unwound.
/// This is used for the parallel compiler to prevent fatal errors from non-deterministically
/// hiding errors by ensuring that everything in the section has completed executing before
/// continuing with unwinding. It's also used for the non-parallel code to ensure error message
/// output match the parallel compiler for testing purposes.
pub struct ParallelGuard {
    panic: Mutex<Option<IntoDynSyncSend<Box<dyn Any + Send + 'static>>>>,
}

impl ParallelGuard {
    pub fn run<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        // Panic containment was dropped with `panic = "abort"`: `f` panicking now aborts the
        // process at its source, so there is nothing to catch, the guard is never armed, and
        // this never returns `None`. The `Err` arm is kept rather than deleted so the
        // hold-and-rethrow behaviour described on the type can be restored when a panic runtime
        // exists again.
        let caught: Result<R, Box<dyn Any + Send + 'static>> = Ok(f());
        caught
            .map_err(|err| {
                let mut panic = self.panic.lock();
                if panic.is_none() || !(*err).is::<FatalErrorMarker>() {
                    *panic = Some(IntoDynSyncSend(err));
                }
            })
            .ok()
    }
}

/// This gives access to a fresh parallel guard in the closure and will unwind any panics
/// caught in it after the closure returns.
#[inline]
pub fn parallel_guard<R>(f: impl FnOnce(&ParallelGuard) -> R) -> R {
    let guard = ParallelGuard { panic: Mutex::new(None) };
    let ret = f(&guard);
    if let Some(IntoDynSyncSend(_panic)) = guard.panic.into_inner() {
        // Was `resume_unwind(_panic)`. Panic containment was dropped with `panic = "abort"`:
        // nothing can be captured into the guard any more, so this arm is unreachable and the
        // process has already aborted at the panic site.
        panic!("parallel section panicked");
    }
    ret
}

fn serial_join<A, B, RA, RB>(oper_a: A, oper_b: B) -> (RA, RB)
where
    A: FnOnce() -> RA,
    B: FnOnce() -> RB,
{
    let (a, b) = parallel_guard(|guard| {
        let a = guard.run(oper_a);
        let b = guard.run(oper_b);
        (a, b)
    });
    (a.unwrap(), b.unwrap())
}

pub fn spawn(func: impl FnOnce() + DynSend + 'static) {
    func()
}

/// Runs the functions in parallel.
///
/// The first function is executed immediately on the current thread.
/// Use that for the longest running function for better scheduling.
// `+ DynSend` dropped from the trait object: no longer an auto trait (see `marker.rs`).
pub fn par_fns(funcs: &mut [&mut dyn FnMut()]) {
    parallel_guard(|guard: &ParallelGuard| {
        for f in funcs {
            guard.run(|| f());
        }
    });
}

#[inline]
pub fn par_join<A, B, RA: DynSend, RB: DynSend>(oper_a: A, oper_b: B) -> (RA, RB)
where
    A: FnOnce() -> RA + DynSend,
    B: FnOnce() -> RB + DynSend,
{
    serial_join(oper_a, oper_b)
}

fn par_slice<I: DynSend>(
    items: &mut [I],
    guard: &ParallelGuard,
    for_each: impl Fn(&mut I) + DynSync + DynSend,
    proof: FromDyn<()>,
) {
    match items {
        [] => return,
        [item] => {
            guard.run(|| for_each(item));
            return;
        }
        _ => (),
    }

    // One group, walked here. The pool arm split this into up to 128 chunks and spawned each;
    // with no pool there is nothing to spawn onto, and the chunking existed only to size the
    // steal granularity.
    for item in items {
        guard.run(|| for_each(item));
    }
}

pub fn par_for_each_in<I: DynSend, T: IntoIterator<Item = I>>(
    t: T,
    for_each: impl Fn(&I) + DynSync + DynSend,
) {
    parallel_guard(|guard| {
        if let Some(proof) = mode::check_dyn_thread_safe() {
            let mut items: Vec<_> = t.into_iter().collect();
            par_slice(&mut items, guard, |i| for_each(&*i), proof)
        } else {
            t.into_iter().for_each(|i| {
                guard.run(|| for_each(&i));
            });
        }
    });
}

// FIXME: actually make parallel and `T: DynSend`
pub fn par_for_each_slice<T>(items: &mut [T], for_each: impl Fn(&mut T)) {
    parallel_guard(|guard| {
        items.iter_mut().for_each(|i| {
            guard.run(|| for_each(i));
        });
    });
}

/// This runs `for_each` in parallel for each iterator item. If one or more of the
/// `for_each` calls returns `Err`, the function will also return `Err`. The error returned
/// will be non-deterministic, but this is expected to be used with `ErrorGuaranteed` which
/// are all equivalent.
pub fn try_par_for_each_in<T: IntoIterator, E: DynSend>(
    t: T,
    for_each: impl Fn(&<T as IntoIterator>::Item) -> Result<(), E> + DynSync + DynSend,
) -> Result<(), E>
where
    <T as IntoIterator>::Item: DynSend,
{
    parallel_guard(|guard| {
        if let Some(proof) = mode::check_dyn_thread_safe() {
            let mut items: Vec<_> = t.into_iter().collect();

            let error = Mutex::new(None);

            par_slice(
                &mut items,
                guard,
                |i| {
                    if let Err(err) = for_each(&*i) {
                        *error.lock() = Some(err);
                    }
                },
                proof,
            );

            if let Some(err) = error.into_inner() { Err(err) } else { Ok(()) }
        } else {
            t.into_iter().filter_map(|i| guard.run(|| for_each(&i))).fold(Ok(()), Result::and)
        }
    })
}

pub fn par_map<I: DynSend, T: IntoIterator<Item = I>, R: DynSend, C: FromIterator<R>>(
    t: T,
    map: impl Fn(I) -> R + DynSync + DynSend,
) -> C {
    parallel_guard(|guard| {
        if let Some(proof) = mode::check_dyn_thread_safe() {
            let map = proof.derive(map);

            let mut items: Vec<(Option<I>, Option<R>)> =
                t.into_iter().map(|i| (Some(i), None)).collect();

            par_slice(
                &mut items,
                guard,
                |i| {
                    i.1 = Some(map(i.0.take().unwrap()));
                },
                proof,
            );

            items.into_iter().filter_map(|i| i.1).collect()
        } else {
            t.into_iter().filter_map(|i| guard.run(|| map(i))).collect()
        }
    })
}

/// Run `op` once per worker. With no pool there is one worker, which is this thread.
pub fn broadcast<R: DynSend>(op: impl Fn(usize) -> R + DynSync) -> Vec<R> {
    vec![op(0)]
}
