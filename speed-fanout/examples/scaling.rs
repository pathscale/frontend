//! How the fanout driver scales, measured against a plain serial loop in the same run.
//!
//! **The machine is shared.** Other work runs beside this, so no number here is comparable with
//! a number from another run. Every parallel time is printed beside the serial arm and the
//! serial arm's own repeat from this run, and only that comparison means anything.
//!
//! **Bounded by work, not time.** A fixed corpus, a fixed number of passes from argv:
//!
//! - `fragments`: 1,000 fixed fragments through `parses_as`.
//! - `parses`: every `../src/**/*.rs` in sorted path order through `parses_as(_, Items)`.
//! - `diagnose`: the same files through `diagnose_with(_, &Options::default())`.
//! - `analyze`: 200 small fixed sources through `analyze_source`, clean and erroring, with a
//!   parse failure among them.
//!
//! Each kind runs these arms, each `passes` times:
//!
//! - `serial`: the single call in a plain loop on the main thread. No driver, no shared globals.
//! - `serial-again`: the same arm again. Its distance from `serial` is the noise floor, printed
//!   as a percentage, and a speedup inside it is not a speedup.
//! - `workers=N` for N in 1, 2, 4, 8, 12: the driver's batch call. The driver is started before
//!   the clock and joined after it, so thread start and join are not in the time. Copying the
//!   inputs into the batch is, which only makes the driver look slower.
//!
//! `speedup` is serial time over the arm's time. `vs w1` is the 1-worker time over the arm's,
//! which separates the gain from threads from the gain from shared globals. **Every arm's
//! answers must equal the serial arm's, input by input**, or the run stops: that is the
//! differential check, and a faster wrong answer is not a result.
//!
//! Run: `cargo run --release --example scaling -- <passes>` from `speed-fanout/`.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use frontend::frontend_facts::analyze_source;
use frontend::frontend_facts::diagnostics::{Options, diagnose_with};
use frontend::frontend_facts::syntax::{Fragment, parses_as};
use speed_fanout::{AnalyzeError, Driver, install};

const WORKERS: &[usize] = &[1, 2, 4, 8, 12];

/// Small inputs of the size an editor asks about one at a time. The same ten as
/// `examples/parse_setup_cost.rs` in frontend.
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

/// Copies of each fragment, so the fragment corpus is a thousand calls.
const FRAGMENT_REPEAT: usize = 100;

/// Sources for the analyze arm.
const ANALYZE_SOURCES: usize = 200;

fn main() {
    let passes: usize = match std::env::args().nth(1) {
        Some(arg) => arg.parse().expect("passes must be a positive integer"),
        None => 1,
    };
    assert!(passes > 0, "passes must be a positive integer");
    install();

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
    let files = corpus(&root);
    let bytes: usize = files.iter().map(|(_, text)| text.len()).sum();
    println!("# speed-fanout scaling");
    println!("# shared machine: compare arms within this run only");
    println!("# corpus: {} files, {} bytes, under {}", files.len(), bytes, root.display());
    println!("# frontend HEAD: {}", head(&root.join("..")));
    println!("# first file: {}", files.first().map_or("none".into(), |(p, _)| p.display().to_string()));
    println!("# last file: {}", files.last().map_or("none".into(), |(p, _)| p.display().to_string()));
    println!("# passes: {passes}; workers: {WORKERS:?}");
    println!();

    let fragments: Vec<(&str, Fragment)> =
        (0..FRAGMENT_REPEAT).flat_map(|_| FRAGMENTS.iter().copied()).collect();
    measure(
        "fragments",
        fragments.len(),
        passes,
        || fragments.iter().map(|&(source, kind)| parses_as(source, kind)).collect(),
        |driver| driver.parses_as_many(&fragments),
    );

    let texts: Vec<&str> = files.iter().map(|(_, text)| text.as_str()).collect();
    let items: Vec<(&str, Fragment)> = texts.iter().map(|&text| (text, Fragment::Items)).collect();
    measure(
        "parses",
        items.len(),
        passes,
        || items.iter().map(|&(source, kind)| parses_as(source, kind)).collect(),
        |driver| driver.parses_as_many(&items),
    );

    let opts = Options::default();
    measure(
        "diagnose",
        texts.len(),
        passes,
        || texts.iter().map(|text| diagnose_with(text, &opts)).collect(),
        |driver| driver.diagnose_many(&texts, &opts),
    );

    let sources = analyze_corpus();
    measure(
        "analyze",
        sources.len(),
        passes,
        || {
            sources
                .iter()
                .map(|(name, source)| analyze_source(name, source).map_err(AnalyzeError::from))
                .collect()
        },
        |driver| driver.analyze_many(&sources),
    );
}

