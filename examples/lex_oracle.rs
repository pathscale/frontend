//! Holds `frontend_lex` to the eager diagnostic lexer: over every file given, both lex it, and
//! where both accept it the parser must read the same tokens, spans and spacing through its own
//! cursor, and the session must record the same raw identifiers and symbol gallery. Prints the
//! counts and the first differences, and every file only one of them accepts.
//!
//! Run: `cargo run --release --example lex_oracle -- [DIR...]`. The generated corpora are always
//! included; each file is compared in the 2015 and the 2021 edition. `SHOW=N` prints up to N
//! differences of each kind (default 10).

#[path = "support/corpus.rs"]
mod corpus;

use std::sync::Arc;

use frontend::rustc_parse::lex_hooks::{self, Compared};
use frontend::rustc_span::create_session_globals_then;
use frontend::rustc_span::edition::Edition;

#[derive(Default)]
struct Tally {
    same: usize,
    tokens: usize,
    both_refuse: usize,
    only_new: Vec<String>,
    only_eager: Vec<String>,
    differ: Vec<String>,
}

fn main() {
    let show: usize = std::env::var("SHOW").ok().and_then(|n| n.parse().ok()).unwrap_or(10);
    let mut files: Vec<(String, Arc<String>)> = Vec::new();
    for (i, text) in corpus::clean().into_iter().enumerate() {
        files.push((format!("clean/{i}"), Arc::new(text)));
    }
    for (i, text) in corpus::large().into_iter().enumerate() {
        files.push((format!("large/{i}"), Arc::new(text)));
    }
    for dir in std::env::args().skip(1) {
        for (path, text) in corpus::rust_files(std::path::Path::new(&dir)) {
            files.push((path.display().to_string(), Arc::new(text)));
        }
    }
    for edition in [Edition::Edition2015, Edition::Edition2021] {
        let mut tally = Tally::default();
        // A fresh session per file, as a check starts each file with a fresh interner.
        for (name, src) in &files {
            let compared = create_session_globals_then(edition, &[], None, || {
                lex_hooks::compare(src, edition)
            });
            match compared {
                Compared::Same(n) => {
                    tally.same += 1;
                    tally.tokens += n;
                }
                Compared::BothRefuse => tally.both_refuse += 1,
                Compared::OnlyNewAccepts => tally.only_new.push(name.clone()),
                Compared::OnlyEagerAccepts(error) => {
                    tally.only_eager.push(format!("{name}: {error:?}"))
                }
                Compared::Differ(what) => tally.differ.push(format!("{name}: {what}")),
            }
        }
        println!(
            "{edition:?}: {} files: {} same ({} tokens), {} both refuse, {} only the new lexer accepts, {} only the eager lexer accepts, {} differ",
            files.len(),
            tally.same,
            tally.tokens,
            tally.both_refuse,
            tally.only_new.len(),
            tally.only_eager.len(),
            tally.differ.len()
        );
        for line in tally.differ.iter().take(show) {
            println!("  differ: {line}");
        }
        for line in tally.only_eager.iter().take(show) {
            println!("  only the eager lexer accepts: {line}");
        }
        for line in tally.only_new.iter().take(show) {
            println!("  only the new lexer accepts: {line}");
        }
    }
}
