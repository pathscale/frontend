//! The driver answers exactly what the single calls answer, and a job that fails does not take
//! a worker with it. Small and bounded: a few dozen inputs of each kind at four workers.

use frontend::frontend_facts::analyze_source;
use frontend::frontend_facts::diagnostics::{Options, diagnose_with};
use frontend::frontend_facts::site::site_at;
use frontend::frontend_facts::syntax::{Fragment, parses_as};
use speed_fanout::{AnalyzeError, Driver, Globals, install};

const WORKERS: usize = 4;

const SOURCE: &str = "\
fn is_vowel(c: char) -> bool { \"aeiou\".contains(c) }
fn count(s: &str) -> usize { let n = s.chars().filter(|c| is_vowel(*c)).count(); n }
struct Point { x: f64, y: f64 }
impl Point { fn norm(&self) -> f64 { (self.x * self.x + self.y * self.y).sqrt() } }
";

fn fragments() -> Vec<(String, Fragment)> {
    let base: &[(&str, Fragment)] = &[
        ("1 + 2", Fragment::Expr),
        ("1 +", Fragment::Expr),
        ("fn f() {}", Fragment::Expr),
        ("fn f() {}", Fragment::Item),
        ("{ let x = 1; x }", Fragment::Block),
        ("let x = 1;", Fragment::Stmt),
        ("Vec<Option<u8>>", Fragment::Type),
        ("Some(x) | None", Fragment::Pat),
        ("use a::b;\nstruct S;\nfn f() {}\n", Fragment::Items),
        ("fn f( {", Fragment::Items),
        ("1 + 2 3", Fragment::Expr),
        ("", Fragment::Items),
    ];
    // Three rounds with a distinct identifier each, so the shared interner sees new symbols.
    (0..3)
        .flat_map(move |round| {
            base.iter().map(move |&(source, kind)| (source.replace('x', &format!("x{round}")), kind))
        })
        .collect()
}

fn sources() -> Vec<String> {
    (0..30)
        .map(|i| match i % 5 {
            0 => SOURCE.to_string(),
            1 => format!("fn f{i}(a: u8) -> u8 {{ a }}\nfn g{i}() -> u8 {{ f{i}(1, 2) + missing }}\n"),
            2 => format!("fn f{i}( {{\n"),
            3 => format!("struct S{i};\nstruct S{i};\n"),
            _ => format!("mod m{i} {{ pub fn h() {{ let unused = 1; }} }}\n"),
        })
        .collect()
}

#[test]
fn parses_as_many_matches_parses_as() {
    install();
    let inputs = fragments();
    let driver = Driver::new(WORKERS);
    let many = driver.parses_as_many(&inputs);
    let one: Vec<_> = inputs.iter().map(|(s, k)| parses_as(s, *k)).collect();
    assert_eq!(many, one);
}

#[test]
fn site_at_many_matches_site_at() {
    install();
    let inputs: Vec<(String, u32)> =
        (0..SOURCE.len() as u32).step_by(7).map(|offset| (SOURCE.to_string(), offset)).collect();
    assert!(inputs.len() >= 24);
    let driver = Driver::new(WORKERS);
    let many = driver.site_at_many(&inputs);
    let one: Vec<_> = inputs.iter().map(|(s, o)| site_at(s, *o)).collect();
    assert_eq!(many, one);
}

#[test]
fn diagnose_many_matches_diagnose_with() {
    install();
    let inputs = sources();
    let opts = Options::default();
    let driver = Driver::new(WORKERS);
    let many = driver.diagnose_many(&inputs, &opts);
    let one: Vec<_> = inputs.iter().map(|s| diagnose_with(s, &opts)).collect();
    assert_eq!(many, one);
}

#[test]
fn analyze_many_matches_analyze_source() {
    install();
    let inputs: Vec<(String, String)> =
        sources().into_iter().take(12).enumerate().map(|(i, s)| (format!("c{i}"), s)).collect();
    let driver = Driver::new(WORKERS);
    let many = driver.analyze_many(&inputs);
    let one: Vec<_> =
        inputs.iter().map(|(n, s)| analyze_source(n, s).map_err(AnalyzeError::from)).collect();
    assert_eq!(many, one);
}

/// A fatal refusal is an ordinary `Err`, and the inputs after it still get answers.
#[test]
fn a_fatal_input_does_not_stop_later_ones() {
    install();
    let mut inputs: Vec<(String, Fragment)> = vec![("fn f( {".to_string(), Fragment::Items); 8];
    inputs.extend((0..32).map(|i| (format!("{i} + 1"), Fragment::Expr)));
    let driver = Driver::new(WORKERS);
    let answers = driver.parses_as_many(&inputs);
    assert_eq!(answers.len(), inputs.len());
    assert!(answers[..8].iter().all(Result::is_err));
    assert!(answers[8..].iter().all(Result::is_ok), "{answers:?}");
}

/// A job that really panics is caught, reported as that job's error with its message, and
/// every worker is still there for the next batch.
#[test]
fn a_panicking_job_does_not_take_a_worker_down() {
    install();
    let driver = Driver::new(WORKERS);
    // More panics than workers, spread through the batch, under shared globals.
    let answers = driver.map((0..64u32).collect(), Globals::Shared, |&i| {
        if i % 8 == 3 {
            panic!("deliberate panic on input {i}");
        }
        parses_as(&format!("{i} * 2"), Fragment::Expr)
    });
    for (i, answer) in answers.iter().enumerate() {
        if i % 8 == 3 {
            let panicked = answer.as_ref().expect_err("a panicking input is an error");
            assert!(panicked.0.contains(&format!("deliberate panic on input {i}")), "{panicked:?}");
        } else {
            assert_eq!(answer, &Ok(Ok(())), "input {i}");
        }
    }
    // The same driver, afterwards: every worker must still be running for this to finish, and
    // with one chunk per worker at least, each of them gets some of it.
    let after = driver.map((0..64u32).collect(), Globals::None, |&i| i + 1);
    assert_eq!(after, (1..65u32).map(Ok).collect::<Vec<_>>());
}
