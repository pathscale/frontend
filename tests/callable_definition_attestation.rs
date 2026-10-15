use frontend::frontend_facts::effects::StaticFact;
use frontend::frontend_facts::{
    CrateRead, Dependency, Loaded, ResolvedCallableKind, analyze_body_type_identity_with_loaded,
    read_crate,
};
use frontend::rustc_data_structures::sync::FreezeLock;
use frontend::rustc_span::def_id::CrateNum;
use frontend::rustc_span::source_map::{FilePathMapping, SourceMap};
use frontend::rustc_span::{FileName, Pos, Span, SpanSnippetError};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

fn range(source: &str, text: &str) -> frontend::frontend_facts::effects::TextRange {
    let start = source.find(text).expect("source range") as u32;
    frontend::frontend_facts::effects::TextRange { start, end: start + text.len() as u32 }
}

fn range_last(source: &str, text: &str) -> frontend::frontend_facts::effects::TextRange {
    let start = source.rfind(text).expect("last source range") as u32;
    frontend::frontend_facts::effects::TextRange { start, end: start + text.len() as u32 }
}

fn blake3_digest(source: &[u8]) -> String {
    format!("blake3={}", blake3::hash(source).to_hex())
}

struct Scratch {
    dir: std::path::PathBuf,
    owned_files: Vec<std::path::PathBuf>,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        std::fs::create_dir_all(&target_dir).expect("create or find Cargo target directory");
        let target_dir = target_dir.canonicalize().expect("canonical Cargo target directory");
        let scratch_parent = target_dir.join("tmp");
        std::fs::create_dir_all(&scratch_parent)
            .expect("create test scratch parent under Cargo target");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        for attempt in 0u32.. {
            let path = scratch_parent.join(format!(
                "callable-definition-attestation-{name}-{}-{nanos}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    return Self { dir: path, owned_files: Vec::new() };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create owned test scratch directory: {error}"),
            }
        }
        unreachable!("an unbounded unique-directory counter is exhausted")
    }

    fn write(&mut self, name: &str, source: &str) -> String {
        let path = self.owned_path(name);
        std::fs::write(&path, source).expect("write test source");
        path.to_str().expect("test path is UTF-8").to_owned()
    }

    fn owned_path(&mut self, name: &str) -> std::path::PathBuf {
        let path = self.dir.join(name);
        self.owned_files.push(path.clone());
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.owned_files {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

const MINIMAL_LANG_ITEMS: &str = "\
#![feature(no_core, lang_items)]
#![allow(internal_features)]
#![no_core]
#[lang = \"pointee_sized\"] pub trait PointeeSized {}
#[lang = \"meta_sized\"] pub trait MetaSized: PointeeSized {}
#[lang = \"sized\"] pub trait Sized: MetaSized {}
#[lang = \"copy\"] pub trait Copy {}
#[lang = \"legacy_receiver\"] pub trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl Copy for u32 {}
";

fn loaded_source_attestation_dependency(
    name: &str,
) -> (Scratch, [Dependency; 1], std::path::PathBuf) {
    let mut scratch = Scratch::new(name);
    let root = scratch.write("lib.rs", &format!("{MINIMAL_LANG_ITEMS}\npub mod target;\n"));
    let target_source = "pub fn target() -> bool { true }\n";
    let target_path = scratch.write("target.rs", target_source);
    let metadata = scratch.owned_path(&format!("lib{name}.rmeta"));
    let metadata = metadata.to_str().expect("metadata path is UTF-8").to_owned();
    let facts = read_crate(
        &CrateRead {
            edition: Some("2024"),
            items_only: true,
            write_metadata: Some(eko::path::Path::new(&metadata)),
            ..CrateRead::new(name, eko::path::Path::new(&root))
        }
        .with_disambiguator("callable-source-hash-control-v1"),
    )
    .expect("write source-attested metadata with this frontend build");
    assert!(facts.complete, "test dependency facts are complete: {:?}", facts.diagnostics);
    assert!(std::path::Path::new(&metadata).is_file(), "metadata was written");
    (scratch, [Dependency::new(name, metadata)], std::path::PathBuf::from(target_path))
}

fn loaded_minimal_lang_items(name: &str) -> (Scratch, [Dependency; 1]) {
    let mut scratch = Scratch::new(name);
    let root = scratch.write("minimal_lang.rs", MINIMAL_LANG_ITEMS);
    let metadata = scratch.owned_path(&format!("lib{name}.rmeta"));
    let metadata = metadata.to_str().expect("metadata path is UTF-8").to_owned();
    read_crate(&CrateRead {
        edition: Some("2024"),
        items_only: true,
        write_metadata: Some(eko::path::Path::new(&metadata)),
        ..CrateRead::new(name, eko::path::Path::new(&root))
    })
    .expect("write self-owned minimal no_core metadata");
    assert!(std::path::Path::new(&metadata).is_file(), "metadata was written");
    let dependencies = [Dependency::new(name, metadata)];
    (scratch, dependencies)
}

fn normalized_source_from_source_map(source: &str) -> String {
    let source_map = SourceMap::for_text(FilePathMapping::empty());
    let file = source_map.new_source_file(FileName::anon_source_code(source), source.to_owned());
    let full_file = Span::with_root_ctxt(file.start_pos, file.end_position());
    source_map
        .span_to_snippet(full_file)
        .expect("read normalized fixture bytes through rustc SourceMap")
}

#[test]
fn direct_call_identity_is_bound_to_exact_source_file_and_definition_bytes() {
    ready();
    let (_scratch, dependencies) = loaded_minimal_lang_items("callable_lang_direct");
    let source = "\u{feff}#![feature(no_core)]\r\n#![no_core]\r\nextern crate callable_lang_direct;\r\nfn target() -> bool { true }\r\nfn caller() -> bool { target() }\r\n";
    let source_map_source = normalized_source_from_source_map(source);
    assert!(source.starts_with('\u{feff}') && source.contains("\r\n"));
    assert!(!source_map_source.starts_with('\u{feff}') && !source_map_source.contains("\r\n"));
    let report = analyze_body_type_identity_with_loaded(
        "callable_definition_attestation",
        source,
        range(source, "{ target() }"),
        Some("2024"),
        Loaded { dependencies: &dependencies, ..Loaded::default() },
    );
    let call_span = range_last(source, "target()");
    let call = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == call_span.start && fact.span.end == call_span.end)
        .expect("source call identity fact");
    let Some(StaticFact::Known { value: Some(callable) }) = call.resolved_callable.as_ref() else {
        panic!("compiler did not resolve direct call: {call:?}");
    };
    let StaticFact::Known { value: attestation } = &callable.source_attestation else {
        panic!("source definition was not attested: {callable:?}");
    };

    assert_eq!(callable.definition, attestation.definition);
    assert_eq!(attestation.source_digest_scheme, "rustc_normalized_source_blake3_v1");
    assert_eq!(attestation.source_content_digest, blake3_digest(source_map_source.as_bytes()));

    let item = source_map_source
        .get(attestation.item_span.start as usize..attestation.item_span.end as usize)
        .expect("attested item span is a UTF-8 byte range");
    assert!(item.contains("fn target() -> bool { true }"), "{item:?}");
    assert_eq!(attestation.item_digest, blake3_digest(item.as_bytes()));
}

#[test]
fn macro_generated_callable_identity_does_not_claim_source_attestation() {
    ready();
    let (_scratch, dependencies) = loaded_minimal_lang_items("callable_lang_macro");
    let source = "#![feature(no_core)]\n#![no_core]\nextern crate callable_lang_macro;\nmacro_rules! make_target { () => { fn target() -> bool { true } } }\nmake_target!();\nfn caller() -> bool { target() }\n";
    let report = analyze_body_type_identity_with_loaded(
        "macro_callable_definition_attestation",
        source,
        range(source, "{ target() }"),
        Some("2024"),
        Loaded { dependencies: &dependencies, ..Loaded::default() },
    );
    let call_span = range_last(source, "target()");
    let call = report
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == call_span.start && fact.span.end == call_span.end)
        .expect("source call identity fact");
    let Some(StaticFact::Known { value: Some(callable) }) = call.resolved_callable.as_ref() else {
        panic!("compiler did not resolve macro-generated function call: {call:?}");
    };
    assert_eq!(callable.kind, ResolvedCallableKind::DirectItem);
    assert!(
        matches!(
            &callable.source_attestation,
            StaticFact::Unknown { why } if why == "definition_from_expansion"
        ),
        "authored caller must resolve the macro-expanded target, whose source-map provenance cannot be attested: {callable:?}"
    );
}

