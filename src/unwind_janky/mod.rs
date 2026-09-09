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
//! # Why it is smaller than an unwinder
//!
//! It is not an unwinder. `core::intrinsics::catch_unwind` is the primitive
//! `std::panic::catch_unwind` is built on - `library/std/src/panicking.rs` calls exactly this. So
//! this crate is that intrinsic with the three-callback dance written out, and `panic!` for the
//! other direction. The runtime it reaches is somebody else's: `__rust_panic_cleanup` is declared
//! below and left undefined in this rlib, to be resolved at link time by whoever linked a panic
//! runtime.
//!
//! # It defines no lang item, and that is the whole point of the split
//!
//! Until 2026-09-08 this crate also carried the `#[panic_handler]`, the `eh_personality` lang
//! item and the `extern crate panic_unwind`/`extern crate unwind` lines that pull the runtime in.
//! That made the consumer test suite unbuildable, because a lang item may be defined
//! once per linked binary and a test harness links `test`, which links `std`, which defines both:
//!
//! ```text
//! error[E0152]: duplicate lang item in crate `std` (which `test` depends on): `panic_impl`
//!   = note: the lang item is first defined in crate `unwind_janky`
//! ```
//!
//! Three crates depend on this one - the consumer, `compiler/rustc_span`,
//! `frontend/compiler/rustc_proc_macro` - and all three want `catch`. None of them wanted a lang
//! item, and a library cannot supply one to a dependent that may also link `std`. So the runtime
//! half moved to a separate runtime crate, which only the final binary names. Read that crate's header for
//! the mechanism; this one is now just the two functions.
//!
//! # This is a bridge, and the bridge is being demolished
//!
//! **`std` must not be linked at all.** The ruling is absolute, and this compiler no longer pulls
//! `libstd-*.rlib`, which a link-line check confirms by reading the line itself.
//! `polonius-engine`, `odht` and the rest are still to go, and `jobserver` went on 2026-09-08,
//! with `fluent-bundle` taking the whole i18n cluster - 18 crates - the same day.
//!
//! So: use this to get `FatalError` working again now. Do not build anything else on it.
//!
//! ## Why our own `__rust_panic_cleanup` is not a smaller step
//!
//! It is the whole unwinder or none of it. Defining that symbol alone collides with the one
//! `panic_unwind` already exports. Raising our own `_Unwind_Exception` with a private exception
//! class to avoid needing it does not work either: `core::intrinsics::catch_unwind` compiles to
//! a landing pad that consults `rust_eh_personality`, which `panic_unwind` owns, and Rust's
//! personality deliberately declines foreign exception classes and lets them propagate - so the
//! exception would fly past our own catch.
//!
//! **Correction, 2026-09-08, measured rather than reasoned.** The paragraph above said four
//! symbols become ours. It is two. The other two already exist in a crate that is not `std`.
//!
//! `panic_unwind` is a separate sysroot crate depending on `core`, `alloc`, `libc` and `unwind`,
//! and it exports both `__rust_start_panic` and `__rust_panic_cleanup`
//! (`library/panic_unwind/src/lib.rs:90` and `:83`). `_Unwind_RaiseException` and
//! `_Unwind_Resume` come from the `unwind` crate over libSystem. None of that needs `std`.
//!
//! What `std` alone owns is the `eh_personality` **lang item**, in
//! `library/std/src/sys/personality/`. Its absence is the *only* reason rustc rejects a
//! `no_std` binary built with `panic = "unwind"`: `rustc_passes/src/weak_lang_items.rs:55`
//! emits "unwinding panics are not supported without std" when that weak lang item is missing,
//! and the message names `std` because `std` is where it normally comes from, not because `std`
//! is required.
//!
//! So the list is, and the first two live in a separate runtime crate rather than here:
//!
//!   `#[panic_handler]`             ours. Builds the payload and calls `__rust_start_panic`.
//!   `#[lang = "eh_personality"]`   ours. Walks the LSDA and decides catch vs propagate.
//!   `__rust_start_panic`           `panic_unwind`'s. Free.
//!   `__rust_panic_cleanup`         `panic_unwind`'s. Free.
//!
//! **Verified, not assumed.** A `no_std` `#![no_main]` binary with `panic = "unwind"`, an
//! `extern crate panic_unwind`, a `#[panic_handler]` and an *aborting stub* for the lang item
//! compiles and links, and its sysroot rlibs are exactly `liballoc`, `libcompiler_builtins`,
//! `libcore`, `liblibc`, `libpanic_unwind`, `librustc_std_workspace_core` and `libunwind`.
//! No `libstd`. Without the stub, and with everything else identical, it fails with that one
//! error. So the personality routine is the whole remaining job.
//!
//! That port is done. It lives in a separate runtime crate, as a copy of
//! `library/std/src/sys/personality/{mod.rs, gcc.rs, dwarf/}` with one line changed.
//!
//! Owning the exception object also fixes `resume` below, which today cannot preserve a payload.
//!
//! # What makes it janky, precisely
//!
//! - **It depends on something else linking a panic runtime.** `__rust_panic_cleanup` is
//!   declared here and defined nowhere in this rlib. In the final binary that is a separate runtime crate
//!   naming `panic_unwind`; in a test binary it is `std`. Link the crate into something that has
//!   neither and it fails at link time, loudly, naming the symbol - which is the good failure.
//! - **The payload is `Box<dyn Any + Send>` built by the panic runtime**, so `downcast_ref` works
//!   on it, which is what `catch_fatal_errors` needs to tell a `FatalErrorMarker` from an ICE.
//! - **It requires `panic = "unwind"`.** Under `panic = "abort"` the compiler emits no landing
//!   pads and the intrinsic can never catch anything - it would compile and silently never work.
//!   There is a check for that below.

