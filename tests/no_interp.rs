use std::sync::Arc;

use frontend::frontend_facts::{
    CrateFacts, CrateRead, Loaded, analyze_source, check_source_against, read_crate,
};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

const SOURCE: &str = r#"
#![feature(no_core, lang_items, auto_traits)]
#![allow(internal_features)]
#![no_core]

#[lang = "pointee_sized"] pub trait PointeeSized {}
#[lang = "meta_sized"] pub trait MetaSized: PointeeSized {}
#[lang = "sized"] pub trait Sized: MetaSized {}
#[lang = "sync"] pub unsafe auto trait Sync {}
#[lang = "clone"] pub trait Clone: Sized {
    #[lang = "clone_fn"] fn clone(&self) -> Self;
}
#[lang = "copy"] pub trait Copy: Clone {}
#[lang = "legacy_receiver"] pub trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl Clone for u8 { fn clone(&self) -> u8 { *self } }
impl Clone for usize { fn clone(&self) -> usize { *self } }
impl Copy for u8 {}
impl Copy for usize {}
#[lang = "future_trait"] pub trait Future { type Output; }
#[lang = "mul"] pub trait Mul<Rhs = Self> { type Output; fn mul(self, rhs: Rhs) -> Self::Output; }
impl Mul for usize { type Output = usize; fn mul(self, _rhs: usize) -> usize { 0 } }

pub const N: usize = 4;
pub const M: usize = N * 2;
pub const M_SAME: usize = N * 2;
pub const LITERAL: [u8; 2] = [1, 2];
pub struct UsesConst { pub values: [u8; N] }
pub struct S<const N: usize> { pub values: [u8; N] }
pub const fn const_fn() -> u8 { 7 }
pub static STATIC: u8 = 7;
"#;

const DEFINITIONS: [&str; 8] = [
    "N", "M", "M_SAME", "LITERAL", "UsesConst", "S", "const_fn", "STATIC",
];

fn assert_definitions(facts: &CrateFacts) {
    let names: Vec<&str> = facts
        .definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    for name in DEFINITIONS {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }
}

fn source_without_array_const() -> String {
    // Array constant values need interpreter allocations, which this frontend deliberately omits.
    SOURCE.replace("pub const LITERAL: [u8; 2] = [1, 2];\n", "")
}

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("frontend-no-interp-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create a temp directory");
        Self(path)
    }

    fn file(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn analysis_entry_points_do_not_create_interpreter_contexts() {
    frontend::unwind_janky::install_catcher(catcher);
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let analyzed = analyze_source("no_interp", SOURCE).expect("source parses");
    assert_definitions(&analyzed);
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let scratch = Scratch::new();
    let source_path_buf = scratch.file("fixture.rs");
    std::fs::write(&source_path_buf, SOURCE).expect("write the source fixture");
    let source_path = source_path_buf.to_str().expect("a UTF-8 fixture path");

    let items = read_crate(&CrateRead {
        edition: Some("2021"),
        items_only: true,
        library: true,
        ..CrateRead::new("no_interp", eko::path::Path::new(source_path))
    })
    .expect("items-only read succeeds");
    assert_definitions(&items);
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let _checked = check_source_against(
        "no_interp",
        Arc::new(SOURCE.to_string()),
        Some("2021"),
        Loaded::default(),
        1,
        false,
    );
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let metadata_path_buf = scratch.file("fixture.rmeta");
    let metadata_path = metadata_path_buf.to_str().expect("a UTF-8 metadata path");
    let written = read_crate(&CrateRead {
        edition: Some("2021"),
        library: true,
        write_metadata: Some(eko::path::Path::new(metadata_path)),
        ..CrateRead::new("no_interp", eko::path::Path::new(source_path))
    })
    .expect("metadata read succeeds");
    assert_definitions(&written);
    assert!(std::path::Path::new(metadata_path).is_file());
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
}

#[test]
fn symbolic_const_terms_do_not_fold_or_interpret() {
    frontend::unwind_janky::install_catcher(catcher);
    let symbolic_source = format!(
        "{}\n\
         fn takes_four(_: [u8; 4]) {{}}\n\
         fn takes_eight(_: [u8; 8]) {{}}\n\
         fn relates_symbolic_consts() {{\n\
             takes_four([0; N]);\n\
             let _: [u8; M_SAME] = [0; M];\n\
             takes_eight([0; M]);\n\
         }}\n",
        source_without_array_const()
    );
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let facts = frontend::frontend_facts::analyze_source("symbolic_consts", &symbolic_source)
        .expect("symbolic const source parses");
    assert!(
        facts.uncomputed_consts.iter().any(|path| path.ends_with("M")),
        "{:?}",
        facts.uncomputed_consts
    );
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    // Const expressions remain named as unknown; use a literal mismatch for the type diagnostic.
    let checked_source = format!(
        "{}\n\
         fn takes_eight(_: [u8; 8]) {{}}\n\
         fn known_length_mismatch() {{ takes_eight([0; 7]); }}\n",
        source_without_array_const()
    );
    let checked = check_source_against(
        "symbolic_consts",
        Arc::new(checked_source),
        Some("2021"),
        Loaded::default(),
        1,
        false,
    );
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let errors: Vec<_> = checked
        .typed
        .iter()
        .filter(|diagnostic| diagnostic.severity == frontend::frontend_facts::Severity::Error)
        .collect();
    assert_eq!(errors.len(), 1, "{:?}", checked.typed);
    assert_eq!(errors[0].code.as_deref(), Some("E0308"));
}

#[test]
fn typed_diagnostic_keeps_the_call_span_without_interpreting_consts() {
    frontend::unwind_janky::install_catcher(catcher);
    let source = format!(
        "{}\npub fn takes_one(_value: u8) {{}}\npub fn calls_it() {{ takes_one(); }}\n",
        source_without_array_const()
    );
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let checked = check_source_against(
        "typed_diagnostic",
        Arc::new(source.clone()),
        Some("2021"),
        Loaded::default(),
        1,
        false,
    );

    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    assert_eq!(checked.errors.len(), 1, "{:?}", checked.errors);
    let errors: Vec<_> = checked
        .typed
        .iter()
        .filter(|diagnostic| diagnostic.severity == frontend::frontend_facts::Severity::Error)
        .collect();
    assert_eq!(errors.len(), 1, "{:?}", checked.typed);
    let diagnostic = errors[0];
    assert!(diagnostic.span_known);
    assert!(diagnostic.span.start < diagnostic.span.end);
    assert_eq!(
        &source[diagnostic.span.start as usize..diagnostic.span.end as usize],
        "takes_one()"
    );
    assert_eq!(diagnostic.code.as_deref(), Some("E0061"));
    assert!(checked.errors[0].contains("E0061"));
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
}
