//! Temporary stage timing: exclusive time per compiler stage, for a front-to-back throughput
//! table (`examples/stage_throughput.rs`). Off unless the `stage-timing` feature is on; without
//! it every entry point is an empty inline function and a guard is a zero-sized value.
//!
//! **Exclusive time.** The compiler is query-driven: borrow checking demands MIR, MIR demands
//! THIR, THIR demands type checking, all inside one another. A wall-clock scope around a pass
//! would charge all of that to the outermost pass. Here each thread keeps the slot it is
//! currently charging and when it started; entering a slot charges the elapsed time to the one
//! it interrupts, and leaving charges its own time and resumes the interrupted one. Every
//! nanosecond on a thread goes to exactly one slot.
//!
//! **Slots.** Every query has one, by its dep kind ([`enter_query`], from the query engine's
//! one execution path), so no work escapes into an unnamed bucket. The rest are the hand-placed
//! [`Marker`]s: the stages before the query system exists (source loading, lexing, parsing,
//! expansion) and [`Marker::Session`], an outer slot that takes whatever no other slot claims.
//! Grouping slots into stages is the report's business, not the recorder's.
//!
//! **Cost.** Two clock reads and a thread-local lookup per transition; a query is one
//! transition in and one out. That is overhead on every query the table includes, so compare
//! stages with each other, not with an untimed build.

/// Hand-placed slots, for work outside any query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Marker {
    /// Everything on the calling thread no other slot claims: session and global context setup,
    /// and the query engine's own work between providers.
    Session,
    /// Source map registration: line, multi-byte and normalization tables.
    Source,
    /// Tokenizing: the indexed token tape, or the eager lexer when the tape refuses a file.
    Lex,
    /// Parsing to an AST, lexing excluded.
    Parse,
    /// Crate injection and macro expansion, with the early name resolution it interleaves.
    Expand,
}

impl Marker {
    pub const ALL: [Marker; 5] =
        [Marker::Session, Marker::Source, Marker::Lex, Marker::Parse, Marker::Expand];

    pub fn name(self) -> &'static str {
        match self {
            Marker::Session => "session",
            Marker::Source => "source",
            Marker::Lex => "lex",
            Marker::Parse => "parse",
            Marker::Expand => "expand",
        }
    }
}

/// One slot's total over a measured interval.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// A query's name, or a marker's.
    pub name: &'static str,
    /// Whether the slot is a query's.
    pub query: bool,
    /// Exclusive nanoseconds, summed over threads.
    pub nanos: u64,
    /// Times the slot was entered.
    pub entries: u64,
}

