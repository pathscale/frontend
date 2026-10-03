//! `frontend_facts::syntax`, driven from outside the crate.
//!
//! Out here because the parser needs a catcher, and a catcher needs `std`, which the library
//! does not name. This file is a `std` program like `src/bin/frontend_facts.rs`, so it installs
//! `std::panic::catch_unwind` the same way. None of it needs a sysroot: parsing reads only the
//! text it is handed.

use frontend::frontend_facts::syntax::{
    Fragment, parses as frontend_parses, parses_as as frontend_parses_as,
};
use frontend::rustc_expand::proc_macro_schema::{
    ExactPart, ProcMacroSchema, SchemaHole, analyze_source,
};
use frontend::rustc_session::parse::ParseSess;

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// Install the catcher. Every test calls it, because the harness runs them in any order and on
/// any thread; installing twice is harmless.
fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

fn no_interp<T>(analyze: impl FnOnce() -> T) -> T {
    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let result = analyze();
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    result
}

fn parses(source: &str) -> Result<(), Vec<String>> {
    no_interp(|| frontend_parses(source))
}

fn parses_as(source: &str, kind: Fragment) -> Result<(), Vec<String>> {
    no_interp(|| frontend_parses_as(source, kind))
}

fn refused(source: &str, kind: Fragment) -> Vec<String> {
    match parses_as(source, kind) {
        Ok(()) => panic!("{source:?} parsed as {kind:?} and should not have"),
        Err(errors) => errors,
    }
}

fn schema(source: &str, parameters: &[String]) -> ProcMacroSchema {
    no_interp(|| {
        frontend::rustc_span::create_session_if_not_set_then(
            frontend::rustc_span::edition::Edition::Edition2024,
            |_| analyze_source(&ParseSess::new(), source, parameters, false),
        )
    })
}

#[test]
fn a_binary_operation_is_an_expression() {
    ready();
    assert_eq!(parses_as("1 + 2", Fragment::Expr), Ok(()));
}

#[test]
fn a_braced_body_is_a_block() {
    ready();
    assert_eq!(parses_as("{ let x = 1; x }", Fragment::Block), Ok(()));
}

#[test]
fn a_function_is_an_item() {
    ready();
    assert_eq!(parses_as("fn f() {}", Fragment::Item), Ok(()));
}

#[test]
fn a_dangling_operator_is_not_an_expression() {
    ready();
    let errors = refused("1 +", Fragment::Expr);
    assert!(!errors.is_empty());
    assert!(errors.iter().all(|e| e.starts_with("error")), "{errors:?}");
}

#[test]
fn a_function_is_not_an_expression() {
    ready();
    assert!(!refused("fn f() {}", Fragment::Expr).is_empty());
}

#[test]
fn trailing_text_is_refused() {
    ready();
    assert!(!refused("1 + 2 3", Fragment::Expr).is_empty());
    assert!(!refused("fn f() {} fn g() {}", Fragment::Item).is_empty());
}

#[test]
fn the_other_kinds() {
    ready();
    assert_eq!(parses_as("let x = 1;", Fragment::Stmt), Ok(()));
    assert_eq!(parses_as("Vec<Option<u8>>", Fragment::Type), Ok(()));
    assert_eq!(parses_as("Some(x) | None", Fragment::Pat), Ok(()));
    assert_eq!(parses("use a::b;\nstruct S;\nfn f() {}\n"), Ok(()));
    assert_eq!(parses(""), Ok(()));
}

#[test]
fn unbalanced_delimiters_are_refused_while_lexing() {
    ready();
    assert!(!refused("fn f( {", Fragment::Items).is_empty());
}

#[test]
fn repeated_calls_on_one_thread_are_independent() {
    ready();
    for _ in 0..3 {
        assert!(parses_as("1 +", Fragment::Expr).is_err());
        assert_eq!(parses_as("1 + 2", Fragment::Expr), Ok(()));
    }
}

#[test]
fn a_quote_template_without_holes_is_exact() {
    ready();
    let source = "fn generated() -> TokenStream { quote! { struct Marker; } }";
    assert_eq!(parses(source), Ok(()));
    let ProcMacroSchema::Template { rules } = schema(source, &[]) else {
        panic!("expected a quote template");
    };
    assert_eq!(rules.len(), 1);
    assert!(rules[0].holes.is_empty());
    assert!(rules[0].rhs.source.contains("struct Marker;"));
}

#[test]
fn a_sole_input_parameter_is_an_exact_hole() {
    ready();
    let source = "fn generated(input: TokenStream) -> TokenStream { quote! { #input } }";
    assert_eq!(parses(source), Ok(()));
    let ProcMacroSchema::Template { rules } = schema(source, &["input".to_string()]) else {
        panic!("expected a quote template");
    };
    assert_eq!(rules[0].holes.len(), 1);
    assert!(matches!(
        &rules[0].holes[0],
        SchemaHole::Exact(ExactPart::InputTokens)
    ));
}

#[test]
fn a_plain_quote_loop_is_recorded_as_a_repetition() {
    ready();
    let source = "fn generated(fields: Fields) { for field in fields { generated.push(quote! { fn #field() {} }); } }";
    assert_eq!(parses(source), Ok(()));
    let ProcMacroSchema::Template { rules } = schema(source, &["fields".to_string()]) else {
        panic!("expected a quote template");
    };
    assert!(rules[0].repetition);
    assert!(matches!(rules[0].holes.as_slice(), [SchemaHole::Opaque { .. }]));
}

#[test]
fn control_flow_in_a_quote_loop_keeps_its_holes_opaque() {
    ready();
    let source = "fn generated(fields: Fields) { for field in fields { if skip(field) { continue; } generated.push(quote! { fn #field() {} }); } }";
    assert_eq!(parses(source), Ok(()));
    let ProcMacroSchema::Template { rules } = schema(source, &["fields".to_string()]) else {
        panic!("expected a quote template");
    };
    assert!(matches!(rules[0].holes.as_slice(), [SchemaHole::Opaque { .. }]));
}

#[test]
fn a_match_in_a_quote_loop_keeps_its_holes_opaque() {
    ready();
    let source = "fn generated(fields: Fields) { for field in fields { match field { _ => {} } generated.push(quote! { fn #field() {} }); } }";
    assert_eq!(parses(source), Ok(()));
    let ProcMacroSchema::Template { rules } = schema(source, &["fields".to_string()]) else {
        panic!("expected a quote template");
    };
    assert!(matches!(rules[0].holes.as_slice(), [SchemaHole::Opaque { .. }]));
}
