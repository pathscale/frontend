//! Live `analyze_source` without a pinned matching sysroot.
//!
//! `run_compiler` still `abort_if_errors`, which kills the process, not the
//! request. Keep `Capabilities.references` off until a resident driver exists.
//! These tests stay ignored until that abort is contained.

#[test]
#[ignore = "run_compiler abort_if_errors kills the process"]
fn within_crate_refs_do_not_need_a_pinned_sysroot() {
    let facts = frontend::frontend_facts::analyze_source(
        "fixture",
        "#![feature(no_core)]\n#![no_core]\nfn foo() {}\nfn bar() { foo(); }\n",
    );
    assert!(
        facts
            .references
            .iter()
            .any(|reference| reference.to_def_path.contains("foo")),
        "{facts:?}"
    );
}

#[test]
#[ignore = "run_compiler abort_if_errors kills the process"]
fn host_sysroot_is_defined_and_optional() {
    let printed = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc --print sysroot");
    assert!(printed.status.success(), "rustc --print sysroot failed");
    let sysroot = String::from_utf8(printed.stdout).unwrap();
    let sysroot = sysroot.trim();
    assert!(!sysroot.is_empty());
    let facts = frontend::frontend_facts::analyze_source_with_sysroot(
        "fixture",
        "#![feature(no_core)]\n#![no_core]\nfn foo() {}\nfn bar() { foo(); }\n",
        Some(sysroot),
    );
    assert!(
        facts
            .definitions
            .iter()
            .any(|definition| definition.name == "foo"),
        "{facts:?}"
    );
}