#[test]
fn changed_test_owned_dependency_source_fails_the_metadata_hash_gate() {
    ready();
    let (_scratch, dependencies, target_path) =
        loaded_source_attestation_dependency("callable_source_hash_gate");
    let source = "#![feature(no_core)]\n#![no_core]\nextern crate callable_source_hash_gate;\nfn caller() -> bool { callable_source_hash_gate::target::target() }\n";
    let body = range(source, "{ callable_source_hash_gate::target::target() }");
    let analyze = || {
        analyze_body_type_identity_with_loaded(
            "changed_test_owned_dependency_source",
            source,
            body,
            Some("2024"),
            Loaded { dependencies: &dependencies, ..Loaded::default() },
        )
    };
    let call_range = range_last(source, "callable_source_hash_gate::target::target()");
    let before = analyze();
    let before_call = before
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == call_range.start && fact.span.end == call_range.end)
        .expect("external call identity before changing its source");
    let Some(StaticFact::Known { value: Some(callable) }) = before_call.resolved_callable.as_ref()
    else {
        panic!("compiler did not resolve external function call: {before_call:?}");
    };
    let original = std::fs::read_to_string(&target_path).expect("read owned dependency source");
    assert_eq!(original, "pub fn target() -> bool { true }\n");
    let StaticFact::Known { value: attestation } = &callable.source_attestation else {
        panic!("unchanged source bytes were not attested: {callable:?}");
    };
    assert_eq!(attestation.source_content_digest, blake3_digest(original.as_bytes()));

    std::fs::write(&target_path, "pub fn target() -> bool { false }\n")
        .expect("mutate only the isolated test-owned source fixture");

    let after = analyze();
    let after_call = after
        .identity_facts
        .expressions
        .iter()
        .find(|fact| fact.span.start == call_range.start && fact.span.end == call_range.end)
        .expect("external call identity after changing its source");
    let Some(StaticFact::Known { value: Some(callable) }) = after_call.resolved_callable.as_ref()
    else {
        panic!(
            "compiler stopped resolving the external function after source mutation: {after_call:?}"
        );
    };
    assert!(
        matches!(
            &callable.source_attestation,
            StaticFact::Unknown { why } if why == "definition_source_unavailable_or_unverified"
        ),
        "changed source bytes must not satisfy the source hash embedded in metadata: {callable:?}"
    );
}

