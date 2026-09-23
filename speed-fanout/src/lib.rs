//! Many independent frontend questions on several threads, driven from the caller's side.
//!
//! frontend spawns no threads and holds no pool: a call runs on the thread that makes it. This
//! crate is the caller that owns threads. A [`Driver`] starts `N` workers, each a
//! `eko::thread::Builder` thread with a 16 MB stack, and gives each one to ps-st3's fanout
//! [`Pool::run`], which spawns nothing itself. Work goes in as chunks of consecutive inputs;
//! answers come back in input order, the same answers the single calls give.
//!
//! # What a worker holds
//!
//! A parse-only chunk (`parses_as`, `site_at`, `diagnose_with`) runs under one set of session
//! globals from [`frontend::frontend_facts::session::with_session`], dropped and rebuilt every
//! [`RECYCLE_EVERY`] calls, because the interners only grow. An `analyze_source` chunk runs
//! with none, because `analyze_source` builds a full compiler session and asserts the thread
//! holds no globals. The globals live for a chunk rather than for the thread's whole life: the
//! scoped thread-local that holds them has to enclose the code that uses them, and the pool's
//! run loop between two chunks is not code this crate can enclose without also enclosing the
//! analyze chunks that must run outside.
//!
//! # Panics
//!
//! A panic that crosses a fanout worker's run loop takes the worker down (ps-st3,
//! `src/fanout/job.rs`, `Job::from_raw`'s contract), and a worker here is an `extern "C"`
//! pthread entry, where an unwind aborts the process. So every input runs inside
//! [`frontend::unwind_janky::catch`], and every chunk inside a second one. A caught panic is that
//! input's error, carrying what the panic said as recorded by the hook [`install`] sets.
//! After a caught panic the chunk drops its session globals and builds a fresh set, since a
//! panic can leave them half-updated.

use std::sync::{Arc, Once};

use eko::thread::{Builder, Condvar, JoinHandle, Mutex};
use frontend::frontend_facts::diagnostics::{Diagnostic, Options, diagnose_with};
use frontend::frontend_facts::session::{RECYCLE_EVERY, with_session};
use frontend::frontend_facts::site::{Site, site_at};
use frontend::frontend_facts::syntax::{Fragment, parses_as};
use frontend::frontend_facts::{CrateFacts, analyze_source};
use frontend::rustc_span::fatal_error::FatalError;
use frontend::unwind_janky;
use st3::fanout::Pool;

pub mod host;

pub use host::EkoHost;

/// The stack every worker gets. rustc recurses deeply, and this is what `rustc_interface`
/// gives its own compilation thread; the platform default is far too small.
pub const WORKER_STACK: usize = 16 * 1024 * 1024;

/// Chunks per worker in one batch. More than one, so a worker that drew a chunk of large inputs
/// does not leave the rest idle at the end; few enough that a chunk still amortises its globals.
const CHUNKS_PER_WORKER: usize = 4;

/// Each worker's private deque. A batch never holds more than `CHUNKS_PER_WORKER` chunks per
/// worker, and the pool's injector is unbounded anyway, so this is a size, not a limit.
const DEQUE_CAPACITY: usize = 256;

/// What `FatalError::raise` panics with: frontend's refusal sentinel, which its own
/// `catch_fatal_errors` turns into `Err`. Mirrored here, since frontend keeps it private, only
/// so the hook can recognise an expected refusal and keep it off stderr.
const FATAL_SENTINEL: &str = "rustc fatal error (compilation refused)";

/// A job that panicked. Carries what the panic said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Panicked(pub String);

impl core::fmt::Display for Panicked {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "panic: {}", self.0)
    }
}

/// Why an `analyze_source` job produced no facts.
///
/// `analyze_source` returns `Err(FatalError)`, which carries no message, so a panic cannot be
/// folded into it the way the parse-only calls fold one into their `Vec<String>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnalyzeError {
    /// What `analyze_source` itself returned: the program has no HIR to read.
    Fatal,
    /// The call panicked, and this is what it said.
    Panicked(String),
}

impl From<FatalError> for AnalyzeError {
    fn from(_: FatalError) -> Self {
        AnalyzeError::Fatal
    }
}

/// Whether a job's inputs run under shared session globals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Globals {
    /// One set per chunk, recycled every [`RECYCLE_EVERY`] calls. For the parse-only calls.
    Shared,
    /// None at all. For `analyze_source`, which builds and asserts its own.
    None,
}

