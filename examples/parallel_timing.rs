//! Time `check_source` and `analyze_source` over two corpora at several parallelism settings,
//! and check every setting gives the width-one answer.
//!
//! Run: `cargo run --release --features parallel --example parallel_timing`.
//!
//! Simple on purpose: one pass per setting over every file of a corpus, total wall time, and the
//! speedup against width one from the same run. Without the `parallel` feature every width is
//! the serial path and the speedups are noise around 1.0.
//!
//! **The two corpora.** Both are valid programs: every file must type check with no error, or
//! the run stops. Timing invalid source times error recovery and the unwind out of a refused
//! run, not the compiler. (This crate's own `src/` was a corpus once; each of its files compiled
//! on its own, without its crate or `core`, is not a valid program, and 1,421 of 1,423 ended
//! fatal, so it measured nothing but that.)
//!
//! - `clean`: 200 generated files of 300 to 1,500 lines that type check with no error, so every
//!   stage runs over every body.
//! - `large`: 6 generated files of 5,000 to 8,000 lines, hundreds of functions each, because a
//!   speedup inside one file needs many bodies in that file.
//!
//! **How the clean files are made.** `tests/corpus/prelude.rs` is the lang item prelude (the
//! one `tests/parallel.rs` uses, plus the operator traits integer arithmetic needs), and each
//! `tests/corpus/unit_*.rs` is a block of items whose every name carries the tag `Q0`. A file is
//! the prelude followed by units, each copy with `Q0` replaced by a tag unique to its file and
//! position, so the copies do not collide and no two files are the same. Which unit comes next
//! and how long a file is are drawn from a fixed-seed generator, so every run builds the same
//! corpus. Nothing is written to disk.
//!
//! The width-one pass checks every file came back with no error; if one did not, it prints that
//! file's first errors and stops, because that is a defect in a unit template to fix, not
//! something to time around.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use frontend::frontend_facts::{
    CrateFacts, Checked, analyze_shared_source_with_width, check_shared_source_with_width,
};

/// Counts every allocation (and reallocation) this program makes, the compiler's included:
/// frontend declares no allocator, so this one serves it.
///
/// Only while `COUNTING` is set, and never during a timed pass: counting in the allocator is
/// work on every allocation of every worker, and a shared counter put them all on one cache
/// line (width 12 fell from 2.8x to 1.0x on the large corpus; sharded counters still cost a
/// third of the gain). Timed passes run with it off, where the allocator reads one flag nobody
/// writes, and the counts come from a separate, untimed pass at the same width.
struct Counting;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
/// Counted allocations by size class (`log2` of the size, 0 to 31), and how many of the
/// counted calls were reallocations: which of them growth up front would remove.
static BY_SIZE: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
static REALLOCS: AtomicU64 = AtomicU64::new(0);
fn size_class(size: usize) -> usize {
    (usize::BITS - size.max(1).leading_zeros() - 1).min(31) as usize
}
/// Group sampled backtraces by the allocating site: the nearest frame of this crate's own code
/// that is not collection or allocator machinery, by site and caller, and by source line.
fn print_sites(traces: &[std::backtrace::Backtrace], every: u64, allocs: u64) {
    let skip = |name: &str| {
        [
            "alloc::", "core::", "std::", "hashbrown", "indexmap", "smallvec", "thin_vec",
            "rustc_arena", "frontend_arena", "Counting", "record_site", "track_live",
            "parallel_timing", "__rust", "RawVec",
            "rustc_data_structures::sharded", "rustc_data_structures::fx", "ToOwned",
            "Clone", "clone", "FromIterator", "Extend", "collect", "backtrace",
        ]
        .iter()
        .any(|pat| name.contains(pat))
    };
    let mut by_site: std::collections::HashMap<String, u64> = Default::default();
    let mut by_pair: std::collections::HashMap<String, u64> = Default::default();
    let mut by_line: std::collections::HashMap<String, u64> = Default::default();
    for trace in traces {
        let text = format!("{trace}");
        // (function, the `at file:line` of the frame, if the trace has one), innermost first.
        let mut frames: Vec<(&str, &str)> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(at) = line.strip_prefix("at ") {
                if let Some(last) = frames.last_mut() {
                    if last.1.is_empty() {
                        last.1 = at;
                    }
                }
            } else if let Some((_, name)) = line.split_once(": ") {
                frames.push((name, ""));
            }
        }
        // The last allocator/collection frame's location is the line in the site that
        // allocated: the frame right below the site.
        // A site is a frame of this crate's own code: by location when the build has line
        // tables (`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`), else by name. Container
        // modules (index vectors, unification tables, arenas, sharded maps) are not sites.
        let own = |(name, at): &(&str, &str)| {
            if at.is_empty() {
                return !skip(name);
            }
            // std's frames sit under `/rustc/<commit>/library`, dependencies' under `.cargo`;
            // this crate's are relative (`./src/..`) or under its own directory.
            ![
                "/rustc/", ".cargo/", "/library/", "rustc_index/", "/ena/", "rustc_arena",
                "frontend_arena", "sharded.rs", "thin_vec", "smallvec", "examples/",
            ]
            .iter()
            .any(|pat| at.contains(pat))
        };
        let first_own = frames.iter().position(|frame| own(frame));
        let site = first_own.map_or("?", |i| frames[i].0);
        let site_at = first_own.map_or("", |i| frames[i].1);
        let caller = first_own
            .and_then(|i| frames[i + 1..].iter().find(|frame| own(frame)))
            .map_or("?", |f| f.0);
        let trim = |s: &str| s.rsplit_once("::h").map_or(s, |(a, _)| a).to_string();
        let short = |at: &str| at.rsplit_once("/src/").map_or(at, |(_, rest)| rest).to_string();
        *by_site.entry(trim(site)).or_default() += 1;
        *by_pair.entry(format!("{}  <-  {}", trim(site), trim(caller))).or_default() += 1;
        *by_line.entry(format!("{}  ({})", short(site_at), trim(site))).or_default() += 1;
    }
    let total = traces.len() as u64;
    println!("{allocs} allocations, {total} sampled (1 in {every})");
    for (title, map) in
        [("by site", by_site), ("by site and caller", by_pair), ("by line", by_line)]
    {
        println!("\n{title}:");
        let mut rows: Vec<_> = map.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        for (name, n) in rows.into_iter().take(80) {
            println!("{:>6.2}%  ~{:>9}  {name}", n as f64 * 100.0 / total as f64, n * every);
        }
    }
}

