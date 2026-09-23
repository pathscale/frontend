//! The batch entry points, checked against the single calls they batch.
//!
//! A batch shares session globals across calls, which is its whole saving. It must never change
//! an answer, so every test here computes each input's answer twice: once through the single
//! call on a thread that holds no globals, which is the reference, and once through the batch.
//! The two must be equal input by input, `Err` text included.
//!
//! Bounded by work: fixed inputs, fixed repeat counts. Some batches are longer than
//! `RECYCLE_EVERY` and some run through `run_batch` with a small `every`, so the answers are
//! also compared across a recycle boundary, where one set of globals is dropped and the next is
//! built.
//!
//! A `std` program, like `tests/syntax.rs`, because the catcher needs `std`.

use frontend::frontend_facts::session::{RECYCLE_EVERY, run_batch, with_session};
use frontend::frontend_facts::site::{site_at, site_at_many};
use frontend::frontend_facts::syntax::{Fragment, parses_as, parses_as_many};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// Install the catcher. Every test calls it, because the harness runs them in any order and on
/// any thread; installing twice is harmless.
fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

/// The fragments of `examples/parse_setup_cost.rs`, plus more refusals and every kind.
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
    ("1 + 2 3", Fragment::Expr),
    ("fn f() {} fn g() {}", Fragment::Item),
    ("let x = 1", Fragment::Stmt),
    ("fn f( {", Fragment::Items),
    ("", Fragment::Items),
    ("#!/usr/bin/env run\nfn main() {}", Fragment::Items),
    ("Vec<Option<u8>", Fragment::Type),
    ("x if y", Fragment::Pat),
    ("async move { a.await }", Fragment::Expr),
    ("unsafe impl Send for S {}", Fragment::Item),
    ("{ 1 }", Fragment::Block),
    ("struct S { a: u8, }", Fragment::Item),
];

/// Whole files, read at build time from this crate, so the batch also sees inputs large enough
/// to intern long spans and many symbols.
const FILES: &[&str] = &[
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/frontend_facts/syntax.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/frontend_facts/session.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/frontend_facts/site.rs")),
];

/// Small sources for `site_at` and `diagnose`: clean, recovered from, and refused.
const SOURCES: &[&str] = &[
    "fn main() { let x = 1; let y = x + 2; println!(\"{y}\"); }\n",
    "struct S { a: u8 }\nimpl S {\n    fn get(&self, k: u32) -> u8 { self.a }\n}\n",
    "mod m { pub fn f<T: Copy>(t: T) -> T where T: Clone { t } }\nuse m::f;\n",
    "fn f() { let x = ; }\n",
    "fn f( {\n",
    "fn f() -> i32 { return 1; }\nfn g() { loop { break; } }\nfn h() { break; }\n",
    "\u{feff}fn crlf() {\r\n    let a = 1;\r\n}\r\n",
    "trait T { fn a(&self); }\nimpl T for () {}\n",
];

/// `count` byte offsets spread evenly over `source`, each moved back onto a character
/// boundary, plus both ends and one past the end, which must be refused the same way both times.
fn offsets(source: &str, count: usize) -> Vec<u32> {
    let len = source.len();
    let mut out: Vec<u32> = (0..count)
        .map(|i| {
            let mut at = len * i / count.max(1);
            while !source.is_char_boundary(at) {
                at -= 1;
            }
            at as u32
        })
        .collect();
    out.push(len as u32);
    out.push(len as u32 + 1);
    out
}

