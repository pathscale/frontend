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

fn bench(label: &str, files: &[Arc<String>], rounds: usize) {
    let bytes: usize = files.iter().map(|f| f.len()).sum();
    let mb = bytes as f64 / 1e6;
    let pass = || {
        create_session_globals_then(Edition::Edition2021, &[], None, || {
            let mut tokens = 0;
            let mut refused = 0;
            for f in files {
                match lex_hooks::build(f, Edition::Edition2021) {
                    Some(n) => tokens += n,
                    None => refused += 1,
                }
            }
            (tokens, refused)
        })
    };
    let (tokens, refused) = pass();
    let mut ms: Vec<f64> = (0..rounds)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(pass());
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    let raw = (0..rounds.min(5))
        .map(|_| {
            let start = Instant::now();
            for f in files {
                let toks = frontend::rustc_lexer::tokenize(
                    f,
                    frontend::rustc_lexer::FrontmatterAllowed::No,
                );
                std::hint::black_box(toks.count());
            }
            start.elapsed().as_secs_f64() * 1e3
        })
        .fold(f64::MAX, f64::min);
    let median = ms[ms.len() / 2];
    println!(
        "{label:<10} {:>5} files {mb:>7.2} MB {tokens:>9} tokens {refused:>4} refused | tape median {median:>8.2} ms {:>7.1} MB/s  best {:>7.1} MB/s | bare lexer {:>7.1} MB/s",
        files.len(),
        mb * 1e3 / median,
        mb * 1e3 / ms[0],
        mb * 1e3 / raw,
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