/// `mem-cap`: live bytes (allocated and not yet freed) while `MEMCAP` is set, their peak, and
/// every `OVER_EVERY`th allocation made while live bytes exceed `CAP` records its backtrace.
static MEMCAP: AtomicBool = AtomicBool::new(false);
static LIVE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static PEAK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static CAP: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MAX);
/// Live bytes when the current file's check began: the cap, the peak and the thresholds are
/// all over that, per file.
static BASE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// `mem-phases`: the next live-bytes level (over `BASE`) whose first crossing records a
/// backtrace, and which of the thresholds it is; `i64::MAX` when none is armed.
static NEXT_LEVEL: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MAX);
static LEVEL_TRACES: std::sync::Mutex<Vec<(usize, std::time::Instant, std::backtrace::Backtrace)>> =
    std::sync::Mutex::new(Vec::new());
static LEVELS: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());
static OVER: AtomicU64 = AtomicU64::new(0);
const OVER_EVERY: u64 = 256;
fn track_live(delta: i64) {
    if !MEMCAP.load(Relaxed) {
        return;
    }
    let live = LIVE.fetch_add(delta, Relaxed) + delta;
    if delta <= 0 {
        return;
    }
    PEAK.fetch_max(live, Relaxed);
    let over_base = live - BASE.load(Relaxed);
    if over_base >= NEXT_LEVEL.load(Relaxed) && !IN_SITE.with(|flag| flag.replace(true)) {
        // Re-check under the lock: another thread may have crossed and advanced it first.
        let mut levels = LEVELS.lock().unwrap();
        let armed = NEXT_LEVEL.load(Relaxed);
        if over_base >= armed {
            let index = levels.iter().position(|&l| l == armed).unwrap_or(0);
            let trace = std::backtrace::Backtrace::force_capture();
            LEVEL_TRACES.lock().unwrap().push((index, std::time::Instant::now(), trace));
            NEXT_LEVEL.store(levels.get(index + 1).copied().unwrap_or(i64::MAX), Relaxed);
        }
        drop(levels);
        IN_SITE.with(|flag| flag.set(false));
    }
    if over_base > CAP.load(Relaxed) {
        let n = OVER.fetch_add(1, Relaxed);
        if n % OVER_EVERY == 0 && !IN_SITE.with(|flag| flag.replace(true)) {
            let trace = std::backtrace::Backtrace::force_capture();
            SITE_TRACES.lock().unwrap().push(trace);
            IN_SITE.with(|flag| flag.set(false));
        }
    }
}

/// `alloc-sites`: every `SITE_EVERY`th counted allocation records its backtrace. Off unless set.
static SITES: AtomicBool = AtomicBool::new(false);
const SITE_EVERY: u64 = 1024;
static SITE_TRACES: std::sync::Mutex<Vec<std::backtrace::Backtrace>> = std::sync::Mutex::new(Vec::new());
thread_local! {
    /// Set while this thread records a backtrace, whose own allocations must not record.
    static IN_SITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn record_site(n: u64) {
    if !SITES.load(Relaxed) || n % SITE_EVERY != 0 || IN_SITE.with(|flag| flag.replace(true)) {
        return;
    }
    let trace = std::backtrace::Backtrace::force_capture();
    SITE_TRACES.lock().unwrap().push(trace);
    IN_SITE.with(|flag| flag.set(false));
}
/// `mem-held`: every allocation of `HELD_BIG` bytes or more, and every `HELD_EVERY`th smaller
/// one, made while `HELD` is set is remembered, with its weight in bytes and backtrace, until it
/// is freed; `HELD_SNAPSHOT` then says what the remembered live ones
/// were allocated by at one moment.
static HELD: AtomicBool = AtomicBool::new(false);
const HELD_EVERY: u64 = 128;
const HELD_BIG: usize = 4096;
static HELD_COUNT: AtomicU64 = AtomicU64::new(0);
static HELD_LIVE: std::sync::Mutex<Option<std::collections::HashMap<usize, (usize, std::backtrace::Backtrace)>>> =
    std::sync::Mutex::new(None);
/// Live bytes (over `BASE`) at which to take the `mem-held` snapshot; `i64::MAX` when none.
static HELD_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MAX);
static HELD_SNAPSHOT: std::sync::Mutex<Vec<(usize, String)>> = std::sync::Mutex::new(Vec::new());

