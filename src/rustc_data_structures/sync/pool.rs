//! Everything the parallel shims take from `std` and `nagoya`, in one place.
//!
//! Compiled only with the `parallel` feature, which is what brings `nagoya` and, through it,
//! `std`. No other module of this crate names either; `owned_stage.rs` builds tasks out of what is
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

/// Owned jobs submitted and not yet finished across the application.
///
/// File tasks, nested body tasks and owned resolver decisions share one pool budget. A successor
/// inherits its predecessor's place; an owner executes work the budget cannot admit.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Submit `work` if the application-wide budget has room, and say whether it did. The budget is
/// the pool's worker count: one job per worker, whoever submitted it. Work not submitted stays
/// in its typed owned task, where its owner can claim and run it.
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
/// pool until it wakes. Owned-task waits are reactive and follow only already-running work.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    nagoya::block_on(future)
}

/// Run `f`, catching a panic that unwinds out of it.
pub(crate) fn catch<R>(f: impl FnOnce() -> R) -> Result<R, Payload> {
    std::panic::catch_unwind(core::panic::AssertUnwindSafe(f))
}

/// Width for a fresh parse-only session. Existing compiler sessions keep their own width.
pub(crate) fn parse_pool_width() -> usize {
    match mode::session_width() {
        0 => workers() + 1,
        width => width,
    }
}

struct RetryParseSerial;

/// Abandon an uncommitted parser transaction without invoking the panic hook.
pub(crate) fn retry_parse_serial() -> ! {
    std::panic::resume_unwind(alloc::boxed::Box::new(RetryParseSerial))
}

/// Catch only parser retry requests and the compiler's fatal sentinel.
pub(crate) fn speculate_parse<R>(f: impl FnOnce() -> R) -> Option<R> {
    match catch(f) {
        Ok(value) => Some(value),
        Err(payload) => {
            let fatal = payload
                .downcast_ref::<&'static str>()
                .is_some_and(|s| *s == "rustc fatal error (compilation refused)")
                || payload
                    .downcast_ref::<alloc::string::String>()
                    .is_some_and(|s| s == "rustc fatal error (compilation refused)");
            let message = crate::unwind_janky::take_last_panic();
            if payload.is::<RetryParseSerial>() || fatal { None } else { resume(payload, message) }
        }
    }
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

// Measured on a 4-core box: below these sizes the fixed cost of a speculative attempt (forking
// the span tables, one owned session and parser state per job, the merge) exceeds what the jobs
// save, and each refused attempt parses the file twice. `set_parse_thresholds(0, 0, 256)`
// restores unconditional speculation for tests and oracles.
const DEFAULT_FILE_TOKENS: usize = 20_000;
const DEFAULT_BODY_TOKENS: usize = 1_000;
const DEFAULT_SEGMENT_TOKENS: usize = 1_024;
static MIN_FILE_TOKENS: AtomicUsize = AtomicUsize::new(DEFAULT_FILE_TOKENS);
static MIN_BODY_TOKENS: AtomicUsize = AtomicUsize::new(DEFAULT_BODY_TOKENS);
static MIN_SEGMENT_TOKENS: AtomicUsize = AtomicUsize::new(DEFAULT_SEGMENT_TOKENS);
static REFUSED_STREAK: AtomicUsize = AtomicUsize::new(0);
static SKIP_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

/// Tune when speculative owned parsing is worth its fixed cost: the fewest tokens a file needs
/// before a parallel attempt, the fewest a deferred body needs, and the fewest a top-level
/// segment needs. For benchmarks; the defaults are the shipped thresholds.
pub fn set_parse_thresholds(file: usize, body: usize, segment: usize) {
    MIN_FILE_TOKENS.store(file, Ordering::Relaxed);
    MIN_BODY_TOKENS.store(body, Ordering::Relaxed);
    MIN_SEGMENT_TOKENS.store(segment, Ordering::Relaxed);
}

pub(crate) fn min_file_tokens() -> usize {
    MIN_FILE_TOKENS.load(Ordering::Relaxed)
}
pub(crate) fn min_body_tokens() -> usize {
    MIN_BODY_TOKENS.load(Ordering::Relaxed)
}
pub(crate) fn min_segment_tokens() -> usize {
    MIN_SEGMENT_TOKENS.load(Ordering::Relaxed)
}

/// Back off after refused attempts: a refused attempt costs a second, serial parse, so after `n`
/// consecutive refusals the next `2^n` attempts (at most 64) are not made. A commit resets it.
/// Only active with the shipped thresholds; zero thresholds mean unconditional speculation.
pub(crate) fn speculation_allowed() -> bool {
    if MIN_FILE_TOKENS.load(Ordering::Relaxed) == 0 {
        return true;
    }
    let mut skip = SKIP_ATTEMPTS.load(Ordering::Relaxed);
    while skip > 0 {
        match SKIP_ATTEMPTS.compare_exchange_weak(
            skip,
            skip - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return false,
            Err(seen) => skip = seen,
        }
    }
    true
}

/// Record whether an attempt committed, for [`speculation_allowed`].
pub(crate) fn speculation_outcome(committed: bool) {
    if committed {
        REFUSED_STREAK.store(0, Ordering::Relaxed);
    } else {
        let streak = (REFUSED_STREAK.fetch_add(1, Ordering::Relaxed) + 1).min(6);
        SKIP_ATTEMPTS.store(1 << streak, Ordering::Relaxed);
    }
}
