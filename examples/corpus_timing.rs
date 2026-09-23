//! Time the reading-only entry points over this crate's own source, one call at a time and
//! batched, and check the two agree.
//!
//! Run: `cargo run --release --features diagnostics --example corpus_timing -- [rss_calls]`.
//! With `rss_calls`, also runs `analyze_source` that many times and prints resident memory
//! every thousand calls, which should stay flat.

use std::time::Instant;

use frontend::frontend_facts::analyze_source;
use frontend::frontend_facts::diagnostics::{Options, diagnose_many, diagnose_with};
use frontend::frontend_facts::site::{site_at, site_at_many};
use frontend::frontend_facts::syntax::{Fragment, parses_as, parses_as_many};

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

fn time<R>(label: &str, calls: usize, f: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let out = f();
    let s = start.elapsed().as_secs_f64();
    println!("{label:<16} {calls:>6} calls {:>10.1} ms {:>9.1} us/call", s * 1e3, s * 1e6 / calls as f64);
    out
}

fn rss_kb() -> String {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    let files = corpus();
    let bytes: usize = files.iter().map(String::len).sum();
    println!("{} files, {bytes} bytes", files.len());

    let items: Vec<(&str, Fragment)> = files.iter().map(|f| (f.as_str(), Fragment::Items)).collect();
    let a = time("parses single", items.len(), || {
        items.iter().map(|&(s, k)| parses_as(s, k)).collect::<Vec<_>>()
    });
    let b = time("parses batch", items.len(), || parses_as_many(items.iter().copied()));
    assert!(a == b, "parses: batch differs from single calls");

    let opts = Options::default();
    let a = time("diagnose single", files.len(), || {
        files.iter().map(|s| diagnose_with(s, &opts)).collect::<Vec<_>>()
    });
    let b = time("diagnose batch", files.len(), || diagnose_many(files.iter().map(String::as_str), &opts));
    assert!(a == b, "diagnose: batch differs from single calls");

    let sites: Vec<(&str, u32)> = files
        .iter()
        .step_by(7)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut at = s.len() / 2;
            while !s.is_char_boundary(at) {
                at += 1;
            }
            (s.as_str(), at as u32)
        })
        .collect();
    let a = time("site_at single", sites.len(), || {
        sites.iter().map(|&(s, o)| site_at(s, o)).collect::<Vec<_>>()
    });
    let b = time("site_at batch", sites.len(), || site_at_many(sites.iter().copied()));
    assert!(a == b, "site_at: batch differs from single calls");

    if let Some(n) = std::env::args().nth(1).and_then(|n| n.parse::<usize>().ok()) {
        let source = "fn f(x: u32) -> u32 { x }\nfn g() -> u32 { f(1) }\n";
        println!("rss before: {} KB", rss_kb());
        time("analyze_source", n, || {
            for i in 0..n {
                let _ = analyze_source("c", source);
                if (i + 1) % 1000 == 0 {
                    println!("  {:>6} calls, rss {} KB", i + 1, rss_kb());
                }
            }
        });
    }
}
