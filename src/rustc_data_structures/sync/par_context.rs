//! What a parallel item needs installed around it, supplied by the modules that own it.
//!
//! `rustc_data_structures` sits below the modules whose state a parallel item needs: it cannot
//! name `rustc_span`'s `SESSION_GLOBALS`, `rustc_middle`'s `ImplicitCtxt` or `rustc_errors`'
//! per-item diagnostic collection. So those are registered here as hooks, by
//! `rustc_interface::util::install_parallel_context`, which `run_compiler` calls, and the
//! parallel shims call the hooks without knowing what is behind them. That is the shape upstream's
//! `rustc_thread_pool` had for the one slot it carried (`tlv`), generalised.
//!
//! Two kinds:
//!
//! - [`ContextHook`]: a thread-local an item reads, captured where a stage scope opens and
//!   installed around every item, on whichever thread runs it.
//! - [`ItemHook`]: one per process, wrapped around every item of a stage and told when the stage
//!   starts and ends. `rustc_errors` uses it to make an item's diagnostics part of the item's
//!   output and forward them in item order (`rustc_errors::item_scope`).
//!
//! Both are used only by parallel sessions. A serial session runs a stage's items in place, in
//! order, and calls neither.

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

// ---- context hooks -------------------------------------------------------------------------

/// A thread-local that a parallel item must see on whichever thread runs it.
///
/// `capture` runs on the thread that opens a stage scope and returns the value as an erased
/// pointer, or null for "nothing installed here". `enter` runs around every item, on the thread
/// that runs it, and must install the captured value for exactly the duration of the call to
/// `run`, then put back whatever was there. The thread may already have the same value installed
/// (the scope's owner, or a thread running an item while it waits for it); `enter` must allow
/// that.
///
/// # Safety, for whoever registers one
///
/// `enter` receives a pointer `capture` produced on another thread. The stage scope guarantees
/// the capturing thread has not left the scope (so every frame below it is alive) for as long as
/// `enter` is on the stack, and guarantees nothing after `enter` returns. `enter` must not keep
/// the pointer or dereference it after `run` returns.
pub struct ContextHook {
    pub capture: fn() -> *const (),
    pub enter: unsafe fn(captured: *const (), run: &mut dyn FnMut()),
}

/// Room for the hooks the compiler registers (two today) with some to spare. A fixed table,
/// because it is read at every stage scope and a lock there would be one more thing contended.
const MAX_HOOKS: usize = 4;

static HOOKS: [AtomicPtr<ContextHook>; MAX_HOOKS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_HOOKS];

/// Register `hook`, once. Registering the same hook again does nothing, so every session can
/// call this on the way in without counting.
///
/// # Panics
///
/// If more than [`MAX_HOOKS`] distinct hooks are registered.
pub fn install_context_hook(hook: &'static ContextHook) {
    let wanted = ptr::from_ref(hook).cast_mut();
    for slot in &HOOKS {
        match slot.compare_exchange(ptr::null_mut(), wanted, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(existing) if existing == wanted => return,
            Err(_) => continue,
        }
    }
    panic!("more than {MAX_HOOKS} parallel context hooks registered");
}

