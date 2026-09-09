//! # `-Z time-passes`, and the corpse of `-Z self-profile`
//!
//! This module used to implement the compiler's self-profiling support: a `SelfProfiler` that
//! recorded typed "events" (a start, an end, a kind and an id) into a binary log for the
//! `measureme` tool suite to post-process.
//!
//! **That is gone.** `measureme` is a std-only external crate, and `std` is banned in this tree
//! (see `AGENTS.md`), so the recording half was deleted rather than ported. `-Z self-profile`
//! now warns that it is unsupported instead of silently producing nothing.
//!
//! What is left is the shape of the API and the one consumer that never went through
//! `measureme`:
//!
//! - [`SelfProfilerRef`] keeps every entry point its ~93 call sites use -
//!   `generic_activity`, `verbose_generic_activity`, `query_provider`, `artifact_size` and the
//!   rest. They are now the cheap no-op path that already existed for a compilation session
//!   with profiling disabled, so the call sites did not have to be touched and the
//!   instrumentation stays in place for whatever records events next.
//! - [`VerboseTimingGuard`] and [`print_time_passes_entry`] are `-Z time-passes`. That prints
//!   to stderr from [`Instant`] and RSS and never touched `measureme`, so it is unaffected.
//!
//! To restore self-profiling, a recorder has to be written against `ekostd` (or
//! `measureme` has to gain a `no_std` path upstream) and hung back off `SelfProfilerRef`. The
//! `-Z self-profile*` flags, [`EventFilter`] and [`QueryInvocationId`] are kept for that: they
//! are the vocabulary the flags parse into and the id scheme the events were keyed by.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::borrow::Borrow;
use core::fmt::Display;
use core::marker::PhantomData;
use core::time::Duration;
use eko::file as fs;
use eko::time::Instant;

bitflags::bitflags! {
    /// The classes of event `-Z self-profile-events` selects between.
    ///
    /// Nothing records events since `measureme` was removed, so every variant below is
    /// currently unread - hence the `allow`. The type is kept because it is what the
    /// `-Z self-profile-events` names in `rustc_session/src/options.rs` parse into, and
    /// because each `SelfProfilerRef` method still states which class it belongs to.
    #[allow(dead_code)]
    #[derive(Clone, Copy)]
    struct EventFilter: u16 {
        const GENERIC_ACTIVITIES  = 1 << 0;
        const QUERY_PROVIDERS     = 1 << 1;
        /// Store detailed instant events, including timestamp and thread ID,
        /// per each query cache hit. Note that this is quite expensive.
        const QUERY_CACHE_HITS    = 1 << 2;
        const QUERY_BLOCKED       = 1 << 3;
        const INCR_CACHE_LOADS    = 1 << 4;

        const QUERY_KEYS          = 1 << 5;
        const FUNCTION_ARGS       = 1 << 6;
        const LLVM                = 1 << 7;
        const INCR_RESULT_HASHING = 1 << 8;
        const ARTIFACT_SIZES      = 1 << 9;
        /// Store aggregated counts of cache hits per query invocation.
        const QUERY_CACHE_HIT_COUNTS  = 1 << 10;

        const DEFAULT = Self::GENERIC_ACTIVITIES.bits() |
                        Self::QUERY_PROVIDERS.bits() |
                        Self::QUERY_BLOCKED.bits() |
                        Self::INCR_CACHE_LOADS.bits() |
                        Self::INCR_RESULT_HASHING.bits() |
                        Self::ARTIFACT_SIZES.bits() |
                        Self::QUERY_CACHE_HIT_COUNTS.bits();

        const ARGS = Self::QUERY_KEYS.bits() | Self::FUNCTION_ARGS.bits();
        const QUERY_CACHE_HIT_COMBINED = Self::QUERY_CACHE_HITS.bits() | Self::QUERY_CACHE_HIT_COUNTS.bits();
    }
}

