//! Compare the parser with all S2 fast paths disabled and enabled.
//!
//! Run with `cargo run --release --example parse_oracle -- [DIR...]`. The generated clean and
//! large corpora are always included. Directory arguments add every readable `.rs` file.

#[path = "support/corpus.rs"]
mod corpus;

use std::sync::Arc;

use frontend::rustc_parse::parse_hooks;
use frontend::rustc_parse::parser::oracle::{self, Mode, Site};
use frontend::rustc_span::edition::Edition;

#[derive(Default)]
struct Tally {
    checked: usize,
    same: usize,
    different: Vec<String>,
}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn main() {
    frontend::unwind_janky::install_catcher(catcher);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let expected = info
            .payload()
            .downcast_ref::<&str>()
            .is_some_and(|message| *message == "rustc fatal error (compilation refused)")
            || info
                .payload()
                .downcast_ref::<String>()
                .is_some_and(|message| message == "rustc fatal error (compilation refused)");
        if !expected {
            previous_hook(info);
        }
    }));
    let mut files: Vec<(String, String)> = Vec::new();
    for (index, source) in corpus::clean().into_iter().enumerate() {
        files.push((format!("clean/{index}"), source));
    }
    for (index, source) in corpus::large().into_iter().enumerate() {
        files.push((format!("large/{index}"), source));
    }
    for directory in std::env::args().skip(1) {
        for (path, source) in corpus::rust_files(std::path::Path::new(&directory)) {
            files.push((path.display().to_string(), source));
        }
    }

    for (index, source) in corpus::oracle_witnesses().into_iter().enumerate() {
        files.push((format!("witness/{index}"), source));
    }
    let original_count = files.len();

    // E0 witness run. Each entry point records its own shadow comparison result. The corpus is
    // deliberately the canonical clean and large set so missing instrumentation is visible.
    oracle::reset_counts();
    for (index, source) in corpus::clean().into_iter().chain(corpus::oracle_witnesses()).enumerate()
    {
        let record =
            parse_hooks::parse_in_mode(Arc::new(source), Edition::Edition2021, Mode::Shadow);
        assert_eq!(record.status, "ok", "E0 clean/{index} failed: {record:?}");
    }
    for (index, source) in corpus::large().into_iter().enumerate() {
        let record =
            parse_hooks::parse_in_mode(Arc::new(source), Edition::Edition2021, Mode::Shadow);
        assert_eq!(record.status, "ok", "E0 large/{index} failed: {record:?}");
    }
    let counts = oracle::snapshot_counts();
    let sites = [
        (Site::Path, "Path", counts.path),
        (Site::Ty, "Ty", counts.ty),
        (Site::Pat, "Pat", counts.pat),
        (Site::FnParams, "FnParams", counts.fn_params),
        (Site::ExprAtom, "ExprAtom", counts.expr_atom),
        (Site::LetStmt, "LetStmt", counts.let_stmt),
    ];
    println!("E0 shadow counts: {counts:?}");
    for failure in oracle::shadow_failures() {
        eprintln!("SHADOW {failure}");
    }
    let mut shadow_failed = false;
    for (_, name, site) in sites {
        if site.hits == 0 || site.shadow_ok == 0 || site.shadow_mismatches != 0 {
            eprintln!("E0 FAIL {name}: {site:?}");
            shadow_failed = true;
        }
    }
    let mut cases = files.clone();
    for (name, source) in &files {
        for mutation in mutations(source) {
            cases.push((format!("{name}{}", mutation.suffix), mutation.source));
        }
    }

    let editions = [Edition::Edition2015, Edition::Edition2021, Edition::Edition2024];
    let mut total = Tally::default();
    for edition in editions {
        let mut tally = Tally::default();
        for (case_index, (name, source)) in cases.iter().enumerate() {
            if case_index % 512 == 0 {
                eprintln!("E1 {edition:?}: {case_index}/{}", cases.len());
            }
            let (case_name, seed, offset) = mutation_detail(name);
            let compared = parse_hooks::compare(Arc::new(source.clone()), edition);
            tally.checked += 1;
            if let Some(detail) = &compared.detail {
                eprintln!(
                    "DIFF file={case_name} edition={edition:?} seed={seed} offset={offset}: {detail}"
                );
            }
            match compared.detail {
                None => tally.same += 1,
                Some(detail) => tally.different.push(format!(
                    "file={case_name} edition={edition:?} seed={seed} offset={offset}: {detail}"
                )),
            }
        }
        println!(
            "{edition:?}: {} cases ({} originals, {} mutations each), {} equal, {} different",
            tally.checked,
            original_count,
            20,
            tally.same,
            tally.different.len()
        );
        for difference in &tally.different {
            eprintln!("DIFF {difference}");
        }
        total.checked += tally.checked;
        total.same += tally.same;
        total.different.extend(tally.different);
    }
    println!(
        "total: {} parses, {} equal, {} different",
        total.checked,
        total.same,
        total.different.len()
    );
    if shadow_failed || !total.different.is_empty() {
        std::process::exit(1);
    }
}

struct Mutation {
    suffix: String,
    source: String,
}

/// Twenty independently seeded mutations: five each of deletion, insertion, replacement and
/// truncation. Every chosen byte offset is a UTF-8 boundary. Payloads deliberately include
/// separators, quotes, identifiers, attributes/docs, operators and malformed endings.
fn mutations(source: &str) -> Vec<Mutation> {
    let boundaries: Vec<usize> = source
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(source.len()))
        .collect();
    let payloads = [";", ",", "\"", "mut ", "#[]\n", "/// doc\n", "::", "=>", "(", "}"];
    let mut result = Vec::with_capacity(20);
    for operator in 0..4 {
        for round in 0..5 {
            let mutation_index = operator * 5 + round;
            let seed = seed_for(source.as_bytes(), mutation_index);
            let mut rng = Lcg(seed);
            let boundary_index = rng.below(boundaries.len());
            let offset = boundaries[boundary_index];
            let next = boundaries.get(boundary_index + 1).copied().unwrap_or(offset);
            let payload = payloads[rng.below(payloads.len())];
            let mut bytes = source.as_bytes().to_vec();
            match operator {
                0 => {
                    if next > offset {
                        bytes.drain(offset..next);
                    }
                }
                1 => {
                    bytes.splice(offset..offset, payload.bytes());
                }
                2 => {
                    if next > offset {
                        bytes.splice(offset..next, payload.bytes());
                    } else {
                        bytes.splice(offset..offset, payload.bytes());
                    }
                }
                _ => bytes.truncate(offset),
            };
            let mutated = String::from_utf8(bytes).expect("mutations preserve UTF-8");
            let op = ["delete", "insert", "replace", "truncate"][operator];
            result.push(Mutation {
                suffix: format!("/mutation-{mutation_index}-{op}-seed-{seed:016x}-offset-{offset}"),
                source: mutated,
            });
        }
    }
    result
}

fn mutation_detail(name: &str) -> (&str, &str, &str) {
    match name.rsplit_once("/mutation-") {
        Some((file, detail)) => {
            let mut parts = detail.split('-');
            let _index = parts.next();
            let _operator = parts.next();
            let _seed_label = parts.next();
            let seed = parts.next().unwrap_or("unknown");
            let _offset_label = parts.next();
            let offset = parts.next().unwrap_or("unknown");
            (file, seed, offset)
        }
        None => (name, "none", "none"),
    }
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, upper: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 32) as usize % upper
    }
}

fn seed_for(bytes: &[u8], mutation: usize) -> u64 {
    let mut hash = 0xcbf29ce484222325u64 ^ mutation as u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