fn held_alloc(ptr: *mut u8, size: usize) {
    if !HELD.load(Relaxed) || IN_SITE.with(|flag| flag.replace(true)) {
        return;
    }
    // Every allocation of `HELD_BIG` bytes or more is remembered at its own size, so the few
    // large tables and arenas are counted exactly; a smaller one stands for the `HELD_EVERY`
    // allocations it was sampled from.
    let sampled = HELD_COUNT.fetch_add(1, Relaxed) % HELD_EVERY == 0;
    if size >= HELD_BIG || sampled {
        let weight = if size >= HELD_BIG { size } else { size * HELD_EVERY as usize };
        let trace = std::backtrace::Backtrace::force_capture();
        if let Some(map) = HELD_LIVE.lock().unwrap().as_mut() {
            map.insert(ptr as usize, (weight, trace));
        }
    }
    // The snapshot, the first time this file's live bytes reach the armed level: every
    // remembered allocation still live, with its trace printed, weighted by its size.
    let over = LIVE.load(Relaxed) - BASE.load(Relaxed);
    if over >= HELD_AT.load(Relaxed) {
        HELD_AT.store(i64::MAX, Relaxed);
        let guard = HELD_LIVE.lock().unwrap();
        let rows: Vec<(usize, String)> = guard
            .as_ref()
            .map(|map| map.values().map(|(size, trace)| (*size, format!("{trace}"))).collect())
            .unwrap_or_default();
        drop(guard);
        HELD_SNAPSHOT.lock().unwrap().extend(rows);
    }
    IN_SITE.with(|flag| flag.set(false));
}

fn held_free(ptr: *mut u8) {
    if !HELD.load(Relaxed) || IN_SITE.with(|flag| flag.get()) {
        return;
    }
    IN_SITE.with(|flag| flag.set(true));
    if let Some(map) = HELD_LIVE.lock().unwrap().as_mut() {
        map.remove(&(ptr as usize));
    }
    IN_SITE.with(|flag| flag.set(false));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        track_live(layout.size() as i64);
        if COUNTING.load(Relaxed) {
            record_site(ALLOCS.load(Relaxed));
            ALLOCS.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
            BY_SIZE[size_class(layout.size())].fetch_add(1, Relaxed);
        }
        let ptr = unsafe { System.alloc(layout) };
        held_alloc(ptr, layout.size());
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        track_live(-(layout.size() as i64));
        held_free(ptr);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        track_live(size as i64 - layout.size() as i64);
        if COUNTING.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(size as u64, Relaxed);
            BY_SIZE[size_class(size)].fetch_add(1, Relaxed);
            REALLOCS.fetch_add(1, Relaxed);
        }
        held_free(ptr);
        let new = unsafe { System.realloc(ptr, layout, size) };
        held_alloc(new, size);
        new
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations and bytes allocated while `f` runs, counted; not for timing.
fn counted<R>(f: impl FnOnce() -> R) -> (R, u64, u64) {
    let (a, b) = (ALLOCS.load(Relaxed), ALLOC_BYTES.load(Relaxed));
    COUNTING.store(true, Relaxed);
    let r = f();
    COUNTING.store(false, Relaxed);
    (r, ALLOCS.load(Relaxed) - a, ALLOC_BYTES.load(Relaxed) - b)
}

/// The pass a printed backtrace is in: the innermost frame naming one.
fn pass_of(text: &str) -> &'static str {
    const PASSES: [(&str, &str); 26] = [
        ("rustc_borrowck::", "borrow check"),
        ("check_liveness", "liveness"),
        ("rustc_mir_build::", "MIR build"),
        ("rustc_mir_transform::", "MIR passes"),
        ("rustc_hir_typeck::", "type check (bodies)"),
        ("wfcheck", "well-formedness"),
        ("coherence", "coherence"),
        ("rustc_lint::", "lints"),
        ("rustc_privacy::", "privacy"),
        ("rustc_ast_lowering::", "lowering"),
        ("late_resolve", "late resolution"),
        ("rustc_resolve::", "resolution"),
        ("rustc_expand::", "expansion"),
        ("rustc_parse::", "parse"),
        ("frontend_facts::extract", "facts"),
        ("rustc_hir_analysis::", "hir analysis (other)"),
        ("drop_in_place", "teardown"),
        ("new_lint_store", "session: lint store"),
        ("register_lints", "session: lint store"),
        ("create_global_ctxt", "session: global context"),
        ("query_system", "session: query system"),
        ("Session::", "session"),
        ("build_session", "session"),
        ("rustc_session::", "session"),
        ("rustc_interface::", "interface (other)"),
        ("rustc_middle::", "middle (other)"),
    ];
    let frames: Vec<&str> =
        text.lines().map(str::trim).filter(|line| !line.starts_with("at ")).collect();
    // The specific passes first, over every frame, innermost first; the general
    // markers (session, interface, middle) only when no pass is on the stack, since a
    // query or an arena from `rustc_middle` is innermost in almost every trace.
    let (specific, general) = PASSES.split_at(17);
    for group in [specific, general] {
        for line in &frames {
            if let Some((_, pass)) = group.iter().find(|(pat, _)| line.contains(pat)) {
                return pass;
            }
        }
    }
    "other"
}

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// The settings timed. The first is the reference every other one is held to.
const SETTINGS: &[usize] = &[1, 2, 4, 8, 12, 16, 24];