fn site_inputs(sources: &[&'static str], per_source: usize) -> Vec<(&'static str, u32)> {
    sources
        .iter()
        .flat_map(|&source| offsets(source, per_source).into_iter().map(move |at| (source, at)))
        .collect()
}

#[test]
fn fragments_match_the_single_call_past_the_default_recycle() {
    ready();
    // Enough repeats that the batch crosses `RECYCLE_EVERY` at least once.
    let repeat = RECYCLE_EVERY / FRAGMENTS.len() + 2;
    let inputs: Vec<(&str, Fragment)> =
        (0..repeat).flat_map(|_| FRAGMENTS.iter().copied()).collect();
    assert!(inputs.len() > RECYCLE_EVERY);
    let single: Vec<_> = inputs.iter().map(|&(s, k)| parses_as(s, k)).collect();
    assert_eq!(parses_as_many(inputs.iter().copied()), single);
}

#[test]
fn whole_files_match_the_single_call() {
    ready();
    let inputs: Vec<(&str, Fragment)> =
        FILES.iter().map(|&s| (s, Fragment::Items)).chain(FRAGMENTS.iter().copied()).collect();
    let single: Vec<_> = inputs.iter().map(|&(s, k)| parses_as(s, k)).collect();
    assert!(single[..FILES.len()].iter().all(Result::is_ok), "the crate's own files parse");
    assert_eq!(parses_as_many(inputs.iter().copied()), single);
}

#[test]
fn a_small_recycle_interval_changes_nothing() {
    ready();
    let inputs: Vec<(&str, Fragment)> = FRAGMENTS.iter().copied().collect();
    let single: Vec<_> = inputs.iter().map(|&(s, k)| parses_as(s, k)).collect();
    // 3 does not divide the count, so the last set runs a short chunk; 1 is a set per call;
    // 0 is one set for the whole batch.
    for every in [3, 1, 0, inputs.len()] {
        let batched = run_batch(inputs.iter().copied(), every, |(s, k)| parses_as(s, k));
        assert_eq!(batched, single, "every = {every}");
    }
}

#[test]
fn run_batch_keeps_order_and_count() {
    // No parser involved: the driver alone, including the empty batch and a batch that ends
    // exactly on a boundary.
    for n in [0usize, 1, 2, 3, 6, 7] {
        for every in [0usize, 1, 3] {
            let out = run_batch(0..n, every, |i| i * 10);
            assert_eq!(out, (0..n).map(|i| i * 10).collect::<Vec<_>>(), "n = {n}, every = {every}");
        }
    }
}

#[test]
fn nested_sessions_reuse_the_outer_one() {
    ready();
    let single: Vec<_> = FRAGMENTS.iter().map(|&(s, k)| parses_as(s, k)).collect();
    let nested = with_session(|| with_session(|| parses_as_many(FRAGMENTS.iter().copied())));
    assert_eq!(nested, single);
}

#[test]
fn sites_match_the_single_call() {
    ready();
    let mut inputs = site_inputs(FILES, 12);
    inputs.extend(site_inputs(SOURCES, 6));
    let single: Vec<_> = inputs.iter().map(|&(s, at)| site_at(s, at)).collect();
    assert_eq!(site_at_many(inputs.iter().copied()), single);
    let batched = run_batch(inputs.iter().copied(), 3, |(s, at)| site_at(s, at));
    assert_eq!(batched, single, "every = 3");
}

#[test]
fn sites_match_the_single_call_past_the_default_recycle() {
    ready();
    let small = site_inputs(SOURCES, 6);
    let repeat = RECYCLE_EVERY / small.len() + 2;
    let inputs: Vec<(&str, u32)> = (0..repeat).flat_map(|_| small.iter().copied()).collect();
    assert!(inputs.len() > RECYCLE_EVERY);
    let single: Vec<_> = inputs.iter().map(|&(s, at)| site_at(s, at)).collect();
    assert_eq!(site_at_many(inputs.iter().copied()), single);
}

#[cfg(feature = "diagnostics")]
mod diagnostics {
    use super::*;
    use frontend::frontend_facts::diagnostics::{Options, Severity, diagnose_many, diagnose_with};

    fn all_options() -> Vec<Options> {
        vec![
            Options::default(),
            Options { codes: None, min_severity: Severity::Error },
            Options { codes: Some(vec!["E0425".to_string()]), min_severity: Severity::WeakWarning },
        ]
    }

    #[test]
    fn diagnostics_match_the_single_call() {
        ready();
        let inputs: Vec<&str> = FILES.iter().chain(SOURCES).copied().collect();
        for opts in all_options() {
            let single: Vec<_> = inputs.iter().map(|s| diagnose_with(s, &opts)).collect();
            assert_eq!(diagnose_many(inputs.iter().copied(), &opts), single, "{opts:?}");
            let batched = run_batch(inputs.iter().copied(), 3, |s| diagnose_with(s, &opts));
            assert_eq!(batched, single, "every = 3, {opts:?}");
        }
    }

    #[test]
    fn diagnostics_match_the_single_call_past_the_default_recycle() {
        ready();
        let repeat = RECYCLE_EVERY / SOURCES.len() + 2;
        let inputs: Vec<&str> = (0..repeat).flat_map(|_| SOURCES.iter().copied()).collect();
        assert!(inputs.len() > RECYCLE_EVERY);
        let opts = Options::default();
        let single: Vec<_> = inputs.iter().map(|s| diagnose_with(s, &opts)).collect();
        assert_eq!(diagnose_many(inputs.iter().copied(), &opts), single);
    }
}