#[cfg(feature = "stage-timing")]
mod imp {
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering::Relaxed};

    use eko::time::Instant;

    use super::{Marker, Sample};

    /// Query slots: dep kinds below this; a larger one shares the last slot.
    const QUERY_SLOTS: usize = 1024;
    const SLOTS: usize = QUERY_SLOTS + Marker::ALL.len();

    static NANOS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    static ENTRIES: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    /// A query slot's name, as pointer and length, written on its first entry.
    static NAME_PTR: [AtomicPtr<u8>; QUERY_SLOTS] =
        [const { AtomicPtr::new(core::ptr::null_mut()) }; QUERY_SLOTS];
    static NAME_LEN: [AtomicUsize; QUERY_SLOTS] = [const { AtomicUsize::new(0) }; QUERY_SLOTS];

    /// What this thread is charging, and since when.
    struct Current {
        slot: Option<usize>,
        since: Instant,
    }

    static CURRENT: eko::thread::ThreadLocal<Current> = eko::thread::ThreadLocal::new();

    fn new_current() -> Current {
        Current { slot: None, since: Instant::now() }
    }

    fn charge(slot: usize, since: Instant, now: Instant) {
        NANOS[slot].fetch_add(now.duration_since(since).as_nanos() as u64, Relaxed);
    }

    /// Charges the interrupted slot and makes `slot` current; the guard restores the previous.
    pub(super) fn enter(slot: usize) -> Option<usize> {
        ENTRIES[slot].fetch_add(1, Relaxed);
        CURRENT
            .with(new_current, |cur| {
                let now = Instant::now();
                if let Some(previous) = cur.slot {
                    charge(previous, cur.since, now);
                }
                cur.since = now;
                cur.slot.replace(slot)
            })
            .flatten()
    }

    pub(super) fn leave(previous: Option<usize>) {
        CURRENT.with(new_current, |cur| {
            let now = Instant::now();
            if let Some(slot) = cur.slot {
                charge(slot, cur.since, now);
            }
            cur.since = now;
            cur.slot = previous;
        });
    }

    pub(super) fn query_slot(dep_kind: usize, name: &'static str) -> usize {
        let slot = dep_kind.min(QUERY_SLOTS - 1);
        if NAME_PTR[slot].load(Relaxed).is_null() {
            NAME_LEN[slot].store(name.len(), Relaxed);
            NAME_PTR[slot].store(name.as_ptr().cast_mut(), Relaxed);
        }
        slot
    }

    pub(super) fn marker_slot(marker: Marker) -> usize {
        QUERY_SLOTS + marker as usize
    }

    pub(super) fn reset() {
        for slot in 0..SLOTS {
            NANOS[slot].store(0, Relaxed);
            ENTRIES[slot].store(0, Relaxed);
        }
    }

    pub(super) fn snapshot() -> Vec<Sample> {
        let mut samples = Vec::new();
        for slot in 0..SLOTS {
            let entries = ENTRIES[slot].load(Relaxed);
            if entries == 0 {
                continue;
            }
            let (name, query) = if slot < QUERY_SLOTS {
                let ptr = NAME_PTR[slot].load(Relaxed);
                let name = if ptr.is_null() {
                    "?"
                } else {
                    // SAFETY: the pointer and length were taken together from one
                    // `&'static str` (a query's name), which is valid UTF-8 for the
                    // program's whole run. A racing first entry writes the same query's name.
                    unsafe {
                        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                            ptr,
                            NAME_LEN[slot].load(Relaxed),
                        ))
                    }
                };
                (name, true)
            } else {
                (Marker::ALL[slot - QUERY_SLOTS].name(), false)
            };
            samples.push(Sample { name, query, nanos: NANOS[slot].load(Relaxed), entries });
        }
        samples
    }
}

/// Charges this thread's time to a slot until dropped.
#[must_use]
pub struct StageGuard {
    #[cfg(feature = "stage-timing")]
    previous: Option<usize>,
}

impl Drop for StageGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "stage-timing")]
        imp::leave(self.previous);
    }
}

/// Charges this thread's time to `marker` until the guard drops.
#[inline]
pub fn enter(marker: Marker) -> StageGuard {
    #[cfg(feature = "stage-timing")]
    return StageGuard { previous: imp::enter(imp::marker_slot(marker)) };
    #[cfg(not(feature = "stage-timing"))]
    {
        let _ = marker;
        StageGuard {}
    }
}

/// Charges this thread's time to the query with this dep kind until the guard drops.
#[inline]
pub fn enter_query(dep_kind: usize, name: &'static str) -> StageGuard {
    #[cfg(feature = "stage-timing")]
    return StageGuard { previous: imp::enter(imp::query_slot(dep_kind, name)) };
    #[cfg(not(feature = "stage-timing"))]
    {
        let _ = (dep_kind, name);
        StageGuard {}
    }
}

/// Whether this build records anything.
pub const ENABLED: bool = cfg!(feature = "stage-timing");

/// Zeroes every slot. Call with nothing being timed.
pub fn reset() {
    #[cfg(feature = "stage-timing")]
    imp::reset();
}

/// Every slot entered since the last [`reset`]. Empty without the feature.
pub fn snapshot() -> alloc::vec::Vec<Sample> {
    #[cfg(feature = "stage-timing")]
    return imp::snapshot();
    #[cfg(not(feature = "stage-timing"))]
    alloc::vec::Vec::new()
}
