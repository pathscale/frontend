//! Time `check_source` and `analyze_source` over three corpora at several parallelism settings,
//! and check every setting gives the width-one answer.
//!
//! Run: `cargo run --release --features parallel --example parallel_timing`.
//!
//! Simple on purpose: one pass per setting over every file of a corpus, total wall time, and the
//! speedup against width one from the same run. Without the `parallel` feature every width is
//! the serial path and the speedups are noise around 1.0.
//!
//! **The three corpora.**
//!
//! - `src`: this crate's own `src/**/*.rs`, sorted by path. Real code, but a session runs as
//!   `no_core`, so nearly every file stops early on a missing lang item or an unresolved `std`
//!   name, and the per-body stages that come after type checking barely run.
//! - `clean`: 200 generated files of 300 to 1,500 lines that type check with no error, so every
//!   stage runs over every body.
//! - `large`: 6 generated files of 5,000 to 8,000 lines, hundreds of functions each, because a
//!   speedup inside one file needs many bodies in that file.
//!
//! **How the clean files are made.** `tests/corpus/prelude.rs` is the lang item prelude (the
//! one `tests/parallel.rs` uses, plus the operator traits integer arithmetic needs), and each
//! `tests/corpus/unit_*.rs` is a block of items whose every name carries the tag `Q0`. A file is
//! the prelude followed by units, each copy with `Q0` replaced by a tag unique to its file and
//! position, so the copies do not collide and no two files are the same. Which unit comes next
//! and how long a file is are drawn from a fixed-seed generator, so every run builds the same
//! corpus. Nothing is written to disk.
//!
//! For the generated corpora the width-one pass also prints how many files came back with no
//! error; if that is not all of them, the first failing file's first errors, which is a defect
//! in a unit template to fix, not something to time around.

use std::time::Instant;

use frontend::frontend_facts::{
    CrateFacts, Checked, analyze_source_with_width, check_source_with_width,
};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// The settings timed. The first is the reference every other one is held to.
const SETTINGS: &[usize] = &[1, 2, 4, 8, 12];

fn src_corpus() -> Vec<String> {
    let mut paths = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                paths.push(path);
            }
        }
    }
    // Sorted, so "file 17" names the same file in every run.
    paths.sort();
    paths.iter().map(|path| std::fs::read_to_string(path).expect("read")).collect()
}

const PRELUDE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/prelude.rs"));

const UNITS: &[&str] = &[
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_geometry.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_ledger.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_machine.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_numeric.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_cells.rs")),
];

/// A 64-bit linear congruential generator (Knuth's MMIX constants) with a fixed seed: the
/// corpus is the same in every run.
struct Lcg(u64);

impl Lcg {
    /// A number in `low..=high`.
    fn between(&mut self, low: usize, high: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        low + (self.0 >> 33) as usize % (high - low + 1)
    }
}

/// `count` files of `min_lines..=max_lines` lines each. `name` goes into every tag, so the
/// clean and large corpora do not share a file either.
fn generated(name: char, count: usize, min_lines: usize, max_lines: usize, seed: u64) -> Vec<String> {
    let mut rng = Lcg(seed);
    (0..count)
        .map(|file| {
            let target = rng.between(min_lines, max_lines);
            let mut text = format!("// Generated corpus file {name}{file}.\n{PRELUDE}");
            let mut unit = 0;
            while text.lines().count() < target {
                let template = UNITS[rng.between(0, UNITS.len() - 1)];
                text.push('\n');
                text.push_str(&template.replace("Q0", &format!("Q{name}{file}x{unit}")));
                unit += 1;
            }
            text
        })
        .collect()
}

/// Both entry points' answers for a whole corpus, at one setting.
type Answers = (Vec<Checked>, Vec<Option<CrateFacts>>);

/// One pass over `files` at stage width `width`: the answers, and milliseconds per entry point.
fn pass(width: usize, files: &[String]) -> (Answers, f64, f64) {
    frontend::unwind_janky::install_catcher(catcher);
    let start = Instant::now();
    let checked: Vec<Checked> =
        files.iter().map(|s| check_source_with_width("corpus", s, width)).collect();
    let check_ms = start.elapsed().as_secs_f64() * 1e3;
    let start = Instant::now();
    let facts: Vec<Option<CrateFacts>> =
        files.iter().map(|s| analyze_source_with_width("corpus", s, None, width).ok()).collect();
    let analyze_ms = start.elapsed().as_secs_f64() * 1e3;
    ((checked, facts), check_ms, analyze_ms)
}