// The `&str` -> `EventFilter` table that parsed `-Z self-profile-events` lived here. It was
// only ever read by `SelfProfiler::new`, which is gone with `measureme`, so it went with it.
// The names it mapped are still listed in the `-Z self-profile-events` help message in
// `rustc_session/src/options.rs`; restore the table beside a new recorder.

/// Something that uniquely identifies a query invocation.
pub struct QueryInvocationId(pub u32);

/// Which format to use for `-Z time-passes`
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum TimePassesFormat {
    /// Emit human readable text
    Text,
    /// Emit structured JSON
    Json,
}

/// A handle to the compiler's (currently absent) self-profiler.
///
/// It can be cloned and sent across thread boundaries at will. Every method below is a no-op:
/// `measureme` was the only recorder and it is gone. They are kept, with their exact
/// signatures, so the ~93 instrumentation sites across the compiler keep compiling and keep
/// saying what they were measuring.
#[derive(Clone)]
pub struct SelfProfilerRef {
    // The self-profiler itself was here. Its `Option<Arc<SelfProfiler>>` is gone with
    // `measureme`, and so is the event filter mask that shadowed it - with no recorder the
    // mask is unconditionally empty, so `exec` always takes the disabled path.

    // Print verbose generic activities to stderr. This is `-Z time-passes`, which does NOT go
    // through the self-profiler, and is the one thing here that still does work.
    print_verbose_generic_activities: Option<TimePassesFormat>,
}

impl SelfProfilerRef {
    pub fn new(print_verbose_generic_activities: Option<TimePassesFormat>) -> SelfProfilerRef {
        SelfProfilerRef { print_verbose_generic_activities }
    }

