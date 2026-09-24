//! Parse-only throughput of three Rust parsers over the same files: frontend's `parses`,
//! rust-analyzer's parser building a rowan tree (`ra_ap_syntax`), and `syn`. Wall clock,
//! MB/s, and allocations counted by this binary's global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size() as u64, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(n as u64, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn files(dir: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p) } else if p.extension().is_some_and(|x| x == "rs") { paths.push(p) }
        }
    }
    paths.sort();
    paths.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect()
}

fn time(name: &str, files: &[String], rounds: usize, mut f: impl FnMut(&str) -> bool) {
    let bytes: usize = files.iter().map(String::len).sum();
    // One warm-up round, not timed.
    let failed = files.iter().filter(|s| !f(s)).count();
    let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    let start = Instant::now();
    for _ in 0..rounds {
        for s in files {
            std::hint::black_box(f(s));
        }
    }
    let secs = start.elapsed().as_secs_f64() / rounds as f64;
    let allocs = (ALLOCS.load(Relaxed) - a0) / rounds as u64;
    let abytes = (BYTES.load(Relaxed) - b0) / rounds as u64;
    println!(
        "{name:<22} {:>8.1} ms {:>8.1} MB/s {:>11} allocs {:>7.1} allocs/KB {:>8.1} MB alloc  ({failed} files refused)",
        secs * 1e3,
        bytes as f64 / 1e6 / secs,
        allocs,
        allocs as f64 / (bytes as f64 / 1e3),
        abytes as f64 / 1e6,
    );
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    std::panic::set_hook(Box::new(|_| {}));
    let dir = std::env::args().nth(1).expect("a directory of .rs files");
    let rounds: usize = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(3);
    let files = files(&dir);
    let bytes: usize = files.iter().map(String::len).sum();
    println!("{} files, {:.1} MB, {rounds} rounds, single thread", files.len(), bytes as f64 / 1e6);
    time("frontend parses", &files, rounds, |s| frontend::frontend_facts::syntax::parses(s).is_ok());
    if std::env::var_os("ONLY_FRONTEND").is_some() { return; }
    time("rust-analyzer (rowan)", &files, rounds, |s| {
        ra_ap_syntax::SourceFile::parse(s, ra_ap_syntax::Edition::Edition2024).errors().is_empty()
    });
    time("syn", &files, rounds, |s| syn::parse_file(s).is_ok());
}