/// Run one kind's arms and print them. Panics if any arm's answers differ from the serial arm's.
fn measure<X: PartialEq + Debug>(
    name: &str,
    calls: usize,
    passes: usize,
    serial: impl Fn() -> Vec<X>,
    parallel: impl Fn(&Driver) -> Vec<X>,
) {
    println!("{name}: {calls} calls x {passes} passes");
    let total = (calls * passes).max(1) as f64;

    let (first, reference) = run(passes, &serial);
    let (again, repeat) = run(passes, &serial);
    same(&reference, &repeat, name, "serial-again");
    let floor = percent_apart(first, again);
    line("serial", first, total, None);
    println!("  {:<14} {:>10.1} ms  floor {floor:.1}%", "serial-again", ms(again));

    let mut one = None;
    for &workers in WORKERS {
        let driver = Driver::new(workers);
        let (took, answers) = run(passes, &|| parallel(&driver));
        // Joined here, outside the clock, so no thread outlives its arm.
        drop(driver);
        same(&reference, &answers, name, &format!("workers={workers}"));
        let one_worker = *one.get_or_insert(took);
        line(
            &format!("workers={workers}"),
            took,
            total,
            Some((first, one_worker, floor)),
        );
    }
    println!();
}

/// Time `passes` calls of `f`, summing only the calls, and return the first pass's answers.
/// Every later pass must answer the same.
fn run<X: PartialEq + Debug>(passes: usize, f: &dyn Fn() -> Vec<X>) -> (Duration, Vec<X>) {
    let start = Instant::now();
    let first = f();
    let mut took = start.elapsed();
    for _ in 1..passes {
        let start = Instant::now();
        let again = f();
        took += start.elapsed();
        same(&first, &again, "a repeated pass", "same arm");
    }
    (took, first)
}

fn line(label: &str, took: Duration, total: f64, against: Option<(Duration, Duration, f64)>) {
    let per_call = took.as_secs_f64() * 1e6 / total;
    match against {
        None => println!("  {label:<14} {:>10.1} ms  {per_call:>9.1} us/call", ms(took)),
        Some((serial, one, floor)) => println!(
            "  {label:<14} {:>10.1} ms  {per_call:>9.1} us/call  speedup x{:.2}  vs w1 x{:.2}  (floor {floor:.1}%)",
            ms(took),
            serial.as_secs_f64() / took.as_secs_f64(),
            one.as_secs_f64() / took.as_secs_f64(),
        ),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn percent_apart(a: Duration, b: Duration) -> f64 {
    let (a, b) = (a.as_secs_f64(), b.as_secs_f64());
    if a == 0.0 { 0.0 } else { (a - b).abs() / a * 100.0 }
}

/// The differential check: `got` answers every input exactly as `reference` does.
fn same<X: PartialEq + Debug>(reference: &[X], got: &[X], kind: &str, arm: &str) {
    assert_eq!(reference.len(), got.len(), "{kind} {arm}: answer count differs from serial");
    if let Some(i) = (0..reference.len()).find(|&i| reference[i] != got[i]) {
        let mut want = format!("{:?}", reference[i]);
        let mut have = format!("{:?}", got[i]);
        want.truncate(2000);
        have.truncate(2000);
        panic!("{kind} {arm}: input {i} answered differently\n serial: {want}\n    arm: {have}");
    }
}

/// Every `.rs` file under `root`, sorted by path, read once.
fn corpus(root: &Path) -> Vec<(PathBuf, String)> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            (path, text)
        })
        .collect()
}

/// What `.git/HEAD` names, resolved one level through a loose ref. Best effort: a packed ref
/// prints as the ref name.
fn head(repo: &Path) -> String {
    let git = repo.join(".git");
    let Ok(head) = std::fs::read_to_string(git.join("HEAD")) else {
        return "unknown".into();
    };
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(name) => match std::fs::read_to_string(git.join(name)) {
            Ok(commit) => format!("{} ({name})", commit.trim()),
            Err(_) => name.to_string(),
        },
        None => head.to_string(),
    }
}

/// Small fixed crates for `analyze_source`: three clean shapes, one that uses library items the
/// `no_core` session cannot see, one that names something missing, and one that does not parse.
fn analyze_corpus() -> Vec<(String, String)> {
    (0..ANALYZE_SOURCES)
        .map(|i| {
            let source = match i % 6 {
                0 => format!(
                    "pub struct S{i} {{ pub a: u32, pub b: bool }}\n\
                     pub fn make{i}(a: u32, b: bool) -> S{i} {{ S{i} {{ a, b }} }}\n"
                ),
                1 => format!(
                    "pub struct P{i} {{ x: u32 }}\n\
                     impl P{i} {{ pub fn x(&self) -> u32 {{ self.x }} }}\n"
                ),
                2 => format!("pub enum E{i} {{ A, B(u32) }}\npub fn pick{i}(e: E{i}) -> bool {{ match e {{ E{i}::A => true, E{i}::B(_) => false }} }}\n"),
                3 => format!(
                    "fn count{i}(s: &str) -> usize {{ let n = s.chars().filter(|c| *c == 'a').count(); n }}\n"
                ),
                4 => format!("pub fn missing{i}() -> u32 {{ not_defined_{i}() }}\n"),
                _ => format!("pub fn broken{i}( {{\n"),
            };
            (format!("c{i}"), source)
        })
        .collect()
}