    /// The shim every event-recording method below goes through.
    ///
    /// It used to check the event filter mask and, on a hit, make an out-of-line cold call into
    /// the profiler. With no profiler there is nothing to filter and nothing to call, so it is
    /// unconditionally the "profiling disabled" arm. The `EventFilter` argument is kept at each
    /// call site because it is the record of which `-Z self-profile-events` class that site
    /// belonged to, and a new recorder will need it back.
    #[inline(always)]
    fn exec(&self, _event_filter: EventFilter) -> TimingGuard<'_> {
        TimingGuard::none()
    }

    /// Start profiling a verbose generic activity. Profiling continues until the
    /// VerboseTimingGuard returned from this call is dropped. "Verbose" generic activities
    /// print a timing entry to stderr if the compiler is invoked with -Ztime-passes; that half
    /// still works, the event-recording half does not.
    pub fn verbose_generic_activity(&self, event_label: &'static str) -> VerboseTimingGuard<'_> {
        let message_and_format =
            self.print_verbose_generic_activities.map(|format| (event_label.to_owned(), format));

        VerboseTimingGuard::start(message_and_format, self.generic_activity(event_label))
    }

    /// Like `verbose_generic_activity`, but with an extra arg.
    pub fn verbose_generic_activity_with_arg<A>(
        &self,
        event_label: &'static str,
        event_arg: A,
    ) -> VerboseTimingGuard<'_>
    where
        A: Borrow<str> + Into<String>,
    {
        let message_and_format = self
            .print_verbose_generic_activities
            .map(|format| (format!("{}({})", event_label, event_arg.borrow()), format));

        VerboseTimingGuard::start(
            message_and_format,
            self.generic_activity_with_arg(event_label, event_arg),
        )
    }

    /// Start profiling a generic activity. Profiling continues until the
    /// TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn generic_activity(&self, _event_label: &'static str) -> TimingGuard<'_> {
        self.exec(EventFilter::GENERIC_ACTIVITIES)
    }

    /// Start profiling a generic activity. Profiling continues until the
    /// TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn generic_activity_with_arg<A>(
        &self,
        _event_label: &'static str,
        _event_arg: A,
    ) -> TimingGuard<'_>
    where
        A: Borrow<str> + Into<String>,
    {
        self.exec(EventFilter::GENERIC_ACTIVITIES)
    }

    /// Start profiling a generic activity, allowing costly arguments to be recorded. Profiling
    /// continues until the `TimingGuard` returned from this call is dropped.
    ///
    /// The closure is passed a `&mut EventArgRecorder` and is only called when argument
    /// recording is on. It never is now, so **the closure is never called** - which is the same
    /// contract it had whenever `-Z self-profile-events=args` was absent. Note that the panic
    /// upstream raises when the closure records no argument therefore cannot fire either.
    #[inline(always)]
    pub fn generic_activity_with_arg_recorder<F>(
        &self,
        _event_label: &'static str,
        _f: F,
    ) -> TimingGuard<'_>
    where
        F: FnMut(&mut EventArgRecorder<'_>),
    {
        self.exec(EventFilter::GENERIC_ACTIVITIES)
    }

    /// Record the size of an artifact that the compiler produces
    ///
    /// `artifact_kind` is the class of artifact (e.g., query_cache, object_file, etc.)
    /// `artifact_name` is an identifier to the specific artifact being stored (usually a filename)
    #[inline(always)]
    pub fn artifact_size<A>(&self, _artifact_kind: &str, _artifact_name: A, _size: u64)
    where
        A: Borrow<str> + Into<String>,
    {
        drop(self.exec(EventFilter::ARTIFACT_SIZES))
    }

    #[inline(always)]
    pub fn generic_activity_with_args(
        &self,
        _event_label: &'static str,
        _event_args: &[String],
    ) -> TimingGuard<'_> {
        self.exec(EventFilter::GENERIC_ACTIVITIES)
    }

    /// Start profiling a query provider. Profiling continues until the
    /// TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn query_provider(&self) -> TimingGuard<'_> {
        self.exec(EventFilter::QUERY_PROVIDERS)
    }

    /// Record a query in-memory cache hit.
    #[inline(always)]
    pub fn query_cache_hit(&self, _query_invocation_id: QueryInvocationId) {}

    /// Start profiling a query being blocked on a concurrent execution.
    /// Profiling continues until the TimingGuard returned from this call is
    /// dropped.
    #[inline(always)]
    pub fn query_blocked(&self) -> TimingGuard<'_> {
        self.exec(EventFilter::QUERY_BLOCKED)
    }

    /// Start profiling how long it takes to load a query result from the
    /// incremental compilation on-disk cache. Profiling continues until the
    /// TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn incr_cache_loading(&self) -> TimingGuard<'_> {
        self.exec(EventFilter::INCR_CACHE_LOADS)
    }

    /// Start profiling how long it takes to hash query results for incremental compilation.
    /// Profiling continues until the TimingGuard returned from this call is dropped.
    #[inline(always)]
    pub fn incr_result_hashing(&self) -> TimingGuard<'_> {
        self.exec(EventFilter::INCR_RESULT_HASHING)
    }

    /// Whether self-profiling is on. It is not, and cannot be, until a recorder replaces
    /// `measureme`.
    #[inline]
    pub fn enabled(&self) -> bool {
        false
    }

    #[inline]
    pub fn llvm_recording_enabled(&self) -> bool {
        false
    }

    /// Is expensive recording of query keys and/or function arguments enabled?
    pub fn is_args_recording_enabled(&self) -> bool {
        false
    }
}

/// A helper for recording costly arguments to self-profiling events. Used with
/// `SelfProfilerRef::generic_activity_with_arg_recorder`.
///
/// It interned its arguments into the profiler's string table. There is no string table, so
/// `record_arg` now discards its argument. The type stays because `rustc_span`'s
/// `SpannedEventArgRecorder` is implemented on it and `rustc_expand` calls through that.
pub struct EventArgRecorder<'p> {
    // Held the `&'p SelfProfiler` the arguments were interned into. The lifetime is kept so
    // that neither the trait in `rustc_span` nor its callers have to change.
    _profiler: PhantomData<&'p ()>,
}

