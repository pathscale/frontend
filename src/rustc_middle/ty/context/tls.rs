// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use eko::thread::ThreadLocal;
use crate::rustc_data_structures::sync;

use super::{GlobalCtxt, TyCtxt};
use crate::rustc_middle::dep_graph::TaskDepsRef;
use crate::rustc_middle::query::QueryJobId;

/// This is the implicit state of rustc. It contains the current
/// `TyCtxt` and query. It is updated when creating a local interner or
/// executing a new query. Whenever there's a `TyCtxt` value available
/// you should also have access to an `ImplicitCtxt` through the functions
/// in this module.
pub struct ImplicitCtxt<'a, 'tcx> {
    /// The current `TyCtxt`.
    pub tcx: TyCtxt<'tcx>,

    /// The current query job, if any.
    pub query: Option<QueryJobId>,

    /// Used to prevent queries from calling too deeply.
    pub query_depth: usize,

    /// The current dep graph task. This is used to add dependencies to queries
    /// when executing them.
    pub task_deps: TaskDepsRef<'a>,
}

impl<'a, 'tcx> ImplicitCtxt<'a, 'tcx> {
    pub fn new(gcx: &'tcx GlobalCtxt<'tcx>) -> Self {
        let tcx = TyCtxt { gcx };
        ImplicitCtxt { tcx, query: None, query_depth: 0, task_deps: TaskDepsRef::Ignore }
    }
}

// Import the thread-local variable from Rayon, which is preserved for Rayon jobs.
// **The pool's thread-local, now ours.** This was `rustc_thread_pool::tlv::TLV`, which is
// nothing more than `std::thread_local!(Cell<*const ()>)` - rayon exported it so that a *pool worker*
// could find the `TyCtxt`. With no pool there are no workers to find it from, and the cell is an
// ordinary thread-local that belongs beside the context it points at.
//
// `std::thread_local!` is gone with `std`; `eko::thread::ThreadLocal` is the same slot
// over `pthread_key_create`. Its `with` takes the initialiser as an argument and returns
// `Option<R>` - `None` only when the process is out of thread-local keys - so the two call sites
// below have to say what that means for them. The `Cell` is kept even though `with` hands out
// `&mut`, so that the store/restore below reads exactly as it did.
static TLV: ThreadLocal<core::cell::Cell<*const ()>> = ThreadLocal::new();

#[inline]
fn tlv_init() -> core::cell::Cell<*const ()> {
    core::cell::Cell::new(core::ptr::null())
}

#[inline]
fn erase(context: &ImplicitCtxt<'_, '_>) -> *const () {
    context as *const _ as *const ()
}

#[inline]
unsafe fn downcast<'a, 'tcx>(context: *const ()) -> &'a ImplicitCtxt<'a, 'tcx> {
    unsafe { &*(context as *const ImplicitCtxt<'a, 'tcx>) }
}

/// Sets `context` as the new current `ImplicitCtxt` for the duration of the function `f`.
#[inline]
pub fn enter_context<'a, 'tcx, F, R>(context: &ImplicitCtxt<'a, 'tcx>, f: F) -> R
where
    F: FnOnce() -> R,
{
    // A thread that cannot be given a key cannot be given a context either, and every query
    // below this point would then read `None` and panic somewhere less obvious. Fail here.
    TLV.with(tlv_init, |tlv| {
        let old = tlv.replace(erase(context));
        let _reset = crate::rustc_data_structures::defer(move || tlv.set(old));
        f()
    })
    .expect("out of thread-local keys: cannot store the ImplicitCtxt")
}

/// Allows access to the current `ImplicitCtxt` in a closure if one is available.
#[inline]
#[track_caller]
pub fn with_context_opt<F, R>(f: F) -> R
where
    F: for<'a, 'tcx> FnOnce(Option<&ImplicitCtxt<'a, 'tcx>>) -> R,
{
    // No key means no context was ever stored on this thread, which is what a null pointer
    // already says here, so the two collapse to the same `f(None)` below.
    let context = TLV.with(tlv_init, |tlv| tlv.get()).unwrap_or(core::ptr::null());
    if context.is_null() {
        f(None)
    } else {
        // We could get an `ImplicitCtxt` pointer from another thread.
        // Ensure that `ImplicitCtxt` is `DynSync`.
        sync::assert_dyn_sync::<ImplicitCtxt<'_, '_>>();

        unsafe { f(Some(downcast(context))) }
    }
}

/// Allows access to the current `ImplicitCtxt`.
/// Panics if there is no `ImplicitCtxt` available.
#[inline]
pub fn with_context<F, R>(f: F) -> R
where
    F: for<'a, 'tcx> FnOnce(&ImplicitCtxt<'a, 'tcx>) -> R,
{
    with_context_opt(|opt_context| f(opt_context.expect("no ImplicitCtxt stored in tls")))
}

/// Allows access to the `TyCtxt` in the current `ImplicitCtxt`.
/// Panics if there is no `ImplicitCtxt` available.
#[inline]
pub fn with<F, R>(f: F) -> R
where
    F: for<'tcx> FnOnce(TyCtxt<'tcx>) -> R,
{
    with_context(|context| f(context.tcx))
}

/// Allows access to the `TyCtxt` in the current `ImplicitCtxt`.
/// The closure is passed None if there is no `ImplicitCtxt` available.
#[inline]
#[track_caller]
pub fn with_opt<F, R>(f: F) -> R
where
    F: for<'tcx> FnOnce(Option<TyCtxt<'tcx>>) -> R,
{
    with_context_opt(
        #[track_caller]
        |opt_context| f(opt_context.map(|context| context.tcx)),
    )
}
