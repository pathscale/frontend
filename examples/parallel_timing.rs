//! Time `check_source` and `analyze_source` over this crate's own source at several
//! parallelism settings, and check every setting gives the one-worker answer.
//!
//! Run: `cargo run --release --features parallel --example parallel_timing`.
//!
//! Simple on purpose: one pass per setting over every `src/**/*.rs` file, total wall time, and
//! the speedup against one worker from the same run. Each setting runs on a fresh thread,
//! because a thread keeps the worker registry its first session sized. Without the `parallel`
//! feature every setting is the serial path and the speedups are noise around 1.0.

use std::time::Instant;

use frontend::frontend_facts::{CrateFacts, Checked, analyze_source, check_source, set_parallelism};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn corpus() -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(std::fs::read_to_string(&path).expect("read"));
            }
        }
    }
    files
}

/// One pass over `files` at `threads` workers: the answers, and milliseconds per entry point.
fn pass(threads: usize, files: &[String]) -> ((Vec<Checked>, Vec<Option<CrateFacts>>), f64, f64) {
    let files = files.to_vec();
    let thread = std::thread::Builder::new().stack_size(64 << 20).spawn(move || {
        frontend::unwind_janky::install_catcher(catcher);
        set_parallelism(threads);
        let start = Instant::now();
        let checked: Vec<Checked> = files.iter().map(|s| check_source("corpus", s)).collect();
        let check_ms = start.elapsed().as_secs_f64() * 1e3;
        let start = Instant::now();
        let facts: Vec<Option<CrateFacts>> =
            files.iter().map(|s| analyze_source("corpus", s).ok()).collect();
        let analyze_ms = start.elapsed().as_secs_f64() * 1e3;
        ((checked, facts), check_ms, analyze_ms)
    });
    thread.expect("spawn").join().expect("a setting panicked")
}

fn main() {
    let files = corpus();
    let bytes: usize = files.iter().map(String::len).sum();
    println!("{} files, {bytes} bytes", files.len());
    let (reference, check_one, analyze_one) = pass(1, &files);
    println!("{:>7} {:>12} {:>8} {:>12} {:>8}", "workers", "check ms", "x", "analyze ms", "x");
    println!("{:>7} {check_one:>12.1} {:>8.2} {analyze_one:>12.1} {:>8.2}", 1, 1.0, 1.0);
    for threads in [2, 4, 8, 12] {
        let (answers, check_ms, analyze_ms) = pass(threads, &files);
        if answers != reference {
            let (checked, facts) = (&answers.0, &answers.1);
            for (i, (a, b)) in reference.0.iter().zip(checked).enumerate() {
                if a != b { eprintln!("check differs at file {i}:\n  one: {a:?}\n  {threads}: {b:?}"); break; }
            }
            for (i, (a, b)) in reference.1.iter().zip(facts).enumerate() {
                if a != b { eprintln!("analyze differs at file {i}"); break; }
            }
            panic!("{threads} workers gave a different answer than one");
        }
        let (check_x, analyze_x) = (check_one / check_ms, analyze_one / analyze_ms);
        let (c, a) = (check_ms, analyze_ms);
        println!("{threads:>7} {c:>12.1} {check_x:>8.2} {a:>12.1} {analyze_x:>8.2}");
    }
}