impl EventArgRecorder<'_> {
    /// Records a single argument within the current generic activity being profiled.
    ///
    /// Discards it: nothing records events. Upstream's "at least one argument or panic" rule is
    /// not enforced here because the closure that would call this is never run.
    pub fn record_arg<A>(&mut self, _event_arg: A)
    where
        A: Borrow<str> + Into<String>,
    {
    }
}

/// The guard an in-progress event was ended by dropping.
///
/// It wrapped an `Option<measureme::TimingGuard>` and is now empty: there is no interval event
/// to close. It is still `#[must_use]` and still carries the borrow of the `SelfProfilerRef`,
/// so the `let _timer = ..` idiom at the call sites keeps meaning what it means, and a recorder
/// can be put back inside it without touching them. `TimingGuard::start` went with the
/// profiler it took.
#[must_use]
pub struct TimingGuard<'a>(PhantomData<&'a ()>);

impl<'a> TimingGuard<'a> {
    /// Ends the event, keying it by the query invocation rather than by the event id it
    /// started with. Records nothing.
    #[inline]
    pub fn finish_with_query_invocation_id(self, _query_invocation_id: QueryInvocationId) {}

    #[inline]
    pub fn none() -> TimingGuard<'a> {
        TimingGuard(PhantomData)
    }

    #[inline(always)]
    pub fn run<R>(self, f: impl FnOnce() -> R) -> R {
        let _timer = self;
        f()
    }
}

struct VerboseInfo {
    start_time: Instant,
    start_rss: Option<usize>,
    message: String,
    format: TimePassesFormat,
}

#[must_use]
pub struct VerboseTimingGuard<'a> {
    info: Option<VerboseInfo>,
    _guard: TimingGuard<'a>,
}

impl<'a> VerboseTimingGuard<'a> {
    pub fn start(
        message_and_format: Option<(String, TimePassesFormat)>,
        _guard: TimingGuard<'a>,
    ) -> Self {
        VerboseTimingGuard {
            _guard,
            info: message_and_format.map(|(message, format)| VerboseInfo {
                start_time: Instant::now(),
                start_rss: get_resident_set_size(),
                message,
                format,
            }),
        }
    }

    #[inline(always)]
    pub fn run<R>(self, f: impl FnOnce() -> R) -> R {
        let _timer = self;
        f()
    }
}

impl Drop for VerboseTimingGuard<'_> {
    fn drop(&mut self) {
        if let Some(info) = &self.info {
            let end_rss = get_resident_set_size();
            let dur = info.start_time.elapsed();
            print_time_passes_entry(&info.message, dur, info.start_rss, end_rss, info.format);
        }
    }
}

struct JsonTimePassesEntry<'a> {
    pass: &'a str,
    time: f64,
    start_rss: Option<usize>,
    end_rss: Option<usize>,
}

impl Display for JsonTimePassesEntry<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self { pass: what, time, start_rss, end_rss } = self;
        write!(f, r#"{{"pass":"{what}","time":{time},"rss_start":"#).unwrap();
        match start_rss {
            Some(rss) => write!(f, "{rss}")?,
            None => write!(f, "null")?,
        }
        write!(f, r#","rss_end":"#)?;
        match end_rss {
            Some(rss) => write!(f, "{rss}")?,
            None => write!(f, "null")?,
        }
        write!(f, "}}")?;
        Ok(())
    }
}

