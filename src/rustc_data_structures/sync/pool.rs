//! Everything the parallel shims take from `std` and `nagoya`, in one place.
//!
//! Compiled only with the `parallel` feature, which is what brings `nagoya` and, through it,
//! `std`. No other module of this crate names either; `stage.rs` builds stages out of what is
//! here. `Cargo.toml` says why the feature is off by default.
//!
//! # Frontend owns no thread
//!
//! Every thread that runs a stage's items is the caller's own or one of nagoya's pool workers: by
//! default `nagoya::runtime::background()`, the process's shared pool, or the pool a program
//! handed over with [`set_parallel_executor`]. This module submits closures and parks the calling
//! thread; it starts nothing.
//!
//! The executor is fetched per use rather than kept anywhere: `EXECUTOR` is configuration the
//! program sets once, and `background()` is nagoya's own one-per-process pool.
//!
//! # Why `std`'s catch and not `unwind_janky::catch`
//!
//! `crate::unwind_janky::catch` catches only once a program installed a catcher, and runs the
//! closure bare otherwise. A panic escaping an item on a pool worker would then unwind out of the
//! pool job, the stage would never learn the item settled, and whoever waits for it would wait
//! forever. `std` is linked whenever this module is, so its `catch_unwind` is always there, and
//! its `resume_unwind` re-raises the **original payload**, which `unwind_janky::resume` cannot:
//! the `FatalError` sentinel still reads as one to `catch_fatal_errors` after it crossed a thread.

extern crate std;

use core::future::Future;

use eko::thread::OnceLock;

use crate::rustc_data_structures::sync::mode;
use crate::unwind_janky::Payload;

/// The pool a program handed over, if it did. Configuration, set once; not a cache.
static EXECUTOR: OnceLock<nagoya::Executor> = OnceLock::new();

/// Run parallel frontend work on `executor`'s pool instead of nagoya's shared one.
///
/// **For a program that owns its workers.** nagoya starts the shared pool's threads with the
/// platform's default stack, and the compiler recurses deeply enough to want 16 MiB (see
/// `NAGOYA-STACK-PATCH.md` at the repository root). A program that builds its own
/// `st3::fanout::Pool`, starts its threads with the stack it wants and runs each with
/// `nagoya::Executor::run_worker`, passes the executor here.
///
/// Call it before the first parallel session. The first call wins; a later one hands its argument
/// back as the `Err`.
pub fn set_parallel_executor(executor: nagoya::Executor) -> Result<(), nagoya::Executor> {
    EXECUTOR.set(executor)
}

fn executor() -> &'static nagoya::Executor {
    match EXECUTOR.get() {
        Some(executor) => executor,
        None => nagoya::runtime::background().executor(),
    }
}

/// The pool's worker count.
pub(crate) fn workers() -> usize {
    executor().pool().workers()
}

/// How many threads a stage may use, the caller included: the session's `jobs.frontend`, or, on
/// a thread outside any session, the pool's width plus the caller.
pub(crate) fn width() -> usize {
    match mode::session_width() {
        0 => workers() + 1,
        width => width,
    }
}

/// Hand `work` to the pool. It runs once, on some worker, start to finish.
pub(crate) fn submit(work: impl FnOnce() + Send + 'static) {
    executor().submit(work);
}

/// Park this thread until `future` completes. On a pool worker this takes the worker out of the
/// pool until it wakes; see `stage.rs` for why every wait here ends.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    nagoya::block_on(future)
}

/// Run `f`, catching a panic that unwinds out of it.
pub(crate) fn catch<R>(f: impl FnOnce() -> R) -> Result<R, Payload> {
    std::panic::catch_unwind(core::panic::AssertUnwindSafe(f))
}

/// Re-raise a caught panic with its original payload, on this thread.
///
/// `message` is what the panic handler recorded on the thread that panicked (see
/// `unwind_janky::record_panic`); it is recorded again here first, so a `take_last_panic` after
/// this thread's own catch finds it. `resume_unwind` does not run the panic hook, so the re-raise
/// does not overwrite it.
pub(crate) fn resume(payload: Payload, message: Option<alloc::string::String>) -> ! {
    if let Some(message) = message {
        crate::unwind_janky::record_panic(message);
    }
    std::panic::resume_unwind(payload)
}
