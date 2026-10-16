//! Lexing throughput: the token tape the parser reads, built alone, in MB/s.
//!
//! Run: `cargo run --release --example lex_bench -- [ROUNDS] [DIR...]`.
//!
//! Workloads: the generated `clean` and `large` corpora, and every `.rs` file under each `DIR`
//! (this crate's `src`, a cargo registry). Each round builds the tape of every file once,
//! inside fresh session globals (the tape interns, and a real check starts each file with a
//! fresh interner); the median and best rounds are reported. A file the tape refuses is
//! counted and its bytes still count towards the rate: the parser then lexes it eagerly.
//!
//! A bare `rustc_lexer::tokenize` pass over the same bytes is printed beside each workload, as
//! the rate of scanning tokens and nothing else. `ONLY=clean|large|dirs` runs one group.

#[path = "support/corpus.rs"]
mod corpus;

use std::sync::Arc;
use std::time::Instant;

use frontend::rustc_ast::tokenstream::lex_hooks;
use frontend::rustc_span::create_session_globals_then;
use frontend::rustc_span::edition::Edition;

/// Median and best ms of `rounds` timed passes of `pass`, after one untimed one.
fn time<R>(rounds: usize, mut pass: impl FnMut() -> R) -> (f64, f64) {
    std::hint::black_box(pass());
    let mut ms: Vec<f64> = (0..rounds)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(pass());
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    (ms[ms.len() / 2], ms[0])
}

fn bench(label: &str, files: &[Arc<String>], rounds: usize) {
    let bytes: usize = files.iter().map(|f| f.len()).sum();
    let mb = bytes as f64 / 1e6;
    let edition = Edition::Edition2021;
    // Each pass in fresh session globals: a check starts each file with a fresh interner.
    let tape = || {
        create_session_globals_then(edition, &[], None, || {
            files.iter().filter(|f| lex_hooks::build(f, edition).is_none()).count()
        })
    };
    let lexer = || {
        create_session_globals_then(edition, &[], None, || {
            files.iter().filter(|f| lex_hooks::lex(f, edition).is_err()).count()
        })
    };
    let (tape_refused, lex_refused) = (tape(), lexer());
    let (tape_median, tape_best) = time(rounds, tape);
    let (lex_median, lex_best) = time(rounds, lexer);
    let (_, raw) = time(rounds.min(5), || {
        for f in files {
            let tokens =
                frontend::rustc_lexer::tokenize(f, frontend::rustc_lexer::FrontmatterAllowed::No);
            std::hint::black_box(tokens.count());
        }
    });
    let rate = |ms: f64| mb * 1e3 / ms;
    println!(
        "{label:<10} {:>5} files {mb:>7.2} MB | old tape {:>7.1} MB/s (best {:>7.1}, {tape_refused} refused) | frontend_lex {:>7.1} MB/s (best {:>7.1}, {lex_refused} stopped) | bare rustc_lexer {:>7.1} MB/s",
        files.len(),
        rate(tape_median),
        rate(tape_best),
        rate(lex_median),
        rate(lex_best),
        rate(raw),
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rounds: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(9);
    let dirs: Vec<String> = args.collect();
    let only = std::env::var("ONLY").ok();
    let wants = |name: &str| only.as_deref().is_none_or(|o| o == name);
    let shared = |files: Vec<String>| files.into_iter().map(Arc::new).collect::<Vec<_>>();
    if wants("clean") {
        bench("clean", &shared(corpus::clean()), rounds);
    }
    if wants("large") {
        bench("large", &shared(corpus::large()), rounds);
    }
    if wants("dirs") {
        for dir in dirs {
            let files = corpus::rust_files(std::path::Path::new(&dir));
            let name = std::path::Path::new(&dir)
                .file_name()
                .map_or(dir.clone(), |n| n.to_string_lossy().into_owned());
            bench(&name, &shared(files.into_iter().map(|(_, text)| text).collect()), rounds);
        }
    }
}