const PRELUDE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/prelude.rs"));

const UNITS: &[&str] = &[
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_geometry.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_ledger.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_machine.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_numeric.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_cells.rs")),
];

/// A 64-bit linear congruential generator (Knuth's MMIX constants) with a fixed seed: the
/// corpus is the same in every run.
struct Lcg(u64);

impl Lcg {
    /// A number in `low..=high`.
    fn between(&mut self, low: usize, high: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        low + (self.0 >> 33) as usize % (high - low + 1)
    }
}

/// `count` files of `min_lines..=max_lines` lines each. `name` goes into every tag, so the
/// clean and large corpora do not share a file either.
fn generated(name: char, count: usize, min_lines: usize, max_lines: usize, seed: u64) -> Vec<String> {
    let mut rng = Lcg(seed);
    (0..count)
        .map(|file| {
            let target = rng.between(min_lines, max_lines);
            let mut text = format!("// Generated corpus file {name}{file}.\n{PRELUDE}");
            let mut unit = 0;
            while text.lines().count() < target {
                let template = UNITS[rng.between(0, UNITS.len() - 1)];
                text.push('\n');
                text.push_str(&template.replace("Q0", &format!("Q{name}{file}x{unit}")));
                unit += 1;
            }
            text
        })
        .collect()
}

/// Both entry points' answers for a whole corpus, at one setting.
type Answers = (Vec<Checked>, Vec<Option<CrateFacts>>);

/// One pass over `files` at stage width `width`: the answers, and milliseconds per entry point.
/// At width one, the check's allocations are left in `LAST_CHECK_ALLOCS`.
fn pass(width: usize, files: &[String]) -> (Answers, f64, f64) {
    pass_counting(width, files, true)
}

/// [`pass`], choosing whether width one also runs the untimed counted check. A single-width
/// run (`parallel_timing clean 1`) does not, so it does exactly the work a wider one does, for
/// comparing CPU time and profiles across widths.
fn pass_counting(width: usize, files: &[String], count: bool) -> (Answers, f64, f64) {
    frontend::unwind_janky::install_catcher(catcher);
    // Shared once, outside the timing: each call below hands its session the same text.
    let files: Vec<Arc<String>> = files.iter().cloned().map(Arc::new).collect();
    let check_all = || -> Vec<Checked> {
        files.iter().map(|s| check_shared_source_with_width("corpus", Arc::clone(s), width)).collect()
    };
    let start = Instant::now();
    let checked = check_all();
    let check_ms = start.elapsed().as_secs_f64() * 1e3;
    // Counted at width one only: the count barely moves with the width (by about 7%), and a
    // counted pass at a wider width makes every worker's allocations contend on the counters,
    // which is time spent, and profile samples taken, in this program rather than the compiler.
    if width == 1 && count {
        let (_, allocs, alloc_bytes) = counted(check_all);
        LAST_CHECK_ALLOCS.store(allocs, Relaxed);
        LAST_CHECK_ALLOC_BYTES.store(alloc_bytes, Relaxed);
    }
    let start = Instant::now();
    let facts: Vec<Option<CrateFacts>> = files
        .iter()
        .map(|s| analyze_shared_source_with_width("corpus", Arc::clone(s), None, width).ok())
        .collect();
    let analyze_ms = start.elapsed().as_secs_f64() * 1e3;
    ((checked, facts), check_ms, analyze_ms)
}

static LAST_CHECK_ALLOCS: AtomicU64 = AtomicU64::new(0);
static LAST_CHECK_ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

/// What the width-one check's allocations were: reallocations (growth), and counts by size.
fn allocation_classes() {
    let total: u64 = BY_SIZE.iter().map(|c| c.load(Relaxed)).sum();
    let reallocs = REALLOCS.load(Relaxed);
    let mut line = format!(
        "        check allocations at width 1: {total} ({:.0} MB), of which reallocations {reallocs} ({:.0}%); by size:",
        LAST_CHECK_ALLOC_BYTES.load(Relaxed) as f64 / 1e6,
        reallocs as f64 * 100.0 / total.max(1) as f64
    );
    for (class, count) in BY_SIZE.iter().enumerate() {
        let count = count.load(Relaxed);
        if count * 200 >= total {
            line.push_str(&format!(" <{}B {:.0}%", 1u64 << (class + 1), count as f64 * 100.0 / total as f64));
        }
    }
    println!("{line}");
}

/// Parse-only throughput of a corpus, one thread: `frontend_facts::syntax::parses` over every
/// file, after one untimed round.
fn parse_only(label: &str, files: &[String]) {
    frontend::unwind_janky::install_catcher(catcher);
    let bytes: usize = files.iter().map(String::len).sum();
    for f in files {
        assert!(frontend::frontend_facts::syntax::parses(f).is_ok(), "{label}: a file does not parse");
    }
    let parse_all = || {
        for f in files {
            let _ = std::hint::black_box(frontend::frontend_facts::syntax::parses(f));
        }
    };
    let start = Instant::now();
    parse_all();
    let secs = start.elapsed().as_secs_f64();
    let ((), allocs, alloc_bytes) = counted(parse_all);
    println!(
        "parse only: {:.1} ms, {:.1} MB/s, {allocs} allocations ({:.0} per KB), {:.0} MB allocated",
        secs * 1e3,
        bytes as f64 / 1e6 / secs,
        allocs as f64 / (bytes as f64 / 1e3),
        alloc_bytes as f64 / 1e6,
    );
}

