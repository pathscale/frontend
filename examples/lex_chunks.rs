//! Chunked lexing: correctness against the serial pass, and throughput on a nagoya pool.
//!
//! Run: `cargo run --release --example lex_chunks -- verify [DIR...]` or
//! `cargo run --release --example lex_chunks -- bench [ROUNDS] [DIR...]`.
//!
//! `verify` lexes every file (the generated corpora and every `.rs` under each `DIR`) serially
//! and chunked at several chunk sizes, and requires every field of the two files to be equal:
//! tokens, groups, item cuts, raw identifiers, suffixes, newlines, multi-byte characters,
//! gallery and error. Chunks there run one after another; order cannot change their output,
//! since a chunk touches nothing shared.
//!
//! `bench` builds large single files by concatenating sources (still one valid token stream)
//! and times, per file: the prescan alone, the serial pass, and the chunked pass with its chunks
//! as `nagoya::par_for` pieces on pools of 1, 2 and 4 workers, at two chunk sizes. Medians of
//! `ROUNDS` (default 9), in MB/s. Every chunked result is checked against the serial one.

#[path = "support/corpus.rs"]
mod corpus;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use frontend::rustc_span::edition::Edition;
use frontend::rustc_span::{create_session_globals_then, with_session_interner};
use frontend_lex::{Chunk, LexedFile, Options, Plan, PrescanSegment, Segments};

fn options() -> Options {
    Options { edition: Edition::Edition2021, strip_shebang: false, frontmatter: false }
}

/// Every field of two lexed files, or the first that differs.
fn same(a: &LexedFile, b: &LexedFile) -> Result<(), &'static str> {
    if a.error != b.error {
        return Err("error");
    }
    if a.tokens != b.tokens {
        return Err("tokens");
    }
    if a.groups != b.groups {
        return Err("groups");
    }
    if a.item_cuts != b.item_cuts {
        return Err("item cuts");
    }
    if a.raw_idents != b.raw_idents {
        return Err("raw identifiers");
    }
    if a.suffixes != b.suffixes {
        return Err("suffixes");
    }
    if a.newlines != b.newlines {
        return Err("newlines");
    }
    if a.multibyte != b.multibyte {
        return Err("multi-byte characters");
    }
    if a.gallery != b.gallery {
        return Err("gallery");
    }
    Ok(())
}

fn serial(src: &Arc<String>) -> Arc<LexedFile> {
    with_session_interner(|interner| frontend_lex::lex(Arc::clone(src), options(), interner))
}

/// Chunks lexed one after another, then assembled.
fn chunked_inline(src: &Arc<String>, plan: &Plan) -> Arc<LexedFile> {
    let chunks: Vec<Chunk> =
        (0..plan.len()).map(|i| frontend_lex::lex_chunk(src, plan, i)).collect();
    with_session_interner(|interner| {
        frontend_lex::assemble(Arc::clone(src), plan, chunks, interner)
    })
}

/// Runs `f(i)` for every `i` in `0..n` as `par_for` pieces on `pool`, results in order.
fn par_map<T: Send + 'static>(
    pool: &Arc<st3::fanout::Pool>,
    n: usize,
    f: impl Fn(usize) -> T + Send + Sync + 'static,
) -> Vec<T> {
    // The pool may still hold the closure, and so `out`, after `par_for` completes: results
    // are taken out of the slots, not unwrapped from the `Arc`.
    let slots: Arc<Vec<Mutex<Option<T>>>> = Arc::new((0..n).map(|_| Mutex::new(None)).collect());
    let out = Arc::clone(&slots);
    nagoya::block_on(nagoya::par_for(Arc::clone(pool), 0..n, move |i| {
        *out[i].lock().expect("no piece panics") = Some(f(i));
    }));
    slots
        .iter()
        .map(|slot| slot.lock().expect("no piece panics").take().expect("every piece ran"))
        .collect()
}

