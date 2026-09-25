//! Proc macros run through the dylib a stable release built (`Dependency::proc_macro_dylib`).
//!
//! One test always runs and needs nothing: a file that is not a dylib is refused with a reason.
//! The other two are `#[ignore]`d, because a proc-macro dylib is what cargo builds and this test
//! builds nothing. Each names what it reads in its `ignore` string.
//!
//! A `std` program, because the catcher needs `std`.

use std::sync::Arc;

use frontend::frontend_facts::{Dependency, Loaded, check_source_against, proc_macro_dylib_macros};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

#[test]
fn a_file_that_is_not_a_proc_macro_dylib_is_refused_with_a_reason() {
    let path = std::env::temp_dir().join(format!("frontend-not-a-dylib-{}.dylib", std::process::id()));
    std::fs::write(&path, "not a shared object").expect("write a fixture");
    let refused = proc_macro_dylib_macros(path.to_str().expect("a UTF-8 temp path"));
    let _ = std::fs::remove_file(&path);
    let message = refused.expect_err("a text file has no proc-macro table");
    assert!(message.contains("frontend-not-a-dylib"), "the file is named: {message}");
}

/// The table of a real dylib, read with the checks a `Dependency::proc_macro_dylib` gets before
/// any of its macros runs.
#[test]
#[ignore = "opens the proc-macro dylib FRONTEND_PROC_MACRO_DYLIB names, built by rustc 1.97 \
            (cargo's target/debug/deps/lib<name>-<hash>.dylib); FRONTEND_PROC_MACRO_EXPECT, \
            optional, is one `kind:name` it must export, such as `derive:Error` for thiserror_impl"]
fn a_release_proc_macro_dylib_lists_its_macros() {
    let dylib = std::env::var("FRONTEND_PROC_MACRO_DYLIB")
        .expect("FRONTEND_PROC_MACRO_DYLIB names a proc-macro dylib rustc 1.97 built");
    let macros = proc_macro_dylib_macros(&dylib).unwrap_or_else(|refused| panic!("{refused}"));
    eprintln!("{dylib}: {macros:?}");
    assert!(!macros.is_empty(), "a proc-macro crate exports at least one macro");
    assert!(macros.iter().all(|(_, name)| !name.is_empty()), "{macros:?}");
    if let Ok(expect) = std::env::var("FRONTEND_PROC_MACRO_EXPECT") {
        let (kind, name) = expect.split_once(':').expect("FRONTEND_PROC_MACRO_EXPECT is `kind:name`");
        assert!(macros.iter().any(|(k, n)| *k == kind && n == name), "no {expect} in {macros:?}");
    }
}

/// One file checked against dependencies a caller already read, one of them a proc-macro crate
/// with its dylib: the check a Delulu row gets. Clean is the expectation for a row's golden fill,
/// and no error may be the refusal a proc-macro crate without a dylib gives.
#[test]
#[ignore = "checks FRONTEND_PM_SOURCE (a file) against FRONTEND_PM_DEPENDENCIES (a JSON file: an \
            array of `Dependency`, the proc-macro ones with `proc_macro_dylib`, as agentcode \
            writes them); FRONTEND_PM_EDITION (default 2021) and FRONTEND_PM_CFG (a JSON file: an \
            array of `--cfg` specs) are optional"]
fn a_file_using_a_proc_macro_with_its_dylib_checks_clean() {
    let read_env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
    let source = std::fs::read_to_string(read_env("FRONTEND_PM_SOURCE")).expect("the source reads");
    let dependencies: Vec<Dependency> = serde_json::from_str(
        &std::fs::read_to_string(read_env("FRONTEND_PM_DEPENDENCIES")).expect("the list reads"),
    )
    .expect("FRONTEND_PM_DEPENDENCIES is a JSON array of Dependency");
    assert!(
        dependencies.iter().any(|d| d.proc_macro_dylib.is_some()),
        "no dependency names a proc_macro_dylib, so nothing here would run"
    );
    let cfg: Vec<String> = match std::env::var("FRONTEND_PM_CFG") {
        Ok(path) => serde_json::from_str(&std::fs::read_to_string(path).expect("the cfg reads"))
            .expect("FRONTEND_PM_CFG is a JSON array of strings"),
        Err(_) => Vec::new(),
    };
    let edition = std::env::var("FRONTEND_PM_EDITION").unwrap_or_else(|_| "2021".to_string());

    frontend::unwind_janky::install_catcher(catcher);
    let loaded = Loaded { dependencies: &dependencies, cfg: &cfg, ..Loaded::default() };
    let checked = check_source_against("row", Arc::new(source), Some(&edition), loaded, 1, false);
    assert!(
        !checked.errors.iter().any(|e| e.contains("is not expanded")),
        "a proc macro was refused: {:?}",
        checked.errors
    );
    assert!(checked.is_clean(), "{:?}", checked.errors);
}