/// Install `std::panic::catch_unwind` as frontend's catcher and a panic hook that records each
/// panic's message through `unwind_janky::record_panic`. Idempotent; [`Driver::new`] calls it.
///
/// A serial caller on its own thread needs this too, since every entry point asserts a
/// catcher is installed.
///
/// The hook keeps frontend's expected refusals (`FatalError::raise`) off stderr and does not
/// record them: `catch_fatal_errors` turns them into `Err` and nobody reads their message.
/// Every other panic is recorded and then printed by whatever hook was installed before.
pub fn install() {
    static HOOK: Once = Once::new();
    unwind_janky::install_catcher(catcher);
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let text = payload_text(info.payload());
            if text == FATAL_SENTINEL {
                return;
            }
            let what = match info.location() {
                Some(at) => format!("{text} ({}:{}:{})", at.file(), at.line(), at.column()),
                None => text.to_string(),
            };
            unwind_janky::record_panic(what);
            previous(info);
        }));
    });
}

fn catcher(f: &mut dyn FnMut()) -> Result<(), unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn payload_text(payload: &(dyn core::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "a panic with a non-string payload"
    }
}

/// Run `f`, turning a panic out of it into [`Panicked`].
fn guarded<R>(f: impl FnOnce() -> R) -> Result<R, Panicked> {
    unwind_janky::catch(f).map_err(|payload| {
        let text = payload_text(&*payload);
        // A refusal the hook declined to record has nothing in the slot to take, and taking
        // anyway would hand this input somebody else's message.
        if text == FATAL_SENTINEL {
            return Panicked(text.to_string());
        }
        Panicked(unwind_janky::take_last_panic().unwrap_or_else(|| text.to_string()))
    })
}

/// Run `f` on each of `items`, in order, on this thread.
fn run_chunk<T, R>(items: &[T], globals: Globals, f: &impl Fn(&T) -> R) -> Vec<Result<R, Panicked>> {
    match globals {
        Globals::None => items.iter().map(|item| guarded(|| f(item))).collect(),
        Globals::Shared => {
            let every = RECYCLE_EVERY.max(1);
            let mut out = Vec::with_capacity(items.len());
            while out.len() < items.len() {
                // One set of globals for up to `every` calls, or until a call panics.
                with_session(|| {
                    let mut used = 0;
                    while out.len() < items.len() && used < every {
                        let result = guarded(|| f(&items[out.len()]));
                        let panicked = result.is_err();
                        out.push(result);
                        used += 1;
                        if panicked {
                            break;
                        }
                    }
                });
            }
            out
        }
    }
}

/// One batch in flight: its chunks' results, and how many chunks have not reported.
struct Batch<R> {
    state: Mutex<BatchState<R>>,
    finished: Condvar,
}

struct BatchState<R> {
    outstanding: usize,
    chunks: Vec<Option<Vec<Result<R, Panicked>>>>,
}

/// `N` caller-owned worker threads running a ps-st3 fanout pool.
///
/// Dropping it shuts the pool down and joins every worker. Several threads may submit batches
/// to one driver at once; batches share the workers and nothing else.
pub struct Driver {
    pool: Arc<Pool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl core::fmt::Debug for Driver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Driver").field("workers", &self.pool.workers()).finish_non_exhaustive()
    }
}

impl Driver {
    /// Start `workers` threads, each running one worker of a fresh pool.
    ///
    /// # Panics
    ///
    /// If `workers` is zero or more than the bits in a `usize` (the pool's limit), or if a
    /// thread cannot be started. Threads already started are shut down and joined first.
    pub fn new(workers: usize) -> Self {
        install();
        let pool = Pool::new(workers, DEQUE_CAPACITY, Arc::new(EkoHost::new(workers)));
        let mut threads = Vec::with_capacity(workers);
        for id in 0..workers {
            let runner = pool.runner(id);
            let pool_here = Arc::clone(&pool);
            let spawned = Builder::new()
                .stack_size(WORKER_STACK)
                .name(format!("speed-fanout-{id}"))
                .spawn(move || {
                    // `false` only if this runner were already running, which cannot happen:
                    // each runner is handed to exactly one thread.
                    let _ = pool_here.run(runner);
                });
            match spawned {
                Ok(handle) => threads.push(handle),
                Err(errno) => {
                    pool.shut_down();
                    for handle in threads {
                        let _ = handle.join();
                    }
                    panic!("could not start fanout worker {id}: {errno:?}");
                }
            }
        }
        Self { pool, threads: Mutex::new(threads) }
    }

    /// How many workers this driver runs.
    pub fn workers(&self) -> usize {
        self.pool.workers()
    }