#[test]
fn imported_source_without_metadata_hash_cannot_be_used_as_source_evidence() {
    let mut scratch = Scratch::new("legacy-source-hash");
    let source_path = scratch.write("legacy.rs", "pub fn target() -> bool { true }\n");
    let source_map = SourceMap::new(FilePathMapping::empty());
    let source_file = source_map
        .load_file(eko::path::Path::new(&source_path))
        .expect("load the source file bytes");
    assert!(source_file.src_hash.is_none(), "this models legacy metadata without a source hash");

    let frontend::rustc_span::SourceFile {
        name,
        checksum_hash,
        normalized_source_len,
        unnormalized_source_len,
        lines,
        multibyte_chars,
        normalized_pos,
        stable_id,
        ..
    } = (*source_file).clone();
    let imported = source_map.new_imported_source_file(
        name,
        None,
        checksum_hash,
        stable_id,
        normalized_source_len.to_u32(),
        unnormalized_source_len,
        CrateNum::ZERO,
        FreezeLock::new(lines.read().clone()),
        multibyte_chars,
        normalized_pos,
        0,
    );
    let span = Span::with_root_ctxt(imported.start_pos, imported.end_position());
    assert!(
        matches!(
            source_map.span_to_snippet(span),
            Err(SpanSnippetError::SourceNotAvailable { .. })
        ),
        "a readable path without the metadata hash must remain unavailable to attestation"
    );
}