/// The prescan as `par_for` pieces, one per segment of `segment_bytes`.
fn plan_parallel(
    src: &Arc<String>,
    pool: &Arc<st3::fanout::Pool>,
    chunk_bytes: usize,
    segment_bytes: usize,
) -> Plan {
    let segments = Arc::new(Segments::new(src, options(), segment_bytes));
    let (s, g) = (Arc::clone(src), Arc::clone(&segments));
    let scanned: Vec<PrescanSegment> =
        par_map(pool, segments.len(), move |i| frontend_lex::prescan_segment(&s, &g, i));
    frontend_lex::plan_from_segments(src, &segments, scanned, chunk_bytes)
}

/// Chunks lexed as `par_for` pieces on `pool`, then assembled in order.
fn chunked_parallel(
    src: &Arc<String>,
    plan: &Arc<Plan>,
    pool: &Arc<st3::fanout::Pool>,
) -> Arc<LexedFile> {
    let (s, p) = (Arc::clone(src), Arc::clone(plan));
    let chunks = par_map(pool, plan.len(), move |i| frontend_lex::lex_chunk(&s, &p, i));
    with_session_interner(|interner| {
        frontend_lex::assemble(Arc::clone(src), plan, chunks, interner)
    })
}

fn inputs(dirs: &[String]) -> Vec<(String, Arc<String>)> {
    let mut files = Vec::new();
    for (i, text) in corpus::clean().into_iter().enumerate() {
        files.push((format!("clean/{i}"), Arc::new(text)));
    }
    for (i, text) in corpus::large().into_iter().enumerate() {
        files.push((format!("large/{i}"), Arc::new(text)));
    }
    for dir in dirs {
        for (path, text) in corpus::rust_files(std::path::Path::new(dir)) {
            files.push((path.display().to_string(), Arc::new(text)));
        }
    }
    files
}

fn verify(dirs: &[String]) {
    let files = inputs(dirs);
    for chunk_bytes in [512, 4096, 65536] {
        let (mut chunked, mut chunks, mut differ) = (0, 0, Vec::new());
        let (mut guessed, mut guesses, mut missed) = (0, 0, 0);
        for (name, src) in &files {
            create_session_globals_then(Edition::Edition2021, &[], None, || {
                let reference = serial(src);
                let plan = frontend_lex::plan(src, options(), chunk_bytes);
                // The prescan in segments, some tiny so literals cross their boundaries and the
                // rescan path runs, must plan exactly the same cuts.
                for segment_bytes in [97, 1000, 16384] {
                    let segments = frontend_lex::Segments::new(src, options(), segment_bytes);
                    let scanned = (0..segments.len())
                        .map(|i| frontend_lex::prescan_segment(src, &segments, i))
                        .collect();
                    let composed =
                        frontend_lex::plan_from_segments(src, &segments, scanned, chunk_bytes);
                    if composed.bounds() != plan.bounds() {
                        differ.push(format!("{name}: segmented prescan ({segment_bytes} B) cuts"));
                    }
                }
                // Speculative cuts: whatever they guess, the assembled file must be the serial one.
                let guess = frontend_lex::speculate(src, options(), chunk_bytes);
                if guess.len() >= 2 {
                    guessed += 1;
                    guesses += guess.len();
                    let lexed: Vec<Chunk> =
                        (0..guess.len()).map(|i| frontend_lex::lex_chunk(src, &guess, i)).collect();
                    if let Some(i) = lexed[..lexed.len() - 1].iter().position(|c| !c.continues()) {
                        missed += guess.len() - 1 - i;
                    }
                    let b = with_session_interner(|interner| {
                        frontend_lex::assemble(Arc::clone(src), &guess, lexed, interner)
                    });
                    if let Err(what) = same(&reference, &b) {
                        differ.push(format!("{name}: speculative {what} ({} chunks)", guess.len()));
                    }
                }
                if plan.len() < 2 {
                    return;
                }
                chunked += 1;
                chunks += plan.len();
                let b = chunked_inline(src, &plan);
                if let Err(what) = same(&reference, &b) {
                    differ.push(format!("{name}: {what} ({} chunks)", plan.len()));
                }
            });
        }
        println!(
            "chunk {chunk_bytes:>6} B: {} files, prescan {chunked} chunked into {chunks}, \
             speculative {guessed} into {guesses} ({missed} lost to a wrong guess), {} differ",
            files.len(),
            differ.len()
        );
        for line in differ.iter().take(10) {
            println!("  differ: {line}");
        }
    }
}

