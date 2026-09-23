//! `frontend_facts::syntax`, driven from outside the crate.
//!
//! Out here because the parser needs a catcher, and a catcher needs `std`, which the library
//! does not name. This file is a `std` program like `src/bin/frontend_facts.rs`, so it installs
//! `std::panic::catch_unwind` the same way. None of it needs a sysroot: parsing reads only the
//! text it is handed.

use frontend::frontend_facts::syntax::{Fragment, parses, parses_as};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

/// Install the catcher. Every test calls it, because the harness runs them in any order and on
/// any thread; installing twice is harmless.
fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

fn refused(source: &str, kind: Fragment) -> Vec<String> {
    match parses_as(source, kind) {
        Ok(()) => panic!("{source:?} parsed as {kind:?} and should not have"),
        Err(errors) => errors,
    }
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
