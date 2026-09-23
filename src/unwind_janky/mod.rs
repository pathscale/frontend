//! `catch_unwind` and `resume_unwind`, without naming `std`.
//!
//! # Why this exists, and why it is called janky
//!
//! `std` is banned in this tree. But `FatalError` - rustc's "I refuse to compile this" - travels
//! by unwinding to a `catch_fatal_errors` at the top, and it is raised from twelve places
//! including every `emit_fatal`. With no way to catch it, a refused program aborts the process.
//! That is not an edge case: compiling a hello world hit it before the daemon could say what it
//! objected to.
//!
//! The honest fix is to make `FatalError` a return value threaded through the frontend. That is
//! hundreds of functions in code we do not own, and it is the *later* in "janky now, better
//! later". This is the now.
//!
//! # How it catches: the consumer lends it a catcher
//!
//! Catching a panic needs one of exactly two primitives, and neither is stable without `std`:
//!
//! - `std::panic::catch_unwind`, which is `std`;
//! - `core::intrinsics::catch_unwind`, which is `core_intrinsics` (nightly only), plus the
//!   `__rust_panic_cleanup` symbol, which can only be declared with
//!   `#[rustc_std_internal_symbol]` (`rustc_attrs`, nightly only) to get its mangled name.
//!
//! This module used the second pair until the crate had to build on stable. It now uses
//! neither. The frontend is a library, and whoever links it into a program links a panic
//! runtime too; that is the party that can catch. So the program calls [`install_catcher`]
//! once, at startup, with a function built on whatever it has. A program with `std`:
//!
//! ```ignore (needs std, which this crate cannot name)
//! fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
//!     std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
//! }
//! frontend::unwind_janky::install_catcher(catcher);
//! ```
//!
//! A `no_std` program with its own panic runtime installs one built on that runtime. Either way
//! the crate itself names no unstable item and no `std`.
//!
//! # What makes it janky, precisely
//!
//! - **It depends on the program installing a catcher.** Without one, [`catch`] runs the
//!   closure and lets a panic go past, as `panic = "abort"` would: a fatal error then ends the
//!   process instead of becoming an `Err`. [`unwinding_is_enabled`] reports false in that state,
//!   so the startup assert in `frontend_facts` fails loudly instead of silently.
//! - **The payload is whatever the catcher hands back**, `Box<dyn Any + Send>` from `std`, so
//!   `downcast_ref` works on it, which is what `catch_fatal_errors` needs.
//! - **It requires `panic = "unwind"`.** Under `panic = "abort"` there are no landing pads and no
//!   catcher can catch anything.

use alloc::boxed::Box;
use core::any::Any;
use core::sync::atomic::{AtomicPtr, Ordering};

/// What a caught panic carries.
///
/// The same type `std::panic::catch_unwind` yields, because it is the same payload: the panic
/// runtime built it, this only takes delivery. `downcast_ref` therefore works as it always did.
pub type Payload = Box<dyn Any + Send + 'static>;

/// A panic catcher: run the closure, and return the payload if it panicked.
///
/// Non-generic on purpose, so one function pointer serves every `catch::<F, R>`.
pub type Catcher = fn(&mut dyn FnMut()) -> Result<(), Payload>;

/// The installed [`Catcher`], or null. A function pointer stored as a data pointer, because
/// `core` has no atomic for function pointers.
static CATCHER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the function [`catch`] uses to catch panics. The last call wins.
///
/// Call it once, at startup, from the program that links the panic runtime. See the module
/// header for a `std` example.
pub fn install_catcher(catcher: Catcher) {
    CATCHER.store(catcher as *mut (), Ordering::Release);
}

fn installed_catcher() -> Option<Catcher> {
    let ptr = CATCHER.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // Safe: the only non-null value ever stored is a `Catcher` cast in `install_catcher`,
        // and a function pointer round-trips through a data pointer on every target we build.
        Some(unsafe { core::mem::transmute::<*mut (), Catcher>(ptr) })
    }
}