/// Time one corpus at every setting, and hold each setting to the first one's answers.
fn run(label: &str, files: &[String]) {
    let bytes: usize = files.iter().map(String::len).sum();
    let lines: usize = files.iter().map(|f| f.lines().count()).sum();
    let mb = bytes as f64 / 1e6;
    println!("\n{label}: {} files, {lines} lines, {mb:.2} MB", files.len());
    parse_only(label, files);
    println!(
        "{:>7} {:>10} {:>8} {:>6} {:>10} {:>8} {:>6}",
        "width", "check ms", "MB/s", "x", "analyze ms", "MB/s", "x"
    );
    let row = |width: usize, check_ms: f64, analyze_ms: f64, check_x: f64, analyze_x: f64| {
        println!(
            "{width:>7} {check_ms:>10.1} {:>8.2} {check_x:>6.2} {analyze_ms:>10.1} {:>8.2} {analyze_x:>6.2}",
            mb / (check_ms / 1e3),
            mb / (analyze_ms / 1e3),
        );
    };
    let one = SETTINGS[0];
    for class in &BY_SIZE {
        class.store(0, Relaxed);
    }
    REALLOCS.store(0, Relaxed);
    let (want, check_one, analyze_one) = pass(one, files);
    row(one, check_one, analyze_one, 1.0, 1.0);
    allocation_classes();
    assert_clean(label, &want.0);
    for &width in &SETTINGS[1..] {
        let (answers, check_ms, analyze_ms) = pass(width, files);
        if answers != want {
            for (i, (a, b)) in want.0.iter().zip(&answers.0).enumerate() {
                if a != b {
                    eprintln!("{label}: check differs at file {i}:\n  one: {a:?}\n  {width}: {b:?}");
                    break;
                }
            }
            for (i, (a, b)) in want.1.iter().zip(&answers.1).enumerate() {
                if a != b {
                    eprintln!("{label}: analyze differs at file {i}:\n  one: {a:?}\n  {width}: {b:?}");
                    break;
                }
            }
            panic!("{label}: width {width} gave a different answer than width one");
        }
        row(width, check_ms, analyze_ms, check_one / check_ms, analyze_one / analyze_ms);
    }
}

/// Every file type checked with no error; if one did not, what it said, and stop.
fn assert_clean(label: &str, checked: &[Checked]) {
    if let Some((i, first)) = checked.iter().enumerate().find(|(_, c)| !c.is_clean()) {
        eprintln!("{label}: file {i} is not a valid program (fatal {}):", first.fatal);
        for error in first.errors.iter().take(5) {
            eprintln!("    {error}");
        }
        panic!("{label}: the corpus must be valid source");
    }
}

