//! `evaluate`: a call run through rustc's MIR interpreter, on `no_core` sources written here.
//!
//! Each source declares the few lang items it uses, so these run with no library anywhere. What
//! they hold: a value comes back rendered with its type and a step count, a recursive call runs,
//! an overflow and an index out of bounds are panics with a message (the compiler's description,
//! since no panic runtime is loaded), a foreign function is refused by name, a budget ends a
//! loop that never would, a source that does not compile is refused with its error, and a
//! compiler bug on the way is refused rather than unwinding out of `evaluate`. A `no_core` value
//! has no `Debug` to run, so these values are read by layout.
//!
//! The same questions against `std` (the strawberry count, a `Vec` sum, a `u8` sum past 255,
//! `unwrap` on `None`, a formatted panic message), and what only `std` has (printing, a
//! `HashSet`'s thread-local keys, rendering by the type's own `Debug`, a file refused), need
//! `std`'s chain read with every function's MIR. That reader is `tests/library_read.rs`'s, so
//! they are there, ignored unless `FRONTEND_RUST_SRC_ROOTS` names a `rust-src` tree.
//!
//! A `std` program, because the catcher needs `std`.

use frontend::frontend_facts::{Evaluation, Loaded, evaluate, evaluate_all};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// The lang items the sources below use. `no_core` itself is injected, as for every source
/// read with nothing loaded.
const LANG: &str = "\
#![feature(lang_items)]
#![allow(internal_features)]
#[lang = \"pointee_sized\"] pub trait PointeeSized {}
#[lang = \"meta_sized\"] pub trait MetaSized: PointeeSized {}
#[lang = \"sized\"] pub trait Sized: MetaSized {}
#[lang = \"copy\"] pub trait Copy {}
impl Copy for u8 {}
impl Copy for usize {}
impl Copy for bool {}
impl Copy for char {}
#[lang = \"add\"] pub trait Add<Rhs = Self> { type Output; fn add(self, rhs: Rhs) -> Self::Output; }
impl Add for u8 { type Output = u8; fn add(self, rhs: u8) -> u8 { self + rhs } }
#[lang = \"sub\"] pub trait Sub<Rhs = Self> { type Output; fn sub(self, rhs: Rhs) -> Self::Output; }
impl Sub for u8 { type Output = u8; fn sub(self, rhs: u8) -> u8 { self - rhs } }
";

fn run(items: &str, call: &str, budget: Option<u64>) -> Evaluation {
    frontend::unwind_janky::install_catcher(catcher);
    evaluate(&format!("{LANG}{items}"), Some("2021"), Loaded::default(), call, budget)
}

#[test]
fn a_call_returns_its_value_rendered_with_its_type() {
    let items =
        "pub fn pair(a: u8, b: u8) -> (u8, bool, char, [u8; 2]) { (a + b, true, 'r', [a, b]) }\n";
    match run(items, "pair(2, 3)", None) {
        Evaluation::Value { rendered, ty, steps } => {
            assert_eq!(rendered, "(5, true, 'r', [2, 3])");
            assert_eq!(ty, "(u8, bool, char, [u8; 2])");
            assert!(steps > 0);
        }
        other => panic!("{other:?}"),
    }
}

/// Calls, recursion and a `match` on an integer: 10 + 9 + ... + 1.
#[test]
fn a_recursive_function_runs() {
    let items = "pub fn sum_to(n: u8) -> u8 { match n { 0 => 0, _ => n + sum_to(n - 1) } }\n";
    match run(items, "sum_to(10)", None) {
        Evaluation::Value { rendered, ty, .. } => {
            assert_eq!(rendered, "55");
            assert_eq!(ty, "u8");
        }
        other => panic!("{other:?}"),
    }
}

/// Overflow checks are on: 200 + 100 in a `u8` panics rather than wrapping to 44.
#[test]
fn an_overflow_is_a_panic() {
    match run("pub fn add(a: u8, b: u8) -> u8 { a + b }\n", "add(200, 100)", None) {
        Evaluation::Panicked { message } => assert!(message.contains("overflow"), "{message}"),
        other => panic!("{other:?}"),
    }
}

