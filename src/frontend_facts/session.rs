//! Many parse-only questions under one set of session globals.
//!
//! Every parse-only entry point ([`super::syntax::parses_as`], [`super::site::site_at`] and,
//! with the `diagnostics` feature, `diagnostics::diagnose_with`) needs rustc's session globals:
//! the symbol interner, the span interner and the hygiene tables. When the calling thread holds
//! none, each call builds a set, uses it once, and drops it. For a small input that setup is
//! most of the call. An editor asking on every keystroke, or a tool walking a corpus, asks the
//! same kind of question thousands of times in a row and pays it every time.
//!
//! Those entry points already reuse globals the thread holds. This module is the documented way
//! to hold some: [`with_session`] runs a closure under one set, and [`run_batch`] drives a
//! sequence of calls through sets that it recycles every so many calls. The `*_many` entry
//! points beside each single call are [`run_batch`] with [`RECYCLE_EVERY`].
//!
//! **Same answers.** A call under shared globals returns exactly what it returns on a thread
//! with none, which `tests/batch.rs` checks input by input. That holds because nothing a call
//! reports depends on what an earlier call left in the globals:
//!
//! - Each call builds its own `SourceMap`, so its one file starts where a fresh map starts, and
//!   every offset and every `line:col` in a message is the same as on a fresh thread.
//! - A `Symbol`'s number does depend on what was interned before it, but no answer carries a
//!   `Symbol`, and the checks use symbols only as lookup keys: nothing is ordered or iterated by
//!   one on the way out.
//! - The span interner hands out different indices for the same long span, and decoding gives
//!   the same `lo` and `hi` whichever index it got.
//! - Parsing expands nothing, so no call adds an expansion or a syntax context to the hygiene
//!   tables, and every span stays at the root context of the edition the globals were built
//!   with, which is 2024 here and on a fresh thread alike.
//!
//! **Caller's thread, nothing spawned.** A batch runs in order on the thread that called it and
//! holds no pool (`AGENTS.md`, House rules). A caller that wants several threads runs one batch
//! per thread; the globals are per thread, so the batches share nothing.

use alloc::vec::Vec;

use crate::rustc_span::create_session_if_not_set_then;
use crate::rustc_span::edition::Edition;

/// The number of calls the batch entry points (`parses_as_many`, `site_at_many`,
/// `diagnose_many`) make under one set of session globals before dropping it and building the
/// next.
///
/// The interners only grow: every new identifier and every long span a call meets stays until
/// the set is dropped, so a set that lives forever is a leak shaped like a cache. Recycling
/// bounds that at the cost of one setup per this many calls. The value was chosen by
/// measurement with `examples/analysis_throughput.rs`, which runs the same fixed corpus at
/// several values beside a fresh-globals arm and its repeated noise floor, and it may be
/// retuned the same way. The answers do not depend on it.
pub const RECYCLE_EVERY: usize = 256;

/// Run `f` under one set of edition 2024 session globals, so that every parse-only call inside
/// it (`parses_as`, `site_at`, `diagnose_with`) uses that set instead of building its own.
///
/// When the calling thread already holds session globals, `f` simply runs under those: a
/// thread holds one set at a time, and the parse-only calls would use them anyway. Their
/// edition then applies, as it does to a single call made there.
///
/// **The set grows while it is held.** Nothing is ever removed from the symbol or span
/// interner, so a long-running caller should not wrap its whole life in one `with_session`. It
/// should hold a set for a bounded number of calls and then let it go, which is what
/// [`run_batch`] does.
///
/// **Parse-only calls only.** [`super::analyze_source`] and [`super::check_source`] build a full
/// compiler session with its own globals and assert that the thread holds none, so calling
/// either inside `with_session` panics on that assertion. Call them outside it.
pub fn with_session<R>(f: impl FnOnce() -> R) -> R {
    // `create_session_if_not_set_then` is the check the parse-only calls make themselves: when
    // the thread holds no globals it builds `SessionGlobals::new(edition, &[], None)`, which is
    // exactly what `create_session_globals_then(Edition2024, &[], None, f)` builds, and when it
    // holds some it runs `f` under them rather than tripping the one-per-thread assertion.
    create_session_if_not_set_then(Edition::Edition2024, |_| f())
}

/// Run `call` on each of `inputs`, in order, on the caller's thread, under one set of session
/// globals that is dropped and rebuilt after every `every` calls. Returns one result per input,
/// in input order. Spawns nothing.
///
/// `every == 0` means never recycle: the whole batch runs under one set, which then grows with
/// the batch. A set is built only when there is an input left to run under it, so an empty
/// batch builds none.
///
/// When the calling thread already holds session globals, every call runs under those and
/// nothing is recycled, since this thread cannot hold a second set. See [`with_session`].
///
/// `call` should be a parse-only entry point or something built on them; the same restriction
/// on [`super::analyze_source`] and [`super::check_source`] applies. A panic out of `call`
/// propagates and drops the set in use; the parse-only calls turn the parser's own fatal
/// errors into `Err` before that point.
pub fn run_batch<I, T, R>(inputs: I, every: usize, mut call: impl FnMut(T) -> R) -> Vec<R>
where
    I: IntoIterator<Item = T>,
{
    let mut inputs = inputs.into_iter();
    let mut out = Vec::with_capacity(inputs.size_hint().0);
    let mut pending = inputs.next();
    while let Some(first) = pending.take() {
        // One set of globals per chunk. The chunk says whether it stopped because it was full,
        // in which case there may be more, or because the inputs ran out.
        let full = with_session(|| {
            out.push(call(first));
            let mut done = 1;
            while every == 0 || done < every {
                match inputs.next() {
                    Some(input) => {
                        out.push(call(input));
                        done += 1;
                    }
                    None => return false,
                }
            }
            true
        });
        // Pulled outside the chunk, so that a full chunk whose inputs happen to end exactly
        // at its boundary does not build a set for nothing.
        if full {
            pending = inputs.next();
        }
    }
    out
}