/// Time one corpus at every setting, and hold each setting to the first one's answers.
fn run(label: &str, files: &[String], report_clean: bool) {
    let bytes: usize = files.iter().map(String::len).sum();
    let lines: usize = files.iter().map(|f| f.lines().count()).sum();
    println!("\n{label}: {} files, {lines} lines, {bytes} bytes", files.len());
    println!("{:>7} {:>12} {:>8} {:>12} {:>8}", "width", "check ms", "x", "analyze ms", "x");
    let one = SETTINGS[0];
    let (want, check_one, analyze_one) = pass(one, files);
    println!("{one:>7} {check_one:>12.1} {:>8.2} {analyze_one:>12.1} {:>8.2}", 1.0, 1.0);
    if report_clean {
        clean_report(&want.0);
    }
    for &width in &SETTINGS[1..] {
        let (answers, check_ms, analyze_ms) = pass(width, files);
        if answers != want {
            for (i, (a, b)) in want.0.iter().zip(&answers.0).enumerate() {
                if a != b {
                    eprintln!("{label}: check differs at file {i}:\n  one: {a:?}\n  {width}: {b:?}");
                    break;
                }
            }
            for (i, (a, b)) in want.1.iter().zip(&answers.1).enumerate() {
                if a != b {
                    eprintln!("{label}: analyze differs at file {i}:\n  one: {a:?}\n  {width}: {b:?}");
                    break;
                }
            }
            panic!("{label}: width {width} gave a different answer than width one");
        }
        let (check_x, analyze_x) = (check_one / check_ms, analyze_one / analyze_ms);
        println!("{width:>7} {check_ms:>12.1} {check_x:>8.2} {analyze_ms:>12.1} {analyze_x:>8.2}");
    }
}

/// How many files type checked with no error, and what the first one that did not said.
fn clean_report(checked: &[Checked]) {
    let clean = checked.iter().filter(|c| c.is_clean()).count();
    println!("clean at width one: {clean} of {}", checked.len());
    if let Some((i, first)) = checked.iter().enumerate().find(|(_, c)| !c.is_clean()) {
        println!("  first unclean file {i}, fatal {}:", first.fatal);
        for error in first.errors.iter().take(5) {
            println!("    {error}");
        }
    }
}

/// `parallel_timing [src|clean|large] [width]`: with a corpus named, only that corpus; with a
/// width as well, only that setting, once, with no comparison.
fn main() {
    let mut args = std::env::args().skip(1);
    let only = args.next();
    let width: Option<usize> = args.next().and_then(|n| n.parse().ok());
    let corpus = |name: &str| match name {
        "src" => src_corpus(),
        "clean" => generated('c', 200, 300, 1_500, 0x5eed_0001),
        _ => generated('l', 6, 5_000, 8_000, 0x5eed_0002),
    };
    // `parallel_timing dump <src|clean|large> <file>`: every answer at width one, written out,
    // to diff one build's answers against another's.
    if only.as_deref() == Some("dump") {
        let name = std::env::args().nth(2).unwrap_or_else(|| "src".into());
        let path = std::env::args().nth(3).expect("dump needs an output file");
        let ((checked, facts), _, _) = pass(1, &corpus(&name));
        std::fs::write(&path, format!("{checked:#?}\n{facts:#?}\n")).expect("write dump");
        return;
    }
    match (only.as_deref(), width) {
        (Some(name), Some(width)) => {
            let (_, check_ms, analyze_ms) = pass(width, &corpus(name));
            println!("{name} at {width}: check {check_ms:.1} ms, analyze {analyze_ms:.1} ms");
        }
        (Some(name), None) => run(name, &corpus(name), name != "src"),
        (None, _) => {
            run("src", &corpus("src"), false);
            run("clean", &corpus("clean"), true);
            run("large", &corpus("large"), true);
        }
    }
}
