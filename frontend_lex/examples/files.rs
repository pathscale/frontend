//! Full lexing of many files at once: each file is one `par_for` piece on a nagoya pool, lexed
//! to its finished `LexedFile` against one shared interner. Without the main crate.
//!
//! Run: `cargo run --release -p frontend_lex --example files -- [ROUNDS] DIR...`. Pools of
//! `LEX_WORKERS` (default `1,2,4`) workers. For each width:
//! - throughput: median of `ROUNDS` (default 9) passes over every file, in MB/s;
//! - latency: each file's own lex time, p50 / p99 / max, over the same passes;
//! - memory, from a counting allocator over one pass that keeps every `LexedFile`: the peak of
//!   live heap during the pass, the bytes allocated, and the bytes the finished files retain,
//!   each per MB of source.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use frontend_lex::{LexedFile, Options};
use frontend_symbol::Interner;
use frontend_symbol::edition::Edition;

/// The system allocator, counting live, peak live and total allocated bytes.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Relaxed) + by;
    TOTAL.fetch_add(by, Relaxed);
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: forwards every call to `System` unchanged; the counters are side effects only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            grew(new_size - layout.size());
        } else {
            LIVE.fetch_sub(layout.size() - new_size, Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let rounds: usize = match args.first().and_then(|n| n.parse().ok()) {
        Some(n) => {
            args.remove(0);
            n
        }
        None => 9,
    };
    let widths: Vec<usize> = std::env::var("LEX_WORKERS")
        .ok()
        .map(|v| v.split(',').filter_map(|n| n.parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 2, 4]);
    let options =
        Options { edition: Edition::Edition2021, strip_shebang: false, frontmatter: false };
    let interner: &'static Interner = Box::leak(Box::new(Interner::with_extra_symbols(&[])));
    for dir in &args {
        let mut paths = Vec::new();
        files(Path::new(dir), &mut paths);
        paths.sort();
        let sources: Arc<Vec<Arc<String>>> = Arc::new(
            paths.iter().filter_map(|p| std::fs::read_to_string(p).ok()).map(Arc::new).collect(),
        );
        let n = sources.len();
        let bytes: usize = sources.iter().map(|s| s.len()).sum();
        let mb = bytes as f64 / 1e6;
        let mut sizes: Vec<usize> = sources.iter().map(|s| s.len()).collect();
        sizes.sort_unstable();
        println!(
            "{dir}: {n} files, {mb:.2} MB; file size p50 {:.1} KB, p99 {:.1} KB, max {:.1} KB",
            sizes[n / 2] as f64 / 1e3,
            sizes[(n - 1) * 99 / 100] as f64 / 1e3,
            sizes[n - 1] as f64 / 1e3
        );
        let mut base_rate = 0.0;
        for &workers in &widths {
            let runtime = nagoya::runtime::Runtime::builder().workers(workers).label("lex").build();
            let pool = runtime.pool();
            // One pass: every file lexed as its own piece; per-file times recorded, and the
            // files kept if `keep`.
            let pass = |keep: bool| -> (Vec<f64>, Vec<Arc<LexedFile>>) {
                let times: Arc<Vec<Mutex<f64>>> =
                    Arc::new((0..n).map(|_| Mutex::new(0.0)).collect());
                let kept: Arc<Vec<Mutex<Option<Arc<LexedFile>>>>> =
                    Arc::new((0..n).map(|_| Mutex::new(None)).collect());
                let (s, t, k) = (Arc::clone(&sources), Arc::clone(&times), Arc::clone(&kept));
                nagoya::block_on(nagoya::par_for(Arc::clone(pool), 0..n, move |i| {
                    let start = Instant::now();
                    let file = frontend_lex::lex(Arc::clone(&s[i]), options, interner);
                    *t[i].lock().unwrap() = start.elapsed().as_secs_f64() * 1e6;
                    if keep {
                        *k[i].lock().unwrap() = Some(file);
                    }
                }));
                let times = times.iter().map(|t| *t.lock().unwrap()).collect();
                let kept = kept.iter().filter_map(|k| k.lock().unwrap().take()).collect();
                (times, kept)
            };
            pass(false);
            let mut wall: Vec<f64> = Vec::new();
            let mut per_file: Vec<f64> = Vec::new();
            for _ in 0..rounds.max(1) {
                let start = Instant::now();
                let (times, _) = pass(false);
                wall.push(start.elapsed().as_secs_f64() * 1e3);
                per_file.extend(times);
            }
            wall.sort_by(f64::total_cmp);
            per_file.sort_by(f64::total_cmp);
            let rate = mb * 1e3 / wall[wall.len() / 2];
            if workers == widths[0] {
                base_rate = rate;
            }

            // Memory: one pass that keeps every file.
            let before = LIVE.load(Relaxed);
            PEAK.store(before, Relaxed);
            let total_before = TOTAL.load(Relaxed);
            let (_, kept) = pass(true);
            let peak = PEAK.load(Relaxed) - before;
            let allocated = TOTAL.load(Relaxed) - total_before;
            let retained = LIVE.load(Relaxed) - before;
            drop(kept);
            let per_mb = |b: usize| b as f64 / bytes as f64;

            println!(
                "  {workers} workers: {rate:>6.1} MB/s ({:.2}x) | per-file latency p50 {:.0} us, p99 {:.0} us, max {:.0} us | memory per source byte: peak {:.2}, allocated {:.2}, retained {:.2} (peak {:.1} MB)",
                rate / base_rate,
                percentile(&per_file, 0.5),
                percentile(&per_file, 0.99),
                per_file[per_file.len() - 1],
                per_mb(peak),
                per_mb(allocated),
                per_mb(retained),
                peak as f64 / 1e6,
            );
        }
    }
}
