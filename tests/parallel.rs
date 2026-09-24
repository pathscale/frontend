//! A wider stage never changes an answer.
//!
//! `check_source_with_width` and `analyze_source_with_width` let up to `width` of one call's
//! independent stage items (per-definition facts, per-body type and borrow checks, lowering) run
//! at once on nagoya's pool. That is only worth having if the answer is the serial one, so each
//! input's answer is computed at width one, the reference, and again at four and at eight, and
//! the whole value must be equal: every definition, reference, import and impl,
//! `unanalyzed_bodies`, `complete`, and every diagnostic string in its order.
//!
//! **Repeated, because a race does not lose every time.** Each width runs its whole input list
//! `RUNS` times, and every run must match the reference. Bounded by work: a fixed list of inputs,
//! a fixed run count, nothing timed.
//!
//! Without the `parallel` feature every width is the serial path, so the comparisons hold
//! trivially; they still check that the same input gives the same answer twice.
//!
//! A `std` program, because the catcher needs `std`.

use frontend::frontend_facts::{
    CrateFacts, Checked, analyze_source_with_width, check_source_with_width,
};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// How many times each width runs the whole input list.
const RUNS: usize = 5;

/// The widths held to the answer at width one.
const WIDTHS: &[usize] = &[4, 8];

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
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rustc_span/edit_distance.rs")),
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


/// What both entry points say about every input, at one width.
#[derive(Debug, PartialEq)]
struct Answers {
    checked: Vec<Checked>,
    /// `FatalError` carries nothing and compares with nothing, so a refusal is `None`.
    facts: Vec<Option<CrateFacts>>,
}

fn answers(width: usize) -> Answers {
    frontend::unwind_janky::install_catcher(catcher);
    let inputs = inputs();
    Answers {
        checked: inputs.iter().map(|s| check_source_with_width("parallel", s, width)).collect(),
        facts: inputs
            .iter()
            .map(|s| analyze_source_with_width("parallel", s, None, width).ok())
            .collect(),
    }
}

#[test]
fn every_width_gives_the_width_one_answer() {
    let reference = answers(1);
    // The fixtures must actually exercise the walks: definitions, references and diagnostics.
    let many = reference.facts[0].as_ref().expect("the many-items source has facts");
    assert!(many.definitions.len() > 20, "{} definitions", many.definitions.len());
    assert!(!reference.checked[1].errors.is_empty(), "the erroring source reports errors");
    for &width in WIDTHS {
        for run in 0..RUNS {
            let got = answers(width);
            for (index, (want, got)) in reference.checked.iter().zip(&got.checked).enumerate() {
                assert_eq!(want, got, "check_source, input {index}, width {width}, run {run}");
            }
            for (index, (want, got)) in reference.facts.iter().zip(&got.facts).enumerate() {
                assert_eq!(want, got, "analyze_source, input {index}, width {width}, run {run}");
            }
        }
    }
}
