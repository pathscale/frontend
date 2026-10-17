//! Serial lexing throughput, file by file, without the main crate.
//! Run: `cargo run --release -p frontend_lex --example serial -- [ROUNDS] DIR...`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use frontend_lex::Options;
use frontend_symbol::Interner;
use frontend_symbol::edition::Edition;

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let rounds: usize = match args.first().and_then(|n| n.parse().ok()) {
        Some(n) => {
            args.remove(0);
            n
        }
        None => 9,
    };
    let options =
        Options { edition: Edition::Edition2021, strip_shebang: false, frontmatter: false };
    let interner = Interner::with_extra_symbols(&[]);
    for dir in &args {
        let mut paths = Vec::new();
        files(Path::new(dir), &mut paths);
        paths.sort();
        let sources: Vec<Arc<String>> =
            paths.iter().filter_map(|p| std::fs::read_to_string(p).ok()).map(Arc::new).collect();
        let bytes: usize = sources.iter().map(|s| s.len()).sum();
        let mut ms: Vec<f64> = (0..rounds.max(1))
            .map(|_| {
                let start = Instant::now();
                for src in &sources {
                    std::hint::black_box(frontend_lex::lex(Arc::clone(src), options, &interner));
                }
                start.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        let median = ms[ms.len() / 2];
        println!(
            "{dir}: {} files, {:.2} MB, {median:.1} ms, {:.1} MB/s",
            sources.len(),
            bytes as f64 / 1e6,
            bytes as f64 / 1e3 / median
        );
    }
}