/// Every registered hook's value, captured on the thread that opened a stage scope.
///
/// A fixed array, not a `Vec`: a scope is opened for every stage a call site runs (a
/// `run_stage` is a scope of its own), and the hooks are at most [`MAX_HOOKS`], so the capture
/// allocates nothing.
#[cfg_attr(not(feature = "parallel"), allow(dead_code))]
pub(crate) struct CapturedContext {
    /// The registered hooks and their values, in registration order; `None` past the last one.
    values: [Option<(&'static ContextHook, *const ())>; MAX_HOOKS],
}

// SAFETY: the pointers are only handed to the hooks' own `enter`, around an item, while the thread
// that captured them is still inside the scope that owns what they point at. See `ContextHook`.
unsafe impl Send for CapturedContext {}
unsafe impl Sync for CapturedContext {}

#[cfg_attr(not(feature = "parallel"), allow(dead_code))]
impl CapturedContext {
    pub(crate) fn capture() -> CapturedContext {
        let mut values = [None; MAX_HOOKS];
        for (slot, value) in HOOKS.iter().zip(values.iter_mut()) {
            let hook = slot.load(Ordering::Acquire);
            if hook.is_null() {
                break;
            }
            // SAFETY: only `&'static ContextHook`s are ever stored, by `install_context_hook`.
            let hook = unsafe { &*hook };
            *value = Some((hook, (hook.capture)()));
        }
        CapturedContext { values }
    }

    /// Run `run` with every captured value installed, first registered outermost.
    ///
    /// The stage calls this once per *run* of items, not once per item: a helper installs the
    /// scope's context when it arrives and runs item after item inside it (see `stage.rs`, "How
    /// items get run"). Every item of a scope wants exactly the same values installed, and an
    /// item puts back whatever it changed on its way out, so the values stay correct between
    /// items and the hooks' thread-local reads and writes are paid once per run.
    ///
    /// # Safety
    ///
    /// The thread that captured this must not have left the scope it was captured for, and must
    /// not leave it until this returns.
    pub(crate) unsafe fn enter(&self, run: &mut dyn FnMut()) {
        unsafe { enter_from(&self.values, run) }
    }
}

unsafe fn enter_from(values: &[Option<(&'static ContextHook, *const ())>], run: &mut dyn FnMut()) {
    match values.split_first() {
        None | Some((None, _)) => run(),
        Some((Some((hook, captured)), rest)) => {
            // SAFETY: forwarded from `CapturedContext::enter`.
            unsafe { (hook.enter)(*captured, &mut || enter_from(rest, run)) }
        }
    }
}

// ---- the item hook -------------------------------------------------------------------------

/// Wrapped around every item of a parallel stage.
///
/// The stage calls `begin` on the thread that starts it, before any item runs, with the stage's
/// item count (so the state can be sized once, per stage, instead of grown per item), and keeps
/// the state it returns; calls `run` around each item, on whichever thread runs it, in any
/// order; and after every item has settled calls exactly one of `finish` (the stage ended
/// normally, or it is the stage a fatal error stopped) or `discard` (an internal compiler error,
/// or a fatal error in an earlier stage, is unwinding out instead).
///
/// `run` is called once per item even when the stage runs a whole chunk of items inside one
/// install of the scope's context: an item's diagnostics are its own output, whatever ran next
/// to it.
///
/// `run` returns `true` when a serial run would not have gone on past this item: the item ended
/// in a fatal error, which the hook caught and keeps. The stage then skips every item after it in
/// serial order that has not started, as a serial run never reaches them, and `finish` raises the
/// fatal error again on the starting thread once every earlier item has been handled.
///
/// # Safety, for whoever registers one
///
/// `state` is what `begin` returned. It is used from several threads at once through `run`, so
/// what it points at must be `Sync`, and it is not used after `finish` or `discard`.
pub struct ItemHook {
    pub begin: fn(len: usize) -> *mut (),
    pub run: unsafe fn(state: *const (), index: usize, item: &mut dyn FnMut()) -> bool,
    pub finish: unsafe fn(state: *mut ()),
    pub discard: unsafe fn(state: *mut ()),
}

static ITEM_HOOK: AtomicPtr<ItemHook> = AtomicPtr::new(ptr::null_mut());

/// Register the item hook. The last call wins; registering the same one again changes nothing.
pub fn install_item_hook(hook: &'static ItemHook) {
    ITEM_HOOK.store(ptr::from_ref(hook).cast_mut(), Ordering::Release);
}

/// The item hook's state for one stage, begun on the thread that started the stage.
///
/// Dropped without [`finish`](ItemScope::finish), it calls the hook's `discard`: that is the
/// unwind path.
#[cfg_attr(not(feature = "parallel"), allow(dead_code))]
pub(crate) struct ItemScope {
    hook: Option<&'static ItemHook>,
    state: *mut (),
    /// Until `finish` or `discard` has consumed `state`.
    open: AtomicBool,
}

// SAFETY: the state is `Sync` by the hook's contract; `run` is the only use made of it from
// several threads, and `finish` and `discard` are each reached once, through `open`.
unsafe impl Send for ItemScope {}
unsafe impl Sync for ItemScope {}

#[cfg_attr(not(feature = "parallel"), allow(dead_code))]
impl ItemScope {
    pub(crate) fn begin(len: usize) -> ItemScope {
        let hook = ITEM_HOOK.load(Ordering::Acquire);
        if hook.is_null() {
            return ItemScope { hook: None, state: ptr::null_mut(), open: AtomicBool::new(false) };
        }
        // SAFETY: only `&'static ItemHook`s are ever stored, by `install_item_hook`.
        let hook = unsafe { &*hook };
        ItemScope { hook: Some(hook), state: (hook.begin)(len), open: AtomicBool::new(true) }
    }

    /// Run one item under the hook. `true` if a serial run would stop after it.
    ///
    /// Only before `finish`: the stage calls it for items that have not settled, and finishes
    /// only once every item has.
    pub(crate) fn run(&self, index: usize, item: &mut dyn FnMut()) -> bool {
        match self.hook {
            None => {
                item();
                false
            }
            // SAFETY: `state` came from this hook's `begin`, and the stage never runs an item
            // after finishing, so it has not been consumed.
            Some(hook) => unsafe { (hook.run)(self.state, index, item) },
        }
    }

    /// Every item has settled. May raise the fatal error an item ended in.
    pub(crate) fn finish(&self) {
        if let Some(hook) = self.hook
            && self.open.swap(false, Ordering::AcqRel)
        {
            // SAFETY: as in `run`; `open` was true and is now false, so this is the one use.
            unsafe { (hook.finish)(self.state) }
        }
    }
}

impl Drop for ItemScope {
    fn drop(&mut self) {
        if let Some(hook) = self.hook
            && *self.open.get_mut()
        {
            *self.open.get_mut() = false;
            // SAFETY: as in `finish`.
            unsafe { (hook.discard)(self.state) }
        }
    }
}