pub fn print_time_passes_entry(
    what: &str,
    dur: Duration,
    start_rss: Option<usize>,
    end_rss: Option<usize>,
    format: TimePassesFormat,
) {
    match format {
        TimePassesFormat::Json => {
            let entry =
                JsonTimePassesEntry { pass: what, time: dur.as_secs_f64(), start_rss, end_rss };

            eko::eprintln!(r#"time: {entry}"#);
            return;
        }
        TimePassesFormat::Text => (),
    }

    // Print the pass if its duration is greater than 5 ms, or it changed the
    // measured RSS.
    let is_notable = || {
        if dur.as_millis() > 5 {
            return true;
        }

        if let (Some(start_rss), Some(end_rss)) = (start_rss, end_rss) {
            let change_rss = end_rss.abs_diff(start_rss);
            if change_rss > 0 {
                return true;
            }
        }

        false
    };
    if !is_notable() {
        return;
    }

    let rss_to_mb = |rss| libm::round(rss as f64 / 1_000_000.0) as usize;
    let rss_change_to_mb = |rss| libm::round(rss as f64 / 1_000_000.0) as i128;

    let mem_string = match (start_rss, end_rss) {
        (Some(start_rss), Some(end_rss)) => {
            let change_rss = end_rss as i128 - start_rss as i128;

            format!(
                "; rss: {:>4}MB -> {:>4}MB ({:>+5}MB)",
                rss_to_mb(start_rss),
                rss_to_mb(end_rss),
                rss_change_to_mb(change_rss),
            )
        }
        (Some(start_rss), None) => format!("; rss start: {:>4}MB", rss_to_mb(start_rss)),
        (None, Some(end_rss)) => format!("; rss end: {:>4}MB", rss_to_mb(end_rss)),
        (None, None) => String::new(),
    };

    eko::eprintln!("time: {:>7}{}\t{}", duration_to_secs_str(dur), mem_string, what);
}

// Hack up our own formatting for the duration to make it easier for scripts
// to parse (always use the same number of decimal places and the same unit).
pub fn duration_to_secs_str(dur: core::time::Duration) -> String {
    format!("{:.3}", dur.as_secs_f64())
}

// `get_thread_id` was here. Every event carried the id of the thread it was recorded on;
// with no events there is nobody left to ask. It read `eko::thread::current_id()`,
// the `pthread_self` handle, which is still there when a recorder needs it back.

// Memory reporting
cfg_select! {
    windows => {
        pub fn get_resident_set_size() -> Option<usize> {
            use windows::Win32::System::ProcessStatus::{
                K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
            };
            use windows::Win32::System::Threading::GetCurrentProcess;

            let mut pmc = PROCESS_MEMORY_COUNTERS::default();
            let pmc_size = size_of_val(&pmc);
            unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc_size as u32) }
                .ok()
                .ok()?;

            Some(pmc.WorkingSetSize)
        }
    }
    target_os = "macos" => {
        pub fn get_resident_set_size() -> Option<usize> {
            use core::mem;

            use libc::{PROC_PIDTASKINFO, c_int, c_void, getpid, proc_pidinfo, proc_taskinfo};
            const PROC_TASKINFO_SIZE: c_int = size_of::<proc_taskinfo>() as c_int;

            unsafe {
                let mut info: proc_taskinfo = mem::zeroed();
                let info_ptr = &mut info as *mut proc_taskinfo as *mut c_void;
                let pid = getpid() as c_int;
                let ret = proc_pidinfo(pid, PROC_PIDTASKINFO, 0, info_ptr, PROC_TASKINFO_SIZE);
                if ret == PROC_TASKINFO_SIZE { Some(info.pti_resident_size as usize) } else { None }
            }
        }
    }
    unix => {
        pub fn get_resident_set_size() -> Option<usize> {
            use libc::{_SC_PAGESIZE, sysconf};
            let field = 1;
            let contents = fs::read("/proc/self/statm").ok()?;
            let contents = String::from_utf8(contents).ok()?;
            let s = contents.split_whitespace().nth(field)?;
            let npages = s.parse::<usize>().ok()?;
            // SAFETY: `sysconf(_SC_PAGESIZE)` has no side effects and is safe to call.
            Some(npages * unsafe { sysconf(_SC_PAGESIZE) } as usize)
        }
    }
    _ => {
        pub fn get_resident_set_size() -> Option<usize> {
            None
        }
    }
}

#[cfg(test)]
mod tests;