fn median(rounds: usize, mut f: impl FnMut()) -> f64 {
    f();
    let mut ms: Vec<f64> = (0..rounds)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[ms.len() / 2]
}

fn bench(rounds: usize, dirs: &[String]) {
    let mut big: Vec<(String, Arc<String>)> = vec![
        ("large x1".into(), Arc::new(corpus::large().concat())),
        ("clean x1".into(), Arc::new(corpus::clean().concat())),
    ];
    for dir in dirs {
        let text: String = corpus::rust_files(std::path::Path::new(dir))
            .into_iter()
            .map(|(_, t)| t + "\n")
            .collect();
        big.push((format!("{dir} concat"), Arc::new(text)));
    }
    let widths: Vec<usize> = std::env::var("LEX_WORKERS")
        .ok()
        .map(|v| v.split(',').filter_map(|n| n.parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 2, 4]);
    let pools: Vec<(usize, nagoya::runtime::Runtime)> = widths
        .into_iter()
        .map(|n| (n, nagoya::runtime::Runtime::builder().workers(n).label("lex").build()))
        .collect();
    for (name, src) in &big {
        let mb = src.len() as f64 / 1e6;
        let rate = |ms: f64| mb * 1e3 / ms;
        create_session_globals_then(Edition::Edition2021, &[], None, || {
            let reference = serial(src);
            let serial_ms = median(rounds, || {
                std::hint::black_box(serial(src));
            });
            println!("{name}: {mb:.2} MB, serial {:.1} MB/s", rate(serial_ms));
            for chunk_bytes in [64 * 1024, 256 * 1024] {
                let plan = frontend_lex::plan(src, options(), chunk_bytes);
                let guess = frontend_lex::speculate(src, options(), chunk_bytes);
                let prescan_ms = median(rounds, || {
                    std::hint::black_box(frontend_lex::plan(src, options(), chunk_bytes));
                });
                println!(
                    "  chunks of {:>3} KiB: prescan {} chunks at {:.1} MB/s, speculative {} chunks",
                    chunk_bytes / 1024,
                    plan.len(),
                    rate(prescan_ms),
                    guess.len()
                );
                type Planner<'a> = Box<dyn Fn(&Arc<st3::fanout::Pool>) -> Plan + 'a>;
                let planners: [(&str, Planner); 3] = [
                    ("prescan  ", Box::new(|_| frontend_lex::plan(src, options(), chunk_bytes))),
                    (
                        "par-scan ",
                        Box::new(|pool| plan_parallel(src, pool, chunk_bytes, chunk_bytes)),
                    ),
                    (
                        "speculate",
                        Box::new(|_| frontend_lex::speculate(src, options(), chunk_bytes)),
                    ),
                ];
                for (label, planner) in &planners {
                    let mut line = format!("    {label} |");
                    for (workers, runtime) in &pools {
                        let pool = runtime.pool();
                        let result = chunked_parallel(src, &Arc::new(planner(pool)), pool);
                        assert!(same(&reference, &result).is_ok(), "{name}: {label} differs");
                        let ms = median(rounds, || {
                            let plan = Arc::new(planner(pool));
                            std::hint::black_box(chunked_parallel(src, &plan, pool));
                        });
                        line.push_str(&format!(
                            " {workers}w {:>7.1} MB/s ({:.2}x)",
                            rate(ms),
                            serial_ms / ms
                        ));
                    }
                    println!("{line}");
                }
            }
        });
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("verify") => verify(&args.collect::<Vec<_>>()),
        Some("bench") => {
            let rest: Vec<String> = args.collect();
            let (rounds, dirs) = match rest.first().and_then(|n| n.parse().ok()) {
                Some(n) => (n, rest[1..].to_vec()),
                None => (9, rest),
            };
            bench(rounds, &dirs)
        }
        _ => eprintln!("usage: lex_chunks verify [DIR...] | lex_chunks bench [ROUNDS] [DIR...]"),
    }
}