/// Bounds-checked indexing under `no_core`. The `Index` fixture this needs trips a delayed bug in
/// type checking (`DelayedBugPanic`) before anything is evaluated, so it is ignored; the same
/// panic is proved on real `std` by `an_evaluation_runs_std_on_rustcs_interpreter` in
/// `tests/library_read.rs` ("index out of bounds: the len is 3 but the index is 5").
#[test]
#[ignore = "the no_core Index fixture trips a delayed typeck bug; covered on std in tests/library_read.rs"]
fn an_index_out_of_bounds_is_a_panic() {
    let items = "#[lang = \"legacy_receiver\"] pub trait LegacyReceiver {}\n\
                 impl<T: ?Sized> LegacyReceiver for &T {}\n\
                 #[lang = \"index\"] pub trait Index<Idx> { type Output: ?Sized; fn index(&self, index: Idx) -> &Self::Output; }\n\
                 impl<T> Index<usize> for [T] { type Output = T; fn index(&self, index: usize) -> &T { &self[index] } }\n\
                 pub fn at(i: usize) -> u8 { let a: [u8; 3] = [1, 2, 3]; a[i] }\n";
    match run(items, "at(5)", None) {
        Evaluation::Panicked { message } => {
            assert!(message.contains("index out of bounds"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    match run(items, "at(2)", None) {
        Evaluation::Value { rendered, .. } => assert_eq!(rendered, "3"),
        other => panic!("{other:?}"),
    }
}

/// Nothing unwinds out of `evaluate`. The `Index` fixture above trips a delayed bug in type
/// checking, which rustc raises as a panic when the session ends; `evaluate` returns it as a
/// refusal that says what it was, rather than letting the panic reach the caller.
#[test]
fn a_compiler_bug_is_refused_not_a_panic() {
    let items = "#[lang = \"legacy_receiver\"] pub trait LegacyReceiver {}\n\
                 impl<T: ?Sized> LegacyReceiver for &T {}\n\
                 #[lang = \"index\"] pub trait Index<Idx> { type Output: ?Sized; fn index(&self, index: Idx) -> &Self::Output; }\n\
                 impl<T> Index<usize> for [T] { type Output = T; fn index(&self, index: usize) -> &T { &self[index] } }\n\
                 pub fn at(i: usize) -> u8 { let a: [u8; 3] = [1, 2, 3]; a[i] }\n";
    match run(items, "at(5)", None) {
        Evaluation::Refused { why } => assert!(!why.is_empty()),
        other => panic!("{other:?}"),
    }
}

/// No foreign function runs: the call is refused, and the refusal names it.
#[test]
fn a_foreign_call_is_refused_by_name() {
    let items = "unsafe extern \"C\" { fn getpid() -> i32; }\n\
                 pub fn pid() -> i32 { unsafe { getpid() } }\n";
    match run(items, "pid()", None) {
        Evaluation::Refused { why } => assert!(why.contains("getpid"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// The caller's budget ends a run that would never end on its own, after exactly that many steps.
#[test]
fn a_budget_ends_a_loop_that_never_would() {
    match run("pub fn spin() -> u8 { loop {} }\n", "spin()", Some(1000)) {
        Evaluation::Exhausted { steps } => assert_eq!(steps, 1000),
        other => panic!("{other:?}"),
    }
}

/// Only a program rustc accepts is run; one it does not is refused with rustc's error.
#[test]
fn a_source_that_does_not_compile_is_refused_with_its_error() {
    match run("", "missing(1)", None) {
        Evaluation::Refused { why } => assert!(why.contains("E0425"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// The outcome serializes with its kind named, for callers that take it as JSON.
#[test]
fn an_evaluation_serializes_tagged_with_its_outcome() {
    let value = Evaluation::Value { rendered: "3".into(), ty: "usize".into(), steps: 7 };
    let json = serde_json::to_string(&value).unwrap();
    assert_eq!(json, r#"{"outcome":"value","rendered":"3","ty":"usize","steps":7}"#);
    let back: Evaluation = serde_json::from_str(&json).unwrap();
    assert_eq!(back, value);
}

/// Several calls over one source share a session, and each answer is the one `evaluate` gives
/// that call alone: a value, a panic, and a call that does not compile, whose error is its own
/// and refuses no other call. The calls that build are answered in a second session without it.
#[test]
fn calls_sharing_a_session_are_each_answered_as_alone() {
    frontend::unwind_janky::install_catcher(catcher);
    let source = format!("{LANG}pub fn add(a: u8, b: u8) -> u8 {{ a + b }}\n");
    let calls = ["add(2, 3)", "add(200, 100)", "missing(1)", "add(1, 1)"];
    let all = evaluate_all(&source, Some("2021"), Loaded::default(), &calls, None);
    assert_eq!(all.evaluations.len(), calls.len());
    for (call, shared) in calls.iter().zip(&all.evaluations) {
        let alone = evaluate(&source, Some("2021"), Loaded::default(), call, None);
        match (shared, &alone) {
            (Evaluation::Refused { why }, Evaluation::Refused { why: alone_why }) => {
                assert!(why.contains("E0425") && alone_why.contains("E0425"), "{why}")
            }
            _ => assert_eq!(shared, &alone, "{call}"),
        }
    }
    assert!(matches!(&all.evaluations[0], Evaluation::Value { rendered, .. } if rendered == "5"));
    assert!(matches!(&all.evaluations[1], Evaluation::Panicked { .. }));
    assert!(matches!(&all.evaluations[3], Evaluation::Value { rendered, .. } if rendered == "2"));
    assert_eq!(all.sessions, 2, "one to find the call that does not build, one for the rest");
}

/// A source that does not compile refuses every call with its error, in one session.
#[test]
fn a_source_that_does_not_compile_refuses_every_call_sharing_it() {
    frontend::unwind_janky::install_catcher(catcher);
    let source = format!("{LANG}pub fn broken() -> u8 {{ nothing }}\n");
    let calls = ["broken()", "1u8"];
    let all = evaluate_all(&source, Some("2021"), Loaded::default(), &calls, None);
    for evaluation in &all.evaluations {
        match evaluation {
            Evaluation::Refused { why } => assert!(why.contains("E0425"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(all.sessions, 1);
}

/// Every call that builds is answered in one session, and a budget is each call's own.
#[test]
fn calls_that_build_share_one_session_and_each_has_its_budget() {
    frontend::unwind_janky::install_catcher(catcher);
    let source = format!("{LANG}pub fn spin() -> u8 {{ loop {{}} }}\npub fn one() -> u8 {{ 1 }}\n");
    let calls = ["spin()", "one()", "spin()"];
    let all = evaluate_all(&source, Some("2021"), Loaded::default(), &calls, Some(1000));
    assert!(matches!(all.evaluations[0], Evaluation::Exhausted { steps: 1000 }));
    assert!(matches!(&all.evaluations[1], Evaluation::Value { rendered, .. } if rendered == "1"));
    assert!(matches!(all.evaluations[2], Evaluation::Exhausted { steps: 1000 }));
    assert_eq!(all.sessions, 1);
}
