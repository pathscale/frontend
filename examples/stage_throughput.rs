//! Front-to-back throughput of `check_source`, stage by stage: where the bytes per second drop.
//!
//! Run: `cargo run --release --features stage-timing --example stage_throughput -- [ROUNDS]`.
//!
//! Each corpus (`parallel_timing`'s `clean` and `large`, the same generator and seeds) is
//! checked once untimed, every file held to no errors, then `ROUNDS` times (default 3) at session
//! width `WIDTH` (default 1), one file at a time. `rustc_data_structures::stage_timing` records
//! exclusive time per query and per hand-placed marker. The table groups those slots into stages in pipeline
//! order and prints, per stage:
//!
//! - **ms**: its exclusive time per pass over the corpus.
//! - **MB/s alone**: corpus bytes over that time, the rate the stage would run at by itself.
//! - **MB/s through**: corpus bytes over the time of every stage up to and including this one,
//!   the throughput the pipeline has fallen to by the end of it.
//! - its top queries, so a misfiled query is visible.
//!
//! Exclusive time means each nanosecond is charged once, to whatever was running: type checking
//! demanded from inside MIR building is type checking, not MIR building. Stages are grouped by
//! query name in [`stage_of`]; the grouping is this file's, and a query no rule names lands in
//! "item sema", which lists its top queries like every other stage.
//!
//! `ONLY=clean|large` runs one corpus. `TOP=N` lists N queries per stage (default 3).
//! The counting allocator delegates to mimalloc.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use frontend::frontend_facts::check_shared_source_with_width;
use frontend::rustc_data_structures::stage_timing;

