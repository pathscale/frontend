//! How much of a parse-only call is session setup, measured rather than guessed.
//!
//! Bounded by work, never by time: a fixed set of inputs, a fixed number of passes. Four arms,
//! each run as its own pass in a fixed order:
//!
//! - `fresh`: every call builds its own session globals, which is what `parses_as` does when the
//!   thread holds none.
//! - `fresh-again`: the same arm a second time. Its distance from `fresh` is the noise floor, and
//!   no difference between the other arms smaller than it means anything.
//! - `shared`: one set of globals wraps every call of the pass, so each call pays only for its
//!   parse session and the parse.
//! - `files`: whole source files instead of fragments, fresh and shared, to show how the share of
//!   setup falls as inputs grow.
//!
//! Run: `cargo run --release --example parse_setup_cost -- <passes>`.

use std::time::Instant;

use frontend::frontend_facts::syntax::{Fragment, parses, parses_as};
use frontend::rustc_span::{create_session_globals_then, edition::Edition};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// Small inputs of the size an editor or a checker asks about one at a time.
const FRAGMENTS: &[(&str, Fragment)] = &[
    ("s.chars().filter(|c| \"aeiou\".contains(*c)).count()", Fragment::Expr),
    ("s.chars().rev().collect()", Fragment::Expr),
    ("n >= 2 && (2..n).all(|d| n % d != 0)", Fragment::Expr),
    ("{ let mut v = Vec::new(); v.push(1); v }", Fragment::Block),
    ("let x: u64 = a.wrapping_add(b);", Fragment::Stmt),
    ("fn f(s: &str) -> usize { s.len() }", Fragment::Item),
    ("Result<i64, std::num::ParseIntError>", Fragment::Type),
    ("Some((a, b)) | None", Fragment::Pat),
    ("1 +", Fragment::Expr),
    ("fn f() {}", Fragment::Expr),
];

/// Calls per fragment per pass, so a pass is a thousand calls.
const REPEAT: usize = 100;

fn fragments_pass() -> usize {
    let mut ok = 0;
    for _ in 0..REPEAT {
        for &(source, kind) in FRAGMENTS {
            ok += usize::from(parses_as(source, kind).is_ok());
        }
    }
    ok
}

fn files_pass(files: &[String]) -> usize {
    files.iter().filter(|text| parses(text).is_ok()).count()
}

fn shared<R>(f: impl FnOnce() -> R) -> R {
    create_session_globals_then(Edition::Edition2024, &[], None, f)
}

fn time<R>(label: &str, calls: usize, f: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    println!(
        "{label:<14} {calls:>6} calls  {:>9.3} ms  {:>8.2} us/call",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e6 / calls as f64
    );
    out
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    let passes: usize = std::env::args().nth(1).and_then(|n| n.parse().ok()).unwrap_or(5);

    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/frontend_facts");
    let mut files: Vec<String> = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir).expect("read_dir").flatten().collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(std::fs::read_to_string(&path).expect("read"));
            }
        }
    }
    let bytes: usize = files.iter().map(String::len).sum();
    println!(
        "{} fragments x {REPEAT}, {} files ({bytes} bytes), {passes} passes",
        FRAGMENTS.len(),
        files.len()
    );

    let calls = FRAGMENTS.len() * REPEAT;
    for pass in 0..passes {
        println!("pass {pass}");
        let a = time("fresh", calls, fragments_pass);
        let b = time("fresh-again", calls, fragments_pass);
        let c = time("shared", calls, || shared(fragments_pass));
        assert!(a == b && b == c, "the arms must agree on every answer: {a} {b} {c}");
        let d = time("files-fresh", files.len(), || files_pass(&files));
        let e = time("files-shared", files.len(), || shared(|| files_pass(&files)));
        assert_eq!(d, e, "the file arms must agree");
    }
}