#![allow(internal_features)]


use alloc::boxed::Box;
use core::any::Any;

/// What a caught panic carries.
///
/// The same type `std::panic::catch_unwind` yields, because it is the same payload: std's panic
/// machinery built it, this only takes delivery. `downcast_ref` therefore works as it always did.
pub type Payload = Box<dyn Any + Send + 'static>;

/// Run `f`, catching a panic that unwinds out of it.
///
/// # What this does not do
///
/// It does not make `f` safe to have panicked. `std::panic::catch_unwind` requires
/// `UnwindSafe` and every caller in this tree wrapped its closure in `AssertUnwindSafe` to get
/// past it, so the bound was carrying no information; it is not reproduced. A caught panic can
/// still leave a data structure half-updated, and the caller is responsible for not reading one.
pub fn catch<F: FnOnce() -> R, R>(f: F) -> Result<R, Payload> {
    union Data<F, R> {
        f: core::mem::ManuallyDrop<F>,
        r: core::mem::ManuallyDrop<R>,
        p: core::mem::ManuallyDrop<Payload>,
    }

    // The three-callback shape is the intrinsic's, not a choice: `try_fn` runs the body,
    // `catch_fn` receives the payload pointer, and the union is how one stack slot carries the
    // argument in, the result out, or the payload out. This is `library/std/src/panicking.rs`
    // with the names changed.
    let mut data: Data<_, R> = Data { f: core::mem::ManuallyDrop::new(f) };
    let data_ptr = (&raw mut data) as *mut u8;

    fn do_call<F: FnOnce() -> R, R>(data: *mut u8) {
        // Safe: the union holds `f` on the way in, and nothing else reads it until this returns.
        unsafe {
            let data = data as *mut Data<F, R>;
            let f = core::mem::ManuallyDrop::take(&mut (*data).f);
            (*data).r = core::mem::ManuallyDrop::new(f());
        }
    }

    fn do_catch<F: FnOnce() -> R, R>(data: *mut u8, exception: *mut u8) {
        // Safe: reached only when the runtime caught a panic, and `exception` is the payload it
        // built. `__rust_panic_cleanup` takes ownership of it and hands back the `Box`.
        unsafe {
            let data = data as *mut Data<F, R>;
            let obj = cleanup(exception);
            (*data).p = core::mem::ManuallyDrop::new(obj);
        }
    }

    // Safe: the callbacks above uphold the intrinsic's contract - `do_call` initialises `r` on
    // the success path, `do_catch` initialises `p` on the unwind path, and exactly one runs.
    unsafe {
        if core::intrinsics::catch_unwind(do_call::<F, R>, data_ptr, do_catch::<F, R>) {
            Err(core::mem::ManuallyDrop::into_inner(data.p))
        } else {
            Ok(core::mem::ManuallyDrop::into_inner(data.r))
        }
    }
}

unsafe extern "Rust" {
    /// `panic_unwind`'s, and the reason this crate is short.
    ///
    /// Turns the runtime's exception object back into the `Box<dyn Any + Send>` the payload
    /// started as. Declaring it here binds to the copy already linked; there is no second one.
    ///
    /// **`#[rustc_std_internal_symbol]` is load-bearing on this declaration.** Without it the
    /// name goes through ordinary C mangling and the linker looks for `___rust_panic_cleanup`,
    /// which nothing exports - the failure is an undefined symbol at the very end of a five
    /// minute build. `library/std/src/panicking.rs` declares it exactly this way.
    #[rustc_std_internal_symbol]
    fn __rust_panic_cleanup(payload: *mut u8) -> Box<dyn Any + Send + 'static>;
}

unsafe fn cleanup(payload: *mut u8) -> Payload {
    // Safe: `payload` came from the runtime's catch, which is the only thing that produces one.
    unsafe { __rust_panic_cleanup(payload) }
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

/// Whether unwinding is actually compiled in.
///
/// `catch` cannot work under `panic = "abort"`: the compiler emits no landing pads, so a panic
/// aborts before the intrinsic sees it. It would still *compile*, which is the dangerous part -
/// containment that silently never runs. Call this once at startup and say so out loud.
pub const fn unwinding_is_enabled() -> bool {
    cfg!(panic = "unwind")
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

