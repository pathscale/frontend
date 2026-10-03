use frontend::frontend_facts::{FactOrigin, Impl, analyze_source, check_source};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

#[test]
fn old_impl_facts_default_to_written_origin() {
    let json = r#"{"def_path":"crate::impl S","self_type":"S","trait_def_path":null,"span":{"file":"lib.rs","start":0,"end":0}}"#;
    let implementation: Impl = serde_json::from_str(json).expect("legacy impl fact");
    assert_eq!(implementation.origin, FactOrigin::Written);
}

#[test]
fn impl_facts_keep_their_origin_and_opaque_bodies_are_named() {
    ready();
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let source = r#"
#![feature(no_core, lang_items, rustc_attrs, decl_macro)]
#![allow(internal_features)]
#![no_core]

#[lang = "pointee_sized"] pub trait PointeeSized {}
#[lang = "meta_sized"] pub trait MetaSized: PointeeSized {}
#[lang = "sized"] pub trait Sized: MetaSized {}
#[lang = "legacy_receiver"] pub trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl<T: ?Sized> LegacyReceiver for &mut T {}
#[lang = "clone"] pub trait Clone: Sized {
    #[lang = "clone_fn"] fn clone(&self) -> Self;
}
#[lang = "copy"] pub trait Copy: Clone {}
pub mod clone { pub use super::Clone; }
extern crate self as core;
#[rustc_builtin_macro] pub macro derive($item:item) {}
#[rustc_builtin_macro] pub macro Clone($item:item) {}
#[rustc_builtin_macro] pub macro compile_error($msg:expr $(,)?) {}

pub struct S;
impl S {}
#[derive(Clone)]
pub struct Derived;
impl Copy for Derived {}
fn opaque_body() { compile_error!("opaque: body"); }
"#;
    let facts = analyze_source("facts_origin", source).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let clean_source = source.replace(
        "fn opaque_body() { compile_error!(\"opaque: body\"); }\n",
        "",
    );
    let clean_facts = analyze_source("facts_origin_clean", &clean_source)
        .expect("source without opaque body parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let implementation = facts
        .impls
        .iter()
        .find(|item| item.self_type == "S")
        .expect("impl S");
    assert_eq!(implementation.origin, FactOrigin::Written);
    let derived = clean_facts
        .impls
        .iter()
        .find(|item| {
            item.self_type == "Derived"
                && item.items.iter().any(|item| item.name == "clone")
        })
        .expect("derived impl");
    assert_eq!(derived.origin, FactOrigin::Builtin);
    let body = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "opaque_body")
        .expect("opaque_body");
    assert!(body.opaque.iter().any(|part| part == "body"));
}

#[test]
fn const_terms_are_reported_on_crate_facts() {
    ready();
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let source = r#"
#![feature(no_core, lang_items)]
#![allow(internal_features)]
#![no_core]

#[lang = "pointee_sized"] pub trait PointeeSized {}
#[lang = "meta_sized"] pub trait MetaSized: PointeeSized {}
#[lang = "sized"] pub trait Sized: MetaSized {}
#[lang = "legacy_receiver"] pub trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl<T: ?Sized> LegacyReceiver for &mut T {}
#[lang = "clone"] pub trait Clone: Sized {
    #[lang = "clone_fn"] fn clone(&self) -> Self;
}
#[lang = "copy"] pub trait Copy: Clone {}
impl Clone for u8 { fn clone(&self) -> u8 { *self } }
impl Clone for usize { fn clone(&self) -> usize { *self } }
impl Copy for u8 {}
impl Copy for usize {}
#[lang = "mul"] pub trait Mul<Rhs = Self> { type Output; fn mul(self, rhs: Rhs) -> Self::Output; }
impl Mul for usize { type Output = usize; fn mul(self, _rhs: usize) -> usize { 0 } }

pub const N: usize = 4;
pub const M: usize = N * 2;
pub fn compares_length() { let _: [u8; M] = [0; 8]; }
"#;
    let facts = analyze_source("facts_consts", source).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    assert!(
        facts.uncomputed_consts.iter().any(|path| path.ends_with("M")),
        "{:?}",
        facts.uncomputed_consts
    );

    let checked = check_source("facts_consts_check", source);
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    assert!(
        checked.uncomputed_consts.iter().any(|path| path.ends_with("M")),
        "{:?}",
        checked.uncomputed_consts
    );
}

#[test]
fn type_diagnostics_keep_suggestions_as_edits() {
    ready();
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let source = r#"
#![feature(no_core, lang_items)]
#![allow(internal_features)]
#![no_core]
#[lang = "pointee_sized"] trait PointeeSized {}
#[lang = "meta_sized"] trait MetaSized: PointeeSized {}
#[lang = "sized"] trait Sized: MetaSized {}
#[lang = "legacy_receiver"] trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl<T: ?Sized> LegacyReceiver for &mut T {}
struct Thing;
impl Thing { fn hello(&self) {} }
fn call(value: Thing) { value.hellp(); }
"#;
    let facts = analyze_source("facts_suggestions", source).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let suggestion = facts
        .typed
        .iter()
        .flat_map(|diagnostic| diagnostic.suggestions.iter())
        .find(|suggestion| suggestion.parts.iter().any(|part| part.replacement == "hello"))
        .expect("similar method name suggestion");
    assert!(!suggestion.message.is_empty());
    assert!(suggestion.parts.iter().any(|part| part.span_known));
}

#[test]
fn full_docs_sections_and_examples_are_kept_as_text() {
    ready();
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let source = r#"
/// Summary line.
/// More detail.
///
/// # Usage
/// A short example.
/// ```rust
/// let answer = 42;
/// ```
///
/// >>> let answer = 42;
/// 42
pub fn guide() {}

pub struct Record {
    /// Field summary.
    ///
    /// # Details
    /// Field detail.
    value: u8,
}
"#;
    let facts = analyze_source("facts_docs", source).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);

    let guide = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "guide")
        .expect("guide");
    assert_eq!(guide.doc.as_deref(), Some("Summary line."));
    assert!(guide.doc_full.as_deref().is_some_and(|text| text.contains("More detail.")));
    let usage = guide
        .doc_sections
        .iter()
        .find(|section| section.heading == "Usage")
        .expect("Usage section");
    assert!(usage.body.contains("A short example."));
    assert!(guide.examples.iter().any(|example| {
        example.kind == frontend::frontend_facts::DocExampleKind::Fenced
            && example.text.contains("let answer = 42;")
    }));
    assert!(guide.examples.iter().any(|example| {
        example.kind == frontend::frontend_facts::DocExampleKind::Doctest
            && example.text.contains(">>> let answer = 42;")
    }));

    let record = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "Record")
        .expect("Record");
    assert_eq!(record.fields[0].doc.as_deref(), Some("Field summary."));
    assert!(record.fields[0].doc_full.as_deref().is_some_and(|text| text.contains("Field detail.")));
    assert!(record.fields[0].doc_sections.iter().any(|section| section.heading == "Details"));
}

#[test]
fn assertions_with_a_local_same_spelling_are_not_misidentified() {
    ready();
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let source = r#"
macro_rules! assert_eq {
    ($left:expr, $right:expr) => {{ let _ = ($left, $right); }};
}
fn call() { assert_eq!(1, 1); }
"#;
    let facts = analyze_source("facts_asserts", source).expect("source parses");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), 0);
    let call = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "call")
        .expect("call");
    assert!(call.asserts.is_empty());
}
