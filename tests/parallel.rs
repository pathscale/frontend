//! Parallelism inside one analysis never changes an answer.
//!
//! [`set_parallelism`] lets one `analyze_source` or `check_source` call spread its own
//! independent work (per-definition facts, per-body type checks, the two syntax diagnostics
//! passes) over several workers. That is only worth having if the answer is the serial one, so
//! every test here computes each input's answer at one worker, which is the reference, and then
//! again at four and at eight, and requires the whole value to be equal: every definition,
//! reference, import and impl, `unanalyzed_bodies`, `complete`, and every diagnostic string in
//! its order.
//!
//! **Repeated, because a race does not lose every time.** Each parallel setting runs its whole
//! input list `RUNS` times, and every run must match the reference. Bounded by work: a fixed
//! list of inputs, a fixed run count, nothing timed.
//!
//! **One setting per fresh thread.** The setting is process-wide, and a thread keeps the
//! worker registry its first session sized, so each setting is applied and used on a thread
//! that has analysed nothing yet, and the tests take a lock so that no two settings overlap.
//! The threads are this test program's, not the library's: frontend spawns nothing.
//!
//! Without the `parallel` feature [`set_parallelism`] does nothing and every setting is the
//! serial path, so the comparisons hold trivially. They are still run, because they still
//! check that the same input gives the same answer twice.
//!
//! A `std` program, like `tests/batch.rs`, because the catcher needs `std`.

use std::sync::Mutex;

use frontend::frontend_facts::{CrateFacts, Checked, analyze_source, check_source, set_parallelism};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// Install the catcher. Installing twice is harmless.
fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

/// How many times each parallel setting runs the whole input list.
const RUNS: usize = 5;

/// The parallel settings, each held to the answer at one worker.
const SETTINGS: &[usize] = &[4, 8];

/// Only one setting in force at a time: the setting is process-wide and the harness runs tests
/// on several threads at once.
static ONE_SETTING_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The lang items a `no_core` crate declares so that ordinary bodies can type check.
const LANG: &str = "\
#![feature(lang_items)]
#[lang = \"pointee_sized\"] pub trait PointeeSized {}
#[lang = \"meta_sized\"] pub trait MetaSized: PointeeSized {}
#[lang = \"sized\"] pub trait Sized: MetaSized {}
#[lang = \"copy\"] pub trait Copy {}
#[lang = \"legacy_receiver\"] pub trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl Copy for u32 {}
impl Copy for bool {}
";

/// Many items, so there are many definitions and bodies to spread: free functions calling one
/// another, a trait with a default method, impls of it, inherent methods, an enum, and a
/// module with a `use`.
const MANY_ITEMS: &str = "
pub struct Point { pub x: u32, pub y: u32 }
pub struct Wrapper(pub u32);
pub enum Shape { Dot(Point), Line(Point, Point), Empty }
pub trait Measure {
    fn size(&self) -> u32;
    fn twice(&self) -> u32 { let a = self.size(); a }
}
impl Measure for Point { fn size(&self) -> u32 { self.x } }
impl Measure for Wrapper { fn size(&self) -> u32 { self.0 } }
impl Point {
    pub fn new(x: u32, y: u32) -> Point { Point { x, y } }
    pub fn origin() -> Point { Point::new(0, 0) }
    pub fn flip(&self) -> Point { Point::new(self.y, self.x) }
}
pub fn a(n: u32) -> u32 { n }
pub fn b(n: u32) -> u32 { a(n) }
pub fn c(n: u32) -> u32 { b(a(n)) }
pub fn d(n: u32) -> u32 { c(b(a(n))) }
pub fn e(p: &Point) -> u32 { p.size() }
pub fn f(p: &Point) -> u32 { p.twice() }
pub fn g() -> Point { Point::origin().flip() }
pub fn h(s: &Shape) -> bool { match s { Shape::Dot(_) => true, _ => false } }
pub mod inner {
    use super::Point;
    pub fn make() -> Point { Point::new(1, 2) }
    pub fn again() -> Point { make() }
    pub const LIMIT: u32 = 7;
    pub static FLAG: bool = true;
}
pub fn i() -> u32 { inner::LIMIT }
pub type Alias = Point;
";

