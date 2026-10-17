//! Parse-only throughput, repeated: the median and best of many rounds per workload, to compare
//! two builds of the parser without one noisy pass deciding the answer.
//!
//! Run: `cargo run --release --example parse_bench -- [ROUNDS] [DIR...]`.
//!
//! Workloads: the `clean` and `large` corpora `parallel_timing` generates (the same generator,
//! seeds and templates), and every `.rs` file under each `DIR` given. Files that do not parse
//! are left out of a directory workload and counted, so both builds read the same bytes.
//! `ONLY=clean|large|dirs` runs one of them.
//! `WIDTH=N` sets the session width (default 1); files are visited serially.
//! The counting allocator delegates to mimalloc.
//!
//! Each round parses every file of the workload once with `frontend_facts::syntax::parses`, on
//! this thread. One untimed round comes first, and one more, untimed, counts allocations.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static INNER: mimalloc::MiMalloc = mimalloc::MiMalloc;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        }
        unsafe { INNER.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { INNER.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(new_size as u64, Relaxed);
        }
        unsafe { INNER.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

const PRELUDE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/prelude.rs"));

const UNITS: &[&str] = &[
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_geometry.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_ledger.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_machine.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_numeric.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_cells.rs")),
];

/// `parallel_timing`'s generator, unchanged, so the corpora are byte for byte the same.
struct Lcg(u64);

impl Lcg {
    fn between(&mut self, low: usize, high: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        low + (self.0 >> 33) as usize % (high - low + 1)
    }
}

fn generated(
    name: char,
    count: usize,
    min_lines: usize,
    max_lines: usize,
    seed: u64,
) -> Vec<String> {
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

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn parse_all(files: &[String]) {
    for f in files {
        let _ = std::hint::black_box(frontend::frontend_facts::syntax::parses(f));
    }
}

fn bench(label: &str, files: &[String], rounds: usize) {
    let bytes: usize = files.iter().map(String::len).sum();
    let mb = bytes as f64 / 1e6;
    parse_all(files);
    let mut ms: Vec<f64> = (0..rounds)
        .map(|_| {
            let start = Instant::now();
            parse_all(files);
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ALLOCS.store(0, Relaxed);
    ALLOC_BYTES.store(0, Relaxed);
    COUNTING.store(true, Relaxed);
    parse_all(files);
    COUNTING.store(false, Relaxed);
    let median = ms[ms.len() / 2];
    println!(
        "{label:<6} {:>4} files {mb:>6.2} MB  median {median:>8.2} ms {:>6.1} MB/s  best {:>8.2} ms {:>6.1} MB/s  {:>9} allocs {:>7.1} MB alloc",
        files.len(),
        mb * 1e3 / median,
        ms[0],
        mb * 1e3 / ms[0],
        ALLOCS.load(Relaxed),
        ALLOC_BYTES.load(Relaxed) as f64 / 1e6,
    );
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    let width = std::env::var("WIDTH")
        .ok()
        .map(|value| value.parse::<usize>().expect("WIDTH must be a positive integer"))
        .unwrap_or(1);
    assert!(width > 0, "WIDTH must be a positive integer");
    let _session_width = frontend::rustc_data_structures::sync::enter_session_width(width);
    let mut args = std::env::args().skip(1);
    let rounds: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(15);
    let dirs: Vec<String> = args.collect();
    let only = std::env::var("ONLY").ok();
    let wants = |name: &str| only.as_deref().is_none_or(|o| o == name);
    println!("parse benchmark width {width}, parallel feature: {}", cfg!(feature = "parallel"));
    if wants("clean") {
        bench("clean", &generated('c', 200, 300, 1_500, 0x5eed_0001), rounds);
    }
    if wants("large") {
        bench("large", &generated('l', 6, 5_000, 8_000, 0x5eed_0002), rounds);
    }
    if wants("dirs") {
        for dir in dirs {
            let mut paths = Vec::new();
            rust_files(std::path::Path::new(&dir), &mut paths);
            let mut refused = 0;
            let files: Vec<String> = paths
                .iter()
                .filter_map(|p| std::fs::read_to_string(p).ok())
                .filter(|text| {
                    let ok = frontend::frontend_facts::syntax::parses(text).is_ok();
                    refused += usize::from(!ok);
                    ok
                })
                .collect();
            println!("dir {dir}: {refused} files refused and left out");
            bench("dir", &files, rounds);
        }
    }
}
