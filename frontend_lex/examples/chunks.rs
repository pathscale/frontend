//! Chunked lexing throughput on a nagoya pool, without the main crate: rebuilds in seconds.
//!
//! Run: `cargo run --release -p frontend_lex --example chunks -- [ROUNDS] DIR...`. Each `DIR`'s
//! `.rs` files are concatenated into one file (still one valid token stream), then timed: the
//! serial pass, the prescan alone (one task, and in segments on the pool), and the chunked pass
//! planned three ways (prescan, segmented prescan on the pool, speculative cuts) with chunks as
//! `par_for` pieces, on pools of `LEX_WORKERS` (default `1,2,4`) workers. Medians, in MB/s.
//! Every chunked result is checked against the serial one.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use frontend_lex::{Assembly, Chunk, LexedFile, Options, Part, Plan, PrescanSegment, Segments};
use frontend_symbol::Interner;
use frontend_symbol::edition::Edition;

type Pool = Arc<st3::fanout::Pool>;

fn options() -> Options {
    Options { edition: Edition::Edition2021, strip_shebang: false, frontmatter: false }
}

fn same(a: &LexedFile, b: &LexedFile) -> bool {
    a.error == b.error
        && a.tokens == b.tokens
        && a.groups == b.groups
        && a.item_cuts == b.item_cuts
        && a.raw_idents == b.raw_idents
        && a.suffixes == b.suffixes
        && a.newlines == b.newlines
        && a.multibyte == b.multibyte
        && a.gallery == b.gallery
}

fn par_map<T: Send + 'static>(
    pool: &Pool,
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

fn plan_parallel(src: &Arc<String>, pool: &Pool, chunk_bytes: usize, segment_bytes: usize) -> Plan {
    let segments = Arc::new(Segments::new(src, options(), segment_bytes));
    let (s, g) = (Arc::clone(src), Arc::clone(&segments));
    let scanned: Vec<PrescanSegment> =
        par_map(pool, segments.len(), move |i| frontend_lex::prescan_segment(&s, &g, i));
    frontend_lex::plan_from_segments(src, &segments, scanned, chunk_bytes)
}

fn chunked(
    src: &Arc<String>,
    plan: Plan,
    pool: &Pool,
    interner: &'static Interner,
) -> Arc<LexedFile> {
    let plan = Arc::new(plan);
    let (s, p) = (Arc::clone(src), Arc::clone(&plan));
    let chunks: Vec<Chunk> =
        par_map(pool, plan.len(), move |i| lex_and_look_up(&s, &p, i, interner));
    let mut assembly = Assembly::new(Arc::clone(src), &plan, chunks, interner);
    patch_parallel(assembly.parts(), pool);
    assembly.finish()
}

/// Every part patched as its own `par_for` piece: each piece takes its part out of a slot.
fn patch_parallel(parts: Vec<Part>, pool: &Pool) {
    if parts.len() < 2 {
        return parts.into_iter().for_each(Part::patch);
    }
    let n = parts.len();
    let slots: Arc<Vec<Mutex<Option<Part>>>> =
        Arc::new(parts.into_iter().map(|part| Mutex::new(Some(part))).collect());
    let input = Arc::clone(&slots);
    par_map(pool, n, move |i| {
        input[i].lock().expect("no piece panics").take().expect("each part is patched once").patch()
    });
}