/// Errors in several bodies at once: an unresolved name, a wrong argument count, a mismatched
/// type, a missing field. Each body's errors must come back in the same order at any setting.
const WITH_ERRORS: &str = "
pub struct S { pub a: u32 }
pub fn one() -> u32 { missing }
pub fn two(x: u32) -> u32 { two(x, x) }
pub fn three() -> bool { 3u32 }
pub fn four() -> S { S {} }
pub fn five() -> u32 { let s = S { a: 1 }; s.nope }
pub fn six() -> u32 { one() }
impl S { pub fn get(&self) -> u32 { self.b } }
pub trait T { fn m(&self); }
impl T for S {}
";

/// Sources that need no lang items to read, with bodies that stop on the missing ones, so
/// `unanalyzed_bodies` is long and its order matters.
const NO_LANG_ITEMS: &[&str] = &[
    "fn a() -> u32 { 1 } fn b() -> u32 { a() } fn c() -> u32 { b() }\nstruct P { x: u32 }\nimpl P { fn x(&self) -> u32 { self.x } }\n",
    "mod m { pub fn f() {} pub fn g() { f() } }\nuse m::f;\nfn h() { f(); m::g(); }\n",
    "trait A { fn a(&self) -> u32 { 0 } }\nstruct S;\nimpl A for S {}\nfn call(s: &S) -> u32 { s.a() }\n",
    "fn broken( {\n",
];

/// Real files from this crate: large, full of library paths the `no_core` session cannot see,
/// so they carry many errors and many bodies.
const FILES: &[&str] = &[
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/frontend_facts/syntax.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/frontend_facts/session.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/frontend_facts/site.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rustc_span/fatal_error.rs")),
];

/// Every input, in a fixed order.
fn inputs() -> Vec<String> {
    let mut all = vec![format!("{LANG}{MANY_ITEMS}"), format!("{LANG}{WITH_ERRORS}")];
    all.extend(NO_LANG_ITEMS.iter().map(|s| s.to_string()));
    all.extend(FILES.iter().map(|s| s.to_string()));
    all
}

/// What both entry points say about every input, at one setting.
#[derive(Debug, PartialEq)]
struct Answers {
    checked: Vec<Checked>,
    /// `FatalError` carries nothing and compares with nothing, so a refusal is `None`.
    facts: Vec<Option<CrateFacts>>,
}

/// Run `f` on a fresh thread that applies `threads` first. Large stack: rustc recurses deeply.
fn at_setting<R: Send + 'static>(threads: usize, f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            ready();
            set_parallelism(threads);
            f()
        })
        .expect("spawn")
        .join()
        .expect("a setting's thread panicked")
}

fn answers(threads: usize) -> Answers {
    at_setting(threads, || {
        let inputs = inputs();
        Answers {
            checked: inputs.iter().map(|s| check_source("parallel", s)).collect(),
            facts: inputs.iter().map(|s| analyze_source("parallel", s).ok()).collect(),
        }
    })
}

#[test]
fn every_setting_gives_the_one_worker_answer() {
    let _one = ONE_SETTING_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let reference = answers(1);
    // The fixtures must actually exercise the walks: definitions, references and diagnostics.
    let many = reference.facts[0].as_ref().expect("the many-items source has facts");
    assert!(many.definitions.len() > 20, "{} definitions", many.definitions.len());
    assert!(!reference.checked[1].errors.is_empty(), "the erroring source reports errors");
    for &threads in SETTINGS {
        for run in 0..RUNS {
            let got = answers(threads);
            for (index, (want, got)) in reference.checked.iter().zip(&got.checked).enumerate() {
                assert_eq!(want, got, "check_source, input {index}, {threads} workers, run {run}");
            }
            for (index, (want, got)) in reference.facts.iter().zip(&got.facts).enumerate() {
                let at = format!("input {index}, {threads} workers, run {run}");
                assert_eq!(want, got, "analyze_source, {at}");
            }
        }
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn the_diagnostics_passes_give_the_one_worker_answer() {
    use frontend::frontend_facts::diagnostics::{Options, diagnose_with};
    let _one = ONE_SETTING_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let run_all =
        || inputs().iter().map(|s| diagnose_with(s, &Options::default())).collect::<Vec<_>>();
    let reference = at_setting(1, run_all);
    for &threads in SETTINGS {
        for run in 0..RUNS {
            let got = at_setting(threads, run_all);
            assert_eq!(reference, got, "diagnose_with, {threads} workers, run {run}");
        }
    }
}
