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
use crate::rustc_span::SessionGlobals;

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
    // No `#[track_caller]` on this closure: that is `closure_track_caller`, unstable.
    with_context_opt(
        |opt_context| f(opt_context.map(|context| context.tcx)),
    )
}

// ---- what a parallel stage's item runs in --------------------------------------------------

/// The thread-local context a parallel stage's items run in: the session's `SessionGlobals` and
/// the `ImplicitCtxt` of the code that opened the stage scope, as typed references.
///
/// **Captured where the scope opens, installed around the items, on whichever thread runs
/// them.** An item reads both through thread-locals (`with_session_globals`, [`with`]), and a
/// pool thread has neither, or has whatever it was doing before. The stage
/// (`rustc_data_structures::sync::stages`) captures this once, on the thread that opens the
/// scope, copies it into every stage the scope starts, and [`enter`](ItemContext::enter)s it
/// around each run of items.
///
/// **Typed, and borrowed for exactly as long as the scope.** [`capture`](ItemContext::capture)
/// hands the context to a closure rather than returning it, so `'c` is a lifetime inside the
/// frames that installed the two values, and the stage opens its whole scope inside that
/// closure: a `StageScope<'scope, _>` holds an `ItemContext<'scope>`, and the borrow checker
/// sees that the scope ends before the frames do. Nothing is erased here. The one place the
/// stage lets go of `'scope` is `sync::stage::parallel::detach`, which says why that is sound.
///
/// This lives here, in `rustc_middle`, because this is the lowest module that sees both halves:
/// `SessionGlobals` is `rustc_span`'s, `ImplicitCtxt` is this module's. It was a registry of
/// function pointers in `rustc_data_structures` over `*const ()`, filled at run time by
/// `rustc_interface`, which is the shape upstream needed because its crates could not name each
/// other; these are modules of one crate, and the stage names this type.
#[derive(Clone, Copy)]
pub struct ItemContext<'c> {
    /// `None` on a thread with no session: a caller that runs stages outside one.
    globals: Option<&'c SessionGlobals>,
    /// `None` outside every `ImplicitCtxt`: a stage run before the `TyCtxt` exists.
    icx: Option<&'c (dyn EnterImplicitCtxt + 'c)>,
}

/// An `ImplicitCtxt`, whatever its two lifetimes, as something that can be entered.
///
/// `ImplicitCtxt<'a, 'tcx>` is invariant in `'tcx` (it holds a `TyCtxt`), so a reference to one
/// cannot be shortened to the scope's lifetime; a reference to it as this trait can, since `'a`
/// and `'tcx` both outlive the reference. The one method is [`enter_context`] itself.
trait EnterImplicitCtxt {
    fn enter(&self, run: &mut dyn FnMut());
}

impl EnterImplicitCtxt for ImplicitCtxt<'_, '_> {
    fn enter(&self, run: &mut dyn FnMut()) {
        enter_context(self, run)
    }
}

/// `icx` as the trait object an [`ItemContext`] holds. A function, not a closure, so the
/// outlives bounds its argument implies (`'a: 'r`, `'tcx: 'r`) are what the coercion checks.
fn as_enterable<'r, 'a, 'tcx>(icx: &'r ImplicitCtxt<'a, 'tcx>) -> &'r (dyn EnterImplicitCtxt + 'r) {
    icx
}

impl ItemContext<'_> {
    /// Call `f` with this thread's context. Everything that uses the context has to happen
    /// inside `f`: that is what ties `'c` to the frames that installed it.
    pub fn capture<R>(f: impl for<'c> FnOnce(ItemContext<'c>) -> R) -> R {
        with_context_opt(|icx| {
            let icx = icx.map(as_enterable);
            if crate::rustc_span::session_globals_are_set() {
                crate::rustc_span::with_session_globals(|globals| {
                    f(ItemContext { globals: Some(globals), icx })
                })
            } else {
                f(ItemContext { globals: None, icx })
            }
        })
    }

    /// Run `run` with the captured values installed, the session's globals outermost, and put
    /// back whatever was there.
    ///
    /// Once per *run* of items, not once per item: a thread taking a stage's work installs the
    /// context when it starts and runs item after item inside it (`stage.rs`, "How items get
    /// run"). Every item of a scope wants exactly these values, and an item puts back what it
    /// changed on its way out, so they stay right between items. A single item gets its own
    /// install only when a thread waits for it and runs it there (`Slots::wait`).
    pub fn enter(&self, run: &mut dyn FnMut()) {
        match self.globals {
            Some(globals) => enter_session_globals(globals, &mut || self.enter_icx(run)),
            None => self.enter_icx(run),
        }
    }

    /// The `ImplicitCtxt` is installed even on a thread that already has one: the item's
    /// queries must see the context the stage was started in, whose `query` is the parent the
    /// query system's cycle check walks (`QueryWaitGraph`), not whatever context a waiting
    /// thread happens to be inside.
    fn enter_icx(&self, run: &mut dyn FnMut()) {
        match self.icx {
            Some(icx) => icx.enter(run),
            None => run(),
        }
    }
}

/// Install `globals` around `run`, unless this thread already has them.
///
/// The thread already has them when it is the session's own, or a pool thread running this item
/// while it waits inside another item of the same session. Anything else installed would mean
/// two sessions on one thread.
fn enter_session_globals(globals: &SessionGlobals, run: &mut dyn FnMut()) {
    if crate::rustc_span::session_globals_are_set() {
        let same =
            crate::rustc_span::with_session_globals(|current| core::ptr::eq(current, globals));
        assert!(same, "a parallel item ran on a thread that is inside another session");
        run()
    } else {
        crate::rustc_span::set_session_globals_then(globals, run)
    }
}