/// `parallel_timing [clean|large] [width]`: with a corpus named, only that corpus; with a
/// width as well, only that setting, once, with no comparison.
fn main() {
    // `--pool N` anywhere: run on a nagoya pool of `N` workers this program builds and hands to
    // frontend, instead of the default one, to see how the widths scale with the pool's size.
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    // `--tuning locality|park|tokio`: the pool's idle policy (fanout's `Tuning`): the default,
    // the default with no idle spin rounds before a worker parks, or `almost_tokio`.
    let tuning = raw.iter().position(|a| a == "--tuning").map(|at| {
        let name = raw.get(at + 1).cloned().expect("--tuning NAME");
        raw.drain(at..at + 2);
        match name.as_str() {
            "locality" => st3::fanout::Tuning::locality(),
            "park" => st3::fanout::Tuning::locality().with_rounds_before_park(0),
            "tokio" => st3::fanout::Tuning::almost_tokio(),
            other => panic!("unknown tuning {other}"),
        }
    });
    let pool = raw.iter().position(|a| a == "--pool").map(|at| {
        let workers: usize = raw.get(at + 1).and_then(|n| n.parse().ok()).expect("--pool N");
        raw.drain(at..at + 2);
        workers
    });
    if pool.is_some() || tuning.is_some() {
        let workers = pool.unwrap_or(12);
        let mut builder = nagoya::runtime::Runtime::builder();
        builder = builder.workers(workers).label("timing").stack_size(16 * 1024 * 1024);
        if let Some(tuning) = tuning {
            builder = builder.tuning(tuning);
        }
        let runtime = builder.build();
        let executor = runtime.executor().clone_handle();
        // Kept for the life of the program: dropping a `Runtime` does not stop its workers.
        std::mem::forget(runtime);
        frontend::rustc_data_structures::sync::set_parallel_executor(executor)
            .unwrap_or_else(|_| panic!("an executor was already set"));
        println!("pool: {workers} workers");
    }
    let mut args = raw.into_iter();
    let only = args.next();
    let width: Option<usize> = args.next().and_then(|n| n.parse().ok());
    let corpus = |name: &str| match name {
        "clean" => generated('c', 200, 300, 1_500, 0x5eed_0001),
        _ => generated('l', 6, 5_000, 8_000, 0x5eed_0002),
    };
    // `parallel_timing dump <clean|large> <file>`: every answer at width one, written out,
    // to diff one build's answers against another's.
    if only.as_deref() == Some("dump") {
        let name = std::env::args().nth(2).unwrap_or_else(|| "clean".into());
        let path = std::env::args().nth(3).expect("dump needs an output file");
        let ((checked, facts), _, _) = pass(1, &corpus(&name));
        std::fs::write(&path, format!("{checked:#?}\n{facts:#?}\n")).expect("write dump");
        return;
    }
    // `parallel_timing alloc-sites`: where the clean corpus's check allocates, at width one:
    // every 1,024th allocation's backtrace, grouped by the nearest compiler frame that is not
    // collection or allocator machinery, and by that frame's caller.
    // `alloc-sites parse` samples the parse alone (`syntax::parses`), the parse-only line's pass.
    if only.as_deref() == Some("alloc-sites") {
        frontend::unwind_janky::install_catcher(catcher);
        let files: Vec<Arc<String>> =
            corpus("clean").into_iter().map(Arc::new).collect();
        let parse = std::env::args().nth(2).as_deref() == Some("parse");
        SITES.store(true, Relaxed);
        let ((), allocs, _) = counted(|| {
            for f in &files {
                if parse {
                    let _ = frontend::frontend_facts::syntax::parses(f);
                } else {
                    let _ = check_shared_source_with_width("corpus", Arc::clone(f), 1);
                }
            }
        });
        SITES.store(false, Relaxed);
        let traces = std::mem::take(&mut *SITE_TRACES.lock().unwrap());
        print_sites(&traces, SITE_EVERY, allocs);
        return;
    }
    // `parallel_timing mem-cap [MB] [width]`: how much memory a check holds at once (live
    // bytes, and their peak per file), and with a cap (default 100 MB), which sites allocate while
    // live bytes are over it: where memory is tightest. Width one by default.
    if only.as_deref() == Some("mem-cap") {
        frontend::unwind_janky::install_catcher(catcher);
        let cap_mb: i64 = width.map_or(100, |w| w as i64);
        let run_width: usize = std::env::args().nth(3).and_then(|w| w.parse().ok()).unwrap_or(1);
        let files: Vec<Arc<String>> = corpus("clean").into_iter().map(Arc::new).collect();
        CAP.store(cap_mb * 1_000_000, Relaxed);
        let mut peaks = Vec::with_capacity(files.len());
        MEMCAP.store(true, Relaxed);
        let start = Instant::now();
        for f in &files {
            // Each file's peak, over what was live when it began.
            let base = LIVE.load(Relaxed);
            PEAK.store(base, Relaxed);
            let _ = check_shared_source_with_width("corpus", Arc::clone(f), run_width);
            peaks.push((PEAK.load(Relaxed) - base, f.len()));
        }
        let ms = start.elapsed().as_secs_f64() * 1e3;
        MEMCAP.store(false, Relaxed);
        let over = OVER.load(Relaxed);
        peaks.sort_by_key(|p| p.0);
        let mb = |b: i64| b as f64 / 1e6;
        let (max, max_src) = peaks[peaks.len() - 1];
        println!(
            "width {run_width}, cap {cap_mb} MB: {ms:.0} ms; peak live per file: median {:.1} MB, max {:.1} MB (a {:.0} KB file), {:.0}x its source; {over} allocations made over the cap",
            mb(peaks[peaks.len() / 2].0),
            mb(max),
            max_src as f64 / 1e3,
            max as f64 / max_src as f64,
        );
        let traces = std::mem::take(&mut *SITE_TRACES.lock().unwrap());
        if !traces.is_empty() {
            print_sites(&traces, OVER_EVERY, over);
        }
        return;
    }
    // `parallel_timing mem-held [width]`: what a check is holding when its live memory reaches
    // 95% of its peak, by the pass that allocated it and by allocating site, weighted by bytes:
    // what stays live to the end, and so what freeing each body's data earlier would release.
    // Every tenth file of the clean corpus; one run for the peaks, one for the snapshot.
    if only.as_deref() == Some("mem-held") {
        frontend::unwind_janky::install_catcher(catcher);
        let run_width = width.unwrap_or(1);
        let files: Vec<Arc<String>> =
            corpus("clean").into_iter().step_by(10).map(Arc::new).collect();
        MEMCAP.store(true, Relaxed);
        let mut peaks = Vec::with_capacity(files.len());
        for f in &files {
            let base = LIVE.load(Relaxed);
            BASE.store(base, Relaxed);
            PEAK.store(base, Relaxed);
            let _ = check_shared_source_with_width("corpus", Arc::clone(f), run_width);
            peaks.push(PEAK.load(Relaxed) - base);
        }
        *HELD_LIVE.lock().unwrap() = Some(Default::default());
        HELD.store(true, Relaxed);
        for (f, &peak) in files.iter().zip(&peaks) {
            BASE.store(LIVE.load(Relaxed), Relaxed);
            HELD_AT.store(peak * 95 / 100, Relaxed);
            let _ = check_shared_source_with_width("corpus", Arc::clone(f), run_width);
            HELD_AT.store(i64::MAX, Relaxed);
            if let Some(map) = HELD_LIVE.lock().unwrap().as_mut() {
                map.clear();
            }
        }
        HELD.store(false, Relaxed);
        MEMCAP.store(false, Relaxed);
        let rows = std::mem::take(&mut *HELD_SNAPSHOT.lock().unwrap());
        let total: usize = rows.iter().map(|r| r.0).sum();
        let site_of = |text: &str| -> String {
            let skip = [
                "alloc::", "core::", "std::", "hashbrown", "indexmap", "smallvec", "thin_vec",
                "rustc_arena", "frontend_arena", "parallel_timing", "__rust", "RawVec",
                "backtrace", "held_alloc", "Counting", "rustc_index::", "sharded", "fx::",
                "Clone", "clone", "FromIterator", "Extend", "collect", "arena::Arena",
                "rustc_middle::arena", "ToOwned", "_malloc", "<unknown>",
            ];
            text.lines()
                .map(str::trim)
                .filter(|line| !line.starts_with("at "))
                .filter_map(|line| line.split_once(": ").map(|(_, name)| name))
                .find(|name| name.contains("frontend") && !skip.iter().any(|pat| name.contains(pat)))
                .map(|name| name.rsplit_once("::h").map_or(name, |(a, _)| a).to_string())
                .unwrap_or_else(|| "?".to_string())
        };
        let mut by_pass: std::collections::HashMap<&str, usize> = Default::default();
        let mut by_site: std::collections::HashMap<String, usize> = Default::default();
        for (size, text) in &rows {
            *by_pass.entry(pass_of(text)).or_default() += size;
            *by_site.entry(site_of(text)).or_default() += size;
        }
        let mut sorted = peaks.clone();
        sorted.sort();
        println!(
            "width {run_width}: {} files, peak live per file median {:.1} MB; held at 95% of the peak, by allocating pass and site ({} remembered allocations: all of {HELD_BIG} bytes or more, 1 in {HELD_EVERY} of the rest; {:.1} MB estimated over the files):",
            files.len(),
            sorted[sorted.len() / 2] as f64 / 1e6,
            rows.len(),
            total as f64 / 1e6,
        );
        for (title, map) in [
            ("by pass", by_pass.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<Vec<_>>()),
            ("by site", by_site.into_iter().collect()),
        ] {
            let mut map = map;
            map.sort_by(|a, b| b.1.cmp(&a.1));
            println!("\n{title}:");
            for (name, bytes) in map.into_iter().take(30) {
                println!("{:>6.2}%  {name}", bytes as f64 * 100.0 / total.max(1) as f64);
            }
        }
        // `mem-held WIDTH SITE`: the callers behind every held allocation whose site contains
        // SITE, as the chain of this crate's frames above it, weighted the same way.
        if let Some(filter) = std::env::args().nth(3) {
            let mut chains: std::collections::HashMap<String, usize> = Default::default();
            for (size, text) in &rows {
                if !site_of(text).contains(&filter) {
                    continue;
                }
                let chain: Vec<String> = text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.starts_with("at "))
                    .filter_map(|line| line.split_once(": ").map(|(_, name)| name))
                    .filter(|name| name.contains("frontend::rustc_") && !name.contains("sharded"))
                    .take(6)
                    .map(|name| {
                        let name = name.rsplit_once("::h").map_or(name, |(a, _)| a);
                        name.rsplit("::").take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("::")
                    })
                    .collect();
                *chains.entry(chain.join(" <- ")).or_default() += size;
            }
            let mut chains: Vec<_> = chains.into_iter().collect();
            chains.sort_by(|a, b| b.1.cmp(&a.1));
            println!("\ncallers of {filter}:");
            for (chain, bytes) in chains.into_iter().take(15) {
                println!("{:>6.2}%  {chain}", bytes as f64 * 100.0 / total.max(1) as f64);
            }
        }
        return;
    }
    // `parallel_timing mem-phases [width]`: what runs while each file's live memory climbs to
    // its peak. A first run records every file's peak; an identical second run records a
    // backtrace the first time live bytes reach 25%, 50%, 75% and 95% of it, and names the pass
    // running then. The peak is where many things are in flight at once; what builds memory up
    // before it is where to look for serial work.
    if only.as_deref() == Some("mem-phases") {
        frontend::unwind_janky::install_catcher(catcher);
        let run_width = width.unwrap_or(1);
        let files: Vec<Arc<String>> = corpus("clean").into_iter().map(Arc::new).collect();
        MEMCAP.store(true, Relaxed);
        let mut peaks = Vec::with_capacity(files.len());
        for f in &files {
            let base = LIVE.load(Relaxed);
            BASE.store(base, Relaxed);
            PEAK.store(base, Relaxed);
            let _ = check_shared_source_with_width("corpus", Arc::clone(f), run_width);
            peaks.push(PEAK.load(Relaxed) - base);
        }
        const FRACTIONS: [f64; 4] = [0.25, 0.50, 0.75, 0.95];
        let mut starts = Vec::with_capacity(files.len());
        let mut ends = Vec::with_capacity(files.len());
        for (f, &peak) in files.iter().zip(&peaks) {
            let levels: Vec<i64> = FRACTIONS.iter().map(|x| (peak as f64 * x) as i64).collect();
            BASE.store(LIVE.load(Relaxed), Relaxed);
            NEXT_LEVEL.store(levels[0], Relaxed);
            *LEVELS.lock().unwrap() = levels;
            let first = LEVEL_TRACES.lock().unwrap().len();
            let start = Instant::now();
            let _ = check_shared_source_with_width("corpus", Arc::clone(f), run_width);
            NEXT_LEVEL.store(i64::MAX, Relaxed);
            starts.push((first, start));
            ends.push(start.elapsed().as_secs_f64() * 1e3);
        }
        MEMCAP.store(false, Relaxed);
        let traces = std::mem::take(&mut *LEVEL_TRACES.lock().unwrap());
        let phase_of = |trace: &std::backtrace::Backtrace| pass_of(&format!("{trace}"));
        let mut table: Vec<std::collections::BTreeMap<&str, (u64, f64)>> =
            (0..FRACTIONS.len()).map(|_| Default::default()).collect();
        for (file, (first, start)) in starts.iter().enumerate() {
            let last = starts.get(file + 1).map_or(traces.len(), |next| next.0);
            for (level, at, trace) in &traces[*first..last] {
                let ms = at.duration_since(*start).as_secs_f64() * 1e3;
                let entry = table[*level].entry(phase_of(trace)).or_default();
                entry.0 += 1;
                entry.1 += ms / ends[file];
            }
        }
        // For the traces no pass names, the innermost driver step on the stack (a
        // `rustc_interface` or `frontend_facts` frame), at the first threshold.
        let mut unnamed: std::collections::BTreeMap<String, u64> = Default::default();
        for (level, _, trace) in &traces {
            if *level != 0 || !phase_of(trace).contains("other") {
                continue;
            }
            let text = format!("{trace}");
            let step = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.starts_with("at "))
                .find(|line| line.contains("rustc_interface::") || line.contains("frontend_facts::"))
                .map(|line| line.split_once(": ").map_or(line, |(_, name)| name))
                .map(|name| name.rsplit_once("::h").map_or(name, |(a, _)| a).to_string())
                .unwrap_or_else(|| "?".to_string());
            *unnamed.entry(step).or_default() += 1;
        }
        let mut sorted = peaks.clone();
        sorted.sort();
        println!(
            "width {run_width}: {} files, peak live per file median {:.1} MB; which pass is running when live memory first reaches each share of the file's peak (and how far into the file's check, on average):",
            files.len(),
            sorted[sorted.len() / 2] as f64 / 1e6
        );
        for (i, fraction) in FRACTIONS.iter().enumerate() {
            let mut rows: Vec<_> = table[i].iter().collect();
            rows.sort_by(|a, b| b.1.0.cmp(&a.1.0));
            let n: u64 = rows.iter().map(|r| r.1.0).sum();
            let parts: Vec<String> = rows
                .iter()
                .take(5)
                .map(|(pass, (count, at))| {
                    format!("{pass} {:.0}% (at {:.0}%)", *count as f64 * 100.0 / n.max(1) as f64, at * 100.0 / *count as f64)
                })
                .collect();
            println!("  {:>3.0}% of peak: {}", fraction * 100.0, parts.join(", "));
        }
        let mut unnamed: Vec<_> = unnamed.into_iter().collect();
        unnamed.sort_by(|a, b| b.1.cmp(&a.1));
        for (step, n) in unnamed.into_iter().take(6) {
            println!("    unnamed at 25%: {n} under {step}");
        }
        return;
    }
    // `parallel_timing overhead`: what one session costs with nothing in it (an empty file,
    // `no_core`), and with the corpus prelude alone, at widths one and twelve: the fixed cost
    // every session pays, and what the prelude every generated file repeats adds to it.
    if only.as_deref() == Some("overhead") {
        frontend::unwind_janky::install_catcher(catcher);
        const RUNS: usize = 1000;
        for (what, file) in [
            ("an empty file", Arc::new(String::new())),
            ("the prelude alone", Arc::new(format!("// Session overhead probe.\n{PRELUDE}"))),
        ]
        .iter()
        .flat_map(|probe| [(1usize, probe.clone()), (12, probe.clone())])
        .map(|(width, (what, file))| ((what, width), file))
        {
            let (what, width) = what;
            let _ = check_shared_source_with_width("probe", Arc::clone(&file), width);
            let start = Instant::now();
            for _ in 0..RUNS {
                std::hint::black_box(check_shared_source_with_width("probe", Arc::clone(&file), width));
            }
            let check_us = start.elapsed().as_secs_f64() * 1e6 / RUNS as f64;
            let ((), allocs, bytes) = counted(|| {
                let _ = check_shared_source_with_width("probe", Arc::clone(&file), width);
            });
            let start = Instant::now();
            for _ in 0..RUNS {
                let _ = std::hint::black_box(frontend::frontend_facts::syntax::parses(&file));
            }
            let parse_us = start.elapsed().as_secs_f64() * 1e6 / RUNS as f64;
            println!(
                "width {width}: one check of {what} ({} bytes) {check_us:.0} us, {allocs} allocations, {:.1} MB allocated; parse only {parse_us:.0} us",
                file.len(),
                bytes as f64 / 1e6
            );
        }
        return;
    }
    match (only.as_deref(), width) {
        (Some(name), Some(width)) => {
            let (_, check_ms, analyze_ms) = pass_counting(width, &corpus(name), false);
            println!(
                "{name} at {width}: check {check_ms:.1} ms, analyze {analyze_ms:.1} ms"
            );
        }
        (Some(name), None) => run(name, &corpus(name)),
        (None, _) => {
            run("clean", &corpus("clean"));
            run("large", &corpus("large"));
        }
    }
}