fn rust_files(dir: &Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            out.push_str(&text);
            out.push('\n');
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

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let rounds = match args.first().and_then(|n| n.parse().ok()) {
        Some(n) => {
            args.remove(0);
            n
        }
        None => 9,
    };
    let widths: Vec<usize> = std::env::var("LEX_WORKERS")
        .ok()
        .map(|v| v.split(',').filter_map(|n| n.parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 2, 4]);
    let pools: Vec<(usize, nagoya::runtime::Runtime)> = widths
        .into_iter()
        .map(|n| (n, nagoya::runtime::Runtime::builder().workers(n).label("lex").build()))
        .collect();
    let interner: &'static Interner = Box::leak(Box::new(Interner::with_extra_symbols(&[])));
    for dir in &args {
        let mut text = String::new();
        rust_files(Path::new(dir), &mut text);
        let src = Arc::new(text);
        let mb = src.len() as f64 / 1e6;
        let rate = |ms: f64| mb * 1e3 / ms;
        let reference = frontend_lex::lex(Arc::clone(&src), options(), interner);
        let serial_ms = median(rounds, || {
            std::hint::black_box(frontend_lex::lex(Arc::clone(&src), options(), interner));
        });
        println!("{dir}: {mb:.2} MB, serial {:.1} MB/s", rate(serial_ms));
        if std::env::var_os("LEX_BREAKDOWN").is_some() {
            for (workers, runtime) in &pools {
                println!("  {workers} workers, 64 KiB:");
                breakdown(&src, runtime.pool(), interner, 64 * 1024);
            }
            continue;
        }
        for chunk_bytes in [64 * 1024, 256 * 1024] {
            let prescan_ms = median(rounds, || {
                std::hint::black_box(frontend_lex::plan(&src, options(), chunk_bytes));
            });
            let mut line = format!(
                "  {:>3} KiB: prescan {} chunks {:.0} MB/s, speculative {} chunks; par prescan",
                chunk_bytes / 1024,
                frontend_lex::plan(&src, options(), chunk_bytes).len(),
                rate(prescan_ms),
                frontend_lex::speculate(&src, options(), chunk_bytes).len(),
            );
            for (workers, runtime) in &pools {
                let pool = runtime.pool();
                let ms = median(rounds, || {
                    std::hint::black_box(plan_parallel(&src, pool, chunk_bytes, chunk_bytes));
                });
                line.push_str(&format!(" {workers}w {:.0}", rate(ms)));
            }
            println!("{line}");
            type Planner<'a> = Box<dyn Fn(&Pool) -> Plan + 'a>;
            let planners: [(&str, Planner); 3] = [
                ("prescan  ", Box::new(|_| frontend_lex::plan(&src, options(), chunk_bytes))),
                ("par-scan ", Box::new(|pool| plan_parallel(&src, pool, chunk_bytes, chunk_bytes))),
                ("speculate", Box::new(|_| frontend_lex::speculate(&src, options(), chunk_bytes))),
            ];
            for (label, planner) in &planners {
                let mut line = format!("    {label} |");
                for (workers, runtime) in &pools {
                    let pool = runtime.pool();
                    let result = chunked(&src, planner(pool), pool, interner);
                    assert!(same(&reference, &result), "{dir}: {label} differs from serial");
                    let ms = median(rounds, || {
                        std::hint::black_box(chunked(&src, planner(pool), pool, interner));
                    });
                    line.push_str(&format!(
                        " {workers}w {:>6.1} MB/s ({:.2}x)",
                        rate(ms),
                        serial_ms / ms
                    ));
                }
                println!("{line}");
            }
        }
    }
}

/// Where the time goes in a speculative chunked lex: planning, the parallel chunks, assembly.
#[allow(dead_code)]
pub fn breakdown(src: &Arc<String>, pool: &Pool, interner: &'static Interner, chunk_bytes: usize) {
    const ROUNDS: usize = 7;
    let mut t = [0f64; 5];
    for _ in 0..ROUNDS {
        let a = Instant::now();
        let plan = Arc::new(frontend_lex::speculate(src, options(), chunk_bytes));
        let b = Instant::now();
        let (s, p) = (Arc::clone(src), Arc::clone(&plan));
        let chunks: Vec<Chunk> =
            par_map(pool, plan.len(), move |i| lex_and_look_up(&s, &p, i, interner));
        let c = Instant::now();
        let mut assembly = Assembly::new(Arc::clone(src), &plan, chunks, interner);
        let d = Instant::now();
        patch_parallel(assembly.parts(), pool);
        let e = Instant::now();
        std::hint::black_box(assembly.finish());
        let f = Instant::now();
        for (slot, (x, y)) in t.iter_mut().zip([(a, b), (b, c), (c, d), (d, e), (e, f)]) {
            *slot += (y - x).as_secs_f64() * 1e3 / ROUNDS as f64;
        }
    }
    println!(
        "    plan {:.1} ms, chunks {:.1} ms, resolve {:.1} ms, patch {:.1} ms, finish {:.1} ms",
        t[0], t[1], t[2], t[3], t[4]
    );
}

/// One chunk's task: lex it, then find the symbols the interner already has for it.
fn lex_and_look_up(src: &str, plan: &Plan, index: usize, interner: &'static Interner) -> Chunk {
    let mut chunk = frontend_lex::lex_chunk(src, plan, index);
    if std::env::var_os("LEX_NO_LOOKUP").is_none() {
        chunk.look_up(src, interner);
    }
    chunk
}