struct Counting;
static INNER: mimalloc::MiMalloc = mimalloc::MiMalloc;
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Relaxed);
        }
        unsafe { INNER.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { INNER.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            ALLOC_BYTES.fetch_add(new_size as u64, Relaxed);
        }
        unsafe { INNER.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

const PRELUDE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/prelude.rs"));

const UNITS: &[&str] = &[
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_geometry.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_ledger.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_machine.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_numeric.rs")),
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/unit_cells.rs")),
];

/// `parallel_timing`'s generator, unchanged, so the corpora are byte for byte the same.
struct Lcg(u64);

impl Lcg {
    fn between(&mut self, low: usize, high: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        low + (self.0 >> 33) as usize % (high - low + 1)
    }
}

fn generated(
    name: char,
    count: usize,
    min_lines: usize,
    max_lines: usize,
    seed: u64,
) -> Vec<String> {
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

/// The stages, front to back.
const STAGES: &[&str] = &[
    "source",
    "lex",
    "parse",
    "expand",
    "resolve",
    "lower to HIR",
    "item sema",
    "traits",
    "typeck",
    "THIR",
    "MIR build",
    "borrowck",
    "MIR passes",
    "const eval",
    "checks, lints",
    "driver",
];

/// Which stage a slot belongs to: markers by name, queries by name.
fn stage_of(name: &str, query: bool) -> &'static str {
    if !query {
        return match name {
            "source" => "source",
            "lex" => "lex",
            "parse" => "parse",
            "expand" => "expand",
            _ => "driver",
        };
    }
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| name.starts_with(p));
    if starts(&["resolver_for_lowering", "resolutions", "registered_tools", "crate_for_resolver"]) {
        "resolve"
    } else if starts(&["lower_to_hir", "hir_", "opt_hir_", "local_def_id_to_hir_id", "lower_"]) {
        "lower to HIR"
    } else if name == "mir_built" {
        "MIR build"
    } else if starts(&["mir_borrowck", "borrowck", "check_liveness_borrowck"]) {
        "borrowck"
    } else if starts(&["thir_", "check_match", "check_unsafety", "check_tail_calls"]) {
        "THIR"
    } else if starts(&["mir_", "optimized_mir", "promoted_mir", "check_liveness"]) {
        "MIR passes"
    } else if starts(&[
        "typeck",
        "diagnostic_only_typeck",
        "used_trait_imports",
        "closure_",
        "coroutine_",
        "check_coroutine",
        "nested_bodies_within",
        "has_ffi_unwind_calls",
    ]) {
        "typeck"
    } else if starts(&[
        "eval_",
        "const_",
        "lit_to_const",
        "valtree_",
        "destructure_",
        "try_destructure",
        "check_validity",
    ]) {
        "const eval"
    } else if starts(&[
        "evaluate_",
        "type_op_",
        "normalize_",
        "dropck_",
        "implied_outlives",
        "method_autoderef",
        "codegen_select",
        "specializ",
        "trait_",
        "impl_",
        "inherent_impls",
        "crate_inherent_impls",
        "coherent_trait",
        "orphan_check",
        "incoherent_impls",
        "type_of_opaque",
        "find_field",
        "instantiate_and_check_impossible",
        "is_copy_raw",
        "is_sized_raw",
        "is_freeze_raw",
        "is_unpin_raw",
        "needs_drop_raw",
        "has_significant_drop",
        "adt_drop_tys",
        "adt_significant_drop",
    ]) {
        "traits"
    } else if starts(&[
        "lint_",
        "lints_",
        "shallow_lint_levels",
        "check_mod_",
        "check_expectations",
        "effective_visibilities",
        "check_private_in_public",
        "live_symbols",
        "check_unused_traits",
        "check_mod_deathness",
        "unused_",
        "stability_",
        "lookup_stability",
        "lookup_const_stability",
        "lookup_deprecation",
        "check_externally_implementable",
        "entry_fn",
        "proc_macro_decls_static",
    ]) {
        "checks, lints"
    } else {
        "item sema"
    }
}

/// One stage's totals over the measured rounds, and its slots by time.
struct Row {
    nanos: u64,
    slots: Vec<(&'static str, u64, u64)>,
}

fn run(label: &str, files: &[String], rounds: usize, top: usize, width: usize) {
    let bytes: usize = files.iter().map(String::len).sum();
    let mb = bytes as f64 / 1e6;
    let lines: usize = files.iter().map(|f| f.lines().count()).sum();
    let files: Vec<Arc<String>> = files.iter().cloned().map(Arc::new).collect();
    for (i, f) in files.iter().enumerate() {
        let checked = check_shared_source_with_width("corpus", Arc::clone(f), width);
        assert!(
            checked.errors.is_empty() && !checked.fatal,
            "{label}: file {i} does not check clean: {:?}",
            &checked.errors[..checked.errors.len().min(3)]
        );
    }
    stage_timing::reset();
    let start = Instant::now();
    for _ in 0..rounds {
        for f in &files {
            std::hint::black_box(check_shared_source_with_width("corpus", Arc::clone(f), width));
        }
    }
    let wall = start.elapsed().as_secs_f64() / rounds as f64;
    let samples = stage_timing::snapshot();

    let mut rows: Vec<Row> = STAGES.iter().map(|_| Row { nanos: 0, slots: Vec::new() }).collect();
    let mut transitions = 0;
    for s in &samples {
        let at = STAGES.iter().position(|&st| st == stage_of(s.name, s.query)).unwrap();
        rows[at].nanos += s.nanos;
        rows[at].slots.push((s.name, s.nanos, s.entries));
        transitions += s.entries;
    }
    let recorded: u64 = rows.iter().map(|r| r.nanos).sum();
    let per_pass = |nanos: u64| nanos as f64 / rounds as f64 / 1e6;

    println!(
        "\n{label}: {} files, {lines} lines, {mb:.2} MB; {rounds} rounds at width {width}, one file at a time",
        files.len()
    );
    println!(
        "wall {:.1} ms per pass ({:.2} MB/s); worker-summed stage time {:.1} ms; {} stage transitions per pass",
        wall * 1e3,
        mb / wall,
        per_pass(recorded),
        transitions / rounds as u64
    );
    // Reference, outside the pipeline: `rustc_lexer` alone, tokens and nothing else (no tape,
    // spans, admission or interning), best of five passes over the same bytes.
    let raw_lex = (0..5)
        .map(|_| {
            let start = Instant::now();
            for f in &files {
                let tokens = frontend::rustc_lexer::tokenize(
                    f,
                    frontend::rustc_lexer::FrontmatterAllowed::No,
                );
                std::hint::black_box(tokens.count());
            }
            start.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min);
    println!(
        "reference: bare rustc_lexer::tokenize {:.2} ms per pass ({:.0} MB/s)",
        raw_lex * 1e3,
        mb / raw_lex
    );
    ALLOCS.store(0, Relaxed);
    ALLOC_BYTES.store(0, Relaxed);
    COUNTING.store(true, Relaxed);
    for f in &files {
        std::hint::black_box(check_shared_source_with_width("corpus", Arc::clone(f), width));
    }
    COUNTING.store(false, Relaxed);
    println!(
        "allocations: {} allocs, {:.1} MB allocated (untimed pass)",
        ALLOCS.load(Relaxed),
        ALLOC_BYTES.load(Relaxed) as f64 / 1e6,
    );
    println!(
        "{:<14} {:>9} {:>6} {:>11} {:>13}  top queries (ms per pass, calls per pass)",
        "stage", "ms", "share", "MB/s alone", "MB/s through"
    );
    let mut through = 0u64;
    for (stage, row) in STAGES.iter().zip(&mut rows) {
        if row.nanos == 0 {
            continue;
        }
        through += row.nanos;
        row.slots.sort_by_key(|s| std::cmp::Reverse(s.1));
        let tops: Vec<String> = row
            .slots
            .iter()
            .take(top)
            .map(|(name, nanos, entries)| {
                format!("{name} {:.1}/{}", per_pass(*nanos), entries / rounds as u64)
            })
            .collect();
        println!(
            "{stage:<14} {:>9.1} {:>5.1}% {:>11.1} {:>13.2}  {}",
            per_pass(row.nanos),
            row.nanos as f64 * 100.0 / recorded as f64,
            mb / (per_pass(row.nanos) / 1e3),
            mb / (per_pass(through) / 1e3),
            tops.join(", ")
        );
    }
}

fn main() {
    if !stage_timing::ENABLED {
        eprintln!("stage_throughput needs the `stage-timing` feature: --features stage-timing");
        std::process::exit(2);
    }
    frontend::unwind_janky::install_catcher(catcher);
    let rounds: usize = std::env::args().nth(1).and_then(|n| n.parse().ok()).unwrap_or(3);
    let width = std::env::var("WIDTH")
        .ok()
        .map(|value| value.parse::<usize>().expect("WIDTH must be a positive integer"))
        .unwrap_or(1);
    assert!(width > 0, "WIDTH must be a positive integer");
    println!("stage benchmark width {width}, parallel feature: {}", cfg!(feature = "parallel"));
    let top: usize = std::env::var("TOP").ok().and_then(|n| n.parse().ok()).unwrap_or(3);
    let only = std::env::var("ONLY").ok();
    let wants = |name: &str| only.as_deref().is_none_or(|o| o == name);
    if wants("clean") {
        run("clean", &generated('c', 200, 300, 1_500, 0x5eed_0001), rounds, top, width);
    }
    if wants("large") {
        run("large", &generated('l', 6, 5_000, 8_000, 0x5eed_0002), rounds, top, width);
    }
}
