//! Everything the parallel shims take from `std` and `nagoya`, in one place.
//!
//! Compiled only with the `parallel` feature, which is what brings `nagoya` and, through it,
//! `std`. No other module of this crate names either; `stage.rs` builds stages out of what is
//! here. `Cargo.toml` says why the feature is off by default.
//!
//! # Frontend owns no thread
//!
//! Every thread that runs a stage's items is the caller's own or one of nagoya's pool workers: by
//! default a nagoya `Runtime` built here on first use, or the pool a program handed over with
//! [`set_parallel_executor`]. This module submits closures and parks the calling thread; the
//! runtime's threads are started by nagoya, not by this crate.
//!
//! # Why not nagoya's shared pool
//!
//! `nagoya::runtime::background()` starts its workers on `std`'s default stack, and the compiler
//! is written for 16 MiB: upstream starts its threads at `DEFAULT_STACK_SIZE`
//! (`rustc_interface::util`), and this tree has no stack-growing fallback, so a deep item on a
//! default stack overflows and takes the process with it. So the default pool is one this module
//! asks nagoya for, shaped like `background()` in every other respect, with `WORKER_STACK`
//! bytes per worker.
//!
//! The executor is fetched per use rather than kept anywhere: `EXECUTOR` is configuration the
//! program sets once, and `RUNTIME` is started once, only if nothing was handed over.
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
use core::sync::atomic::{AtomicUsize, Ordering};

use eko::thread::OnceLock;

use crate::rustc_data_structures::sync::mode;
use crate::unwind_janky::Payload;

/// Each default pool worker's stack, in bytes: upstream's `DEFAULT_STACK_SIZE`, which the
/// compiler's recursion depth is written against.
const WORKER_STACK: usize = 16 * 1024 * 1024;

/// The pool a program handed over, if it did. Configuration, set once; not a cache.
static EXECUTOR: OnceLock<nagoya::Executor> = OnceLock::new();

/// The default pool, started on first use when nothing was handed over. Kept for the life of the
/// process: dropping a `Runtime` does not stop its threads, and this is the one handle to them.
static RUNTIME: OnceLock<nagoya::runtime::Runtime> = OnceLock::new();

/// Run parallel frontend work on `executor`'s pool instead of the one this module starts.
///
/// **For a program that owns its workers**, or that already runs a nagoya pool and does not want
/// a second. Its threads run compiler items, so they want the 16 MiB stacks the default pool's
/// workers get, as `nagoya::runtime::Runtime::builder().stack_size(16 * 1024 * 1024)` gives them.
///
/// Call it before the first parallel session. The first call wins; a later one hands its argument
/// back as the `Err`.
pub fn set_parallel_executor(executor: nagoya::Executor) -> Result<(), nagoya::Executor> {
    EXECUTOR.set(executor)
}

fn executor() -> &'static nagoya::Executor {
    match EXECUTOR.get() {
        Some(executor) => executor,
        None => RUNTIME
            .get_or_init(|| {
                nagoya::runtime::Runtime::builder()
                    .label("frontend")
                    .stack_size(WORKER_STACK)
                    .build()
            })
            .executor(),
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

/// Fanout jobs submitted and not yet finished, across every scope of every session in the
/// process.
///
/// **The budget is the application's, not a scope's.** There is one pool, and a stage inside an
/// item of another stage, or a stage of another session, draws on the same workers. Counting
/// per scope let every scope submit up to its own width, so nested and concurrent scopes
/// together queued many times the pool's size, and the surplus only contended: jobs that found
/// no registry slot, and locks fought over by more runners than there are cores.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Submit `work` if the application-wide budget has room, and say whether it did. The budget is
/// the pool's worker count: one job per worker, whoever submitted it. Work not submitted stays
/// with its scope, whose owner runs it.
pub(crate) fn try_submit(work: impl FnOnce() + Send + 'static) -> bool {
    let limit = workers();
    let mut current = IN_FLIGHT.load(Ordering::Relaxed);
    loop {
        if current >= limit {
            return false;
        }
        match IN_FLIGHT.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(seen) => current = seen,
        }
    }
    launch(work);
    true
}

/// Submit `work` as the continuation of the job calling this: it takes over that job's place in
/// the budget, which the caller gives up as it ends, so the count is back where it was once the
/// caller returns. Called only from inside a job `try_submit` started.
pub(crate) fn submit_continuation(work: impl FnOnce() + Send + 'static) {
    IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    launch(work);
}

/// Hand a budgeted job to the pool, releasing its place in the budget when it ends.
fn launch(work: impl FnOnce() + Send + 'static) {
    executor().submit(move || {
        // Released when the job ends however it ends, so a panic cannot leak budget.
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _release = Release;
        work()
    });
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