    /// Run `f` on every input, spread across the workers, and return the results in input
    /// order. A panic in `f` is caught and becomes that input's `Err`; the worker carries on.
    ///
    /// Blocks the calling thread until every input has an answer.
    pub fn map<T, R, F>(&self, inputs: Vec<T>, globals: Globals, f: F) -> Vec<Result<R, Panicked>>
    where
        T: Send + Sync + 'static,
        R: Send + 'static,
        F: Fn(&T) -> R + Send + Sync + 'static,
    {
        let total = inputs.len();
        if total == 0 {
            return Vec::new();
        }
        let chunks = (self.workers() * CHUNKS_PER_WORKER).min(total);
        let size = total.div_ceil(chunks);
        let chunks = total.div_ceil(size);

        let inputs = Arc::new(inputs);
        let f = Arc::new(f);
        let batch = Arc::new(Batch {
            state: Mutex::new(BatchState { outstanding: chunks, chunks: (0..chunks).map(|_| None).collect() }),
            finished: Condvar::new(),
        });

        for chunk in 0..chunks {
            let start = chunk * size;
            let end = (start + size).min(total);
            let inputs = Arc::clone(&inputs);
            let f = Arc::clone(&f);
            let batch = Arc::clone(&batch);
            self.pool.submit_fn(move || {
                let items = &inputs[start..end];
                // The second catch: whatever escapes the per-input one, `with_session` included,
                // still must not cross the worker's run loop.
                let results = match unwind_janky::catch(|| run_chunk(items, globals, &*f)) {
                    Ok(results) => results,
                    Err(payload) => {
                        let what = unwind_janky::take_last_panic()
                            .unwrap_or_else(|| payload_text(&*payload).to_string());
                        items.iter().map(|_| Err(Panicked(what.clone()))).collect()
                    }
                };
                let mut state = batch.state.lock();
                state.chunks[chunk] = Some(results);
                state.outstanding -= 1;
                if state.outstanding == 0 {
                    batch.finished.notify_all();
                }
                drop(state);
            });
        }

        let mut state = batch.state.lock();
        while state.outstanding > 0 {
            state = batch.finished.wait(state);
        }
        let mut out = Vec::with_capacity(total);
        for results in state.chunks.iter_mut() {
            out.extend(results.take().expect("every chunk reported before the batch finished"));
        }
        out
    }

    /// [`parses_as`] on every `(source, kind)`, in order.
    pub fn parses_as_many<S: AsRef<str>>(&self, inputs: &[(S, Fragment)]) -> Vec<Result<(), Vec<String>>> {
        let owned: Vec<(String, Fragment)> =
            inputs.iter().map(|(source, kind)| (source.as_ref().to_string(), *kind)).collect();
        fold(self.map(owned, Globals::Shared, |(source, kind)| parses_as(source, *kind)))
    }

    /// [`site_at`] on every `(source, offset)`, in order.
    pub fn site_at_many<S: AsRef<str>>(&self, inputs: &[(S, u32)]) -> Vec<Result<Site, Vec<String>>> {
        let owned: Vec<(String, u32)> =
            inputs.iter().map(|(source, offset)| (source.as_ref().to_string(), *offset)).collect();
        fold(self.map(owned, Globals::Shared, |(source, offset)| site_at(source, *offset)))
    }

    /// [`diagnose_with`] on every source with the same `opts`, in order.
    pub fn diagnose_many<S: AsRef<str>>(
        &self,
        sources: &[S],
        opts: &Options,
    ) -> Vec<Result<Vec<Diagnostic>, Vec<String>>> {
        let owned: Vec<String> = sources.iter().map(|source| source.as_ref().to_string()).collect();
        let opts = opts.clone();
        fold(self.map(owned, Globals::Shared, move |source| diagnose_with(source, &opts)))
    }

    /// [`analyze_source`] on every `(crate_name, source)`, in order, with no session globals
    /// held around the calls.
    pub fn analyze_many<N: AsRef<str>, S: AsRef<str>>(
        &self,
        inputs: &[(N, S)],
    ) -> Vec<Result<CrateFacts, AnalyzeError>> {
        let owned: Vec<(String, String)> = inputs
            .iter()
            .map(|(name, source)| (name.as_ref().to_string(), source.as_ref().to_string()))
            .collect();
        self.map(owned, Globals::None, |(name, source)| analyze_source(name, source))
            .into_iter()
            .map(|result| match result {
                Ok(Ok(facts)) => Ok(facts),
                Ok(Err(fatal)) => Err(AnalyzeError::from(fatal)),
                Err(Panicked(what)) => Err(AnalyzeError::Panicked(what)),
            })
            .collect()
    }
}

/// Fold a caught panic into the parse-only calls' own error shape: one more message.
fn fold<T>(results: Vec<Result<Result<T, Vec<String>>, Panicked>>) -> Vec<Result<T, Vec<String>>> {
    results
        .into_iter()
        .map(|result| match result {
            Ok(answer) => answer,
            Err(panicked) => Err(vec![panicked.to_string()]),
        })
        .collect()
}

impl Drop for Driver {
    /// Shut the pool down and join every worker, so no thread outlives the driver.
    fn drop(&mut self) {
        self.pool.shut_down();
        let threads = core::mem::take(&mut *self.threads.lock());
        for handle in threads {
            let _ = handle.join();
        }
    }
}
