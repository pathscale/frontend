//! Chunked lexing against the serial pass, per file, without the main crate.
//!
//! Run: `cargo run --release -p frontend_lex --example verify -- DIR...`. For every `.rs` file
//! under each `DIR`, at chunk sizes of 512 B, 4 KiB and 64 KiB: the segmented prescan (segments
//! of 97 B, 1000 B and 16 KiB, so literals cross segment boundaries) must plan the single pass's
//! cuts, and the file lexed in prescanned chunks and in speculative chunks must equal the serial
//! one in every field. Prints the first differences and how many speculative chunks were
//! guessed wrong.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use frontend_lex::{Chunk, LexedFile, Options, Plan};
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

fn same(a: &LexedFile, b: &LexedFile) -> Result<(), &'static str> {
    let checks: [(bool, &str); 9] = [
        (a.error == b.error, "error"),
        (a.tokens == b.tokens, "tokens"),
        (a.groups == b.groups, "groups"),
        (a.item_cuts == b.item_cuts, "item cuts"),
        (a.raw_idents == b.raw_idents, "raw identifiers"),
        (a.suffixes == b.suffixes, "suffixes"),
        (a.newlines == b.newlines, "newlines"),
        (a.multibyte == b.multibyte, "multi-byte characters"),
        (a.gallery == b.gallery, "gallery"),
    ];
    match checks.iter().find(|(ok, _)| !ok) {
        Some((_, what)) => Err(what),
        None => Ok(()),
    }
}

/// The file lexed in `plan`'s chunks, and how many chunks did not leave the lexer fresh.
fn chunked(src: &Arc<String>, plan: &Plan, interner: &Interner) -> (Arc<LexedFile>, usize) {
    let chunks: Vec<Chunk> =
        (0..plan.len()).map(|i| frontend_lex::lex_chunk(src, plan, i)).collect();
    let wrong = chunks[..chunks.len() - 1].iter().filter(|c| !c.continues()).count();
    (frontend_lex::assemble(Arc::clone(src), plan, chunks, interner), wrong)
}

fn main() {
    let options =
        Options { edition: Edition::Edition2021, strip_shebang: false, frontmatter: false };
    let mut paths = Vec::new();
    for dir in std::env::args().skip(1) {
        files(Path::new(&dir), &mut paths);
    }
    paths.sort();
    let interner = Interner::with_extra_symbols(&[]);
    let sources: Vec<(PathBuf, Arc<String>)> = paths
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|s| (p, Arc::new(s))))
        .collect();
    let serial: Vec<Arc<LexedFile>> = sources
        .iter()
        .map(|(_, src)| frontend_lex::lex(Arc::clone(src), options, &interner))
        .collect();
    for chunk_bytes in [512, 4096, 65536] {
        let mut differ = Vec::new();
        let (mut planned, mut guessed, mut wrong) = (0, 0, 0);
        for ((path, src), reference) in sources.iter().zip(&serial) {
            let name = path.display();
            let plan = frontend_lex::plan(src, options, chunk_bytes);
            for segment_bytes in [97, 1000, 16384] {
                let segments = frontend_lex::Segments::new(src, options, segment_bytes);
                let scanned = (0..segments.len())
                    .map(|i| frontend_lex::prescan_segment(src, &segments, i))
                    .collect();
                let composed =
                    frontend_lex::plan_from_segments(src, &segments, scanned, chunk_bytes);
                if composed.bounds() != plan.bounds() {
                    differ.push(format!("{name}: segmented prescan ({segment_bytes} B) cuts"));
                }
            }
            if plan.len() > 1 {
                planned += plan.len();
                let (file, missed) = chunked(src, &plan, &interner);
                if let Err(what) = same(reference, &file) {
                    differ.push(format!("{name}: prescanned {what}"));
                }
                if missed > 0 && reference.error.is_none() {
                    differ.push(format!("{name}: {missed} prescanned chunks not fresh"));
                }
            }
            let guess = frontend_lex::speculate(src, options, chunk_bytes);
            if guess.len() > 1 {
                guessed += guess.len();
                let (file, missed) = chunked(src, &guess, &interner);
                wrong += missed;
                if let Err(what) = same(reference, &file) {
                    differ.push(format!("{name}: speculative {what}"));
                }
            }
        }
        println!(
            "chunk {chunk_bytes:>5} B: {} files; prescan {planned} chunks; speculative {guessed} \
             chunks, {wrong} guessed wrong; {} differ",
            sources.len(),
            differ.len()
        );
        for line in differ.iter().take(8) {
            println!("  {line}");
        }
    }
}