/// Run `f`, catching a panic that unwinds out of it.
///
/// With no catcher installed this runs `f` and lets any panic propagate; see the module header.
///
/// # What this does not do
///
/// It does not make `f` safe to have panicked. `std::panic::catch_unwind` requires
/// `UnwindSafe` and every caller in this tree wrapped its closure in `AssertUnwindSafe` to get
/// past it, so the bound was carrying no information; it is not reproduced. A caught panic can
/// still leave a data structure half-updated, and the caller is responsible for not reading one.
pub fn catch<F: FnOnce() -> R, R>(f: F) -> Result<R, Payload> {
    // The catcher takes a non-generic `&mut dyn FnMut()`, so the `FnOnce` and its result travel
    // through two `Option`s: the closure is taken on its one call and the result written back.
    let mut f = Some(f);
    let mut result = None;
    let mut call = || {
        let f = f.take().expect("a catcher called the closure twice");
        result = Some(f());
    };
    match installed_catcher() {
        Some(catcher) => catcher(&mut call)?,
        None => call(),
    }
    Ok(result.expect("a catcher returned Ok without calling the closure"))
}

/// Continue unwinding with a payload [`catch`] produced.
///
/// **The payload is not preserved.** `std::panic::resume_unwind` re-raises the original object;
/// this panics afresh, so a `downcast` further out sees a `&str` rather than whatever was thrown.
/// Every caller in this tree either discards the payload or has already inspected it - the one
/// that mattered, `catch_fatal_errors`, checks for `FatalErrorMarker` before deciding to resume -
/// so nothing reads it twice today. Fixing it means `__rust_start_panic`, which is the same
/// argument as writing a real unwinder.
pub fn resume(_payload: Payload) -> ! {
    // The re-raise must not overwrite what the original panic said. `record_panic` takes the last
    // writer, which is right for an ordinary sequence of panics and wrong for exactly this one:
    // the message here carries no information, and the one it would displace is the whole reason
    // the slot exists.
    RESUMING.store(true, core::sync::atomic::Ordering::Release);
    panic!("resuming a caught panic")
}

/// Set across [`resume`]'s own raise, so [`record_panic`] can decline it.
static RESUMING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether [`catch`] can actually catch: unwinding is compiled in and a catcher is installed.
///
/// `catch` cannot work under `panic = "abort"`: there are no landing pads, so a panic aborts
/// before anything sees it. Nor can it work before [`install_catcher`]. Either way it would
/// still *compile*, which is the dangerous part - containment that silently never runs. Call
/// this once at startup and say so out loud. (It was a `const fn` while it only read the cfg.)
pub fn unwinding_is_enabled() -> bool {
    cfg!(panic = "unwind") && installed_catcher().is_some()
}

// ---- what the last panic said ----------------------------------------------------------------

/// The message and location of the most recent panic, for whoever catches it.
///
/// # Why the payload is not enough
///
/// [`resume`] does not preserve the payload: it panics afresh, so a [`catch`] further out
/// downcasts *its* string rather than the original. Every compiler ICE meets
/// `catch_fatal_errors` first, which resumes anything that is not its own `FatalErrorMarker`, so
/// by the time the daemon catches one the message has already been replaced. Recording it where
/// it is still in hand costs one pointer and does not wait on fixing the raise path.
///
/// An atomic pointer rather than a lock because this crate has no dependencies, and the panic
/// path is not where a lock should first be taken. The last writer wins, which is the right
/// answer: a panic while panicking is the one still unwinding.
///
/// Written by the `#[panic_handler]` in `unwind-runtime`, which is the only thing that sees a
/// `PanicInfo`. This half lives here because any target may link this crate, and only a `no_std`
/// binary may link that one.
static LAST_PANIC: core::sync::atomic::AtomicPtr<alloc::string::String> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Record what a panic said. Called from the panic handler, before the raise.
pub fn record_panic(what: alloc::string::String) {
    use core::sync::atomic::Ordering;
    // `resume`'s own raise says only "resuming a caught panic", and it happens *after* the panic
    // worth reporting. Declining it is what makes the slot hold the original message rather than
    // the re-raise that replaced its payload.
    if RESUMING.swap(false, Ordering::AcqRel) {
        return;
    }
    let boxed = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(what));
    let previous = LAST_PANIC.swap(boxed, Ordering::AcqRel);
    if !previous.is_null() {
        // Safe: only this function stores into the slot, and only with `Box::into_raw`.
        drop(unsafe { alloc::boxed::Box::from_raw(previous) });
    }
}

/// What the last panic said, taking it.
///
/// `None` once it has been read, so a later request cannot report a panic that belonged to an
/// earlier one. A caller that catches and finds `None` was not the thing that panicked.
pub fn take_last_panic() -> Option<alloc::string::String> {
    use core::sync::atomic::Ordering;
    let taken = LAST_PANIC.swap(core::ptr::null_mut(), Ordering::AcqRel);
    if taken.is_null() {
        return None;
    }
    // Safe: as above.
    Some(*unsafe { alloc::boxed::Box::from_raw(taken) })
}
