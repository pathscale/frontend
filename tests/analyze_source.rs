//! Live `analyze_source` without a pinned matching sysroot.
//!
//! `abort_if_errors` still raises a fatal error. `analyze_source` catches it, so
//! a refused program is `Err` and the process stays up.

const WITHIN_CRATE: &str = r#"
#![feature(no_core, lang_items)]
#![no_core]
#[lang = "pointee_sized"]
pub trait PointeeSized {}
#[lang = "meta_sized"]
pub trait MetaSized: PointeeSized {}
#[lang = "sized"]
pub trait Sized: MetaSized {}
#[lang = "copy"]
pub trait Copy {}
fn foo() {}
fn bar() { foo(); }
"#;

#[test]
fn a_refused_program_does_not_kill_the_process() {
    assert!(
        frontend::unwind_janky::unwinding_is_enabled(),
        "panic=unwind is required to contain abort_if_errors"
    );
    let refused = frontend::frontend_facts::analyze_source("fixture", "fn");
    assert!(refused.is_err(), "{refused:?}");
}

#[test]
fn within_crate_refs_do_not_need_a_pinned_sysroot() {
    let facts = frontend::frontend_facts::analyze_source("fixture", WITHIN_CRATE)
        .expect("within-crate fixture should analyse");
    assert!(
        facts
            .references
            .iter()
            .any(|reference| reference.to_def_path.contains("foo")),
        "{facts:?}"
    );
}

const WITHIN_CRATE_USES: &str = r#"
#![feature(no_core, lang_items)]
#![no_core]
#[lang = "pointee_sized"]
pub trait PointeeSized {}
#[lang = "meta_sized"]
pub trait MetaSized: PointeeSized {}
#[lang = "sized"]
pub trait Sized: MetaSized {}
#[lang = "copy"]
pub trait Copy {}
pub fn foo() {}
mod inner {
    pub use crate::foo as bar;
    use crate::foo;
    use crate::*;
    fn uses() { foo(); bar(); }
}
"#;

#[test]
fn use_bindings_include_rename_and_skip_list_stems() {
    let facts = frontend::frontend_facts::analyze_source("fixture", WITHIN_CRATE_USES)
        .expect("within-crate uses should analyse");
    let renamed = facts
        .imports
        .iter()
        .find(|import| import.reexport && import.path == "crate::foo")
        .expect("pub use crate::foo as bar");
    assert_eq!(renamed.bindings, ["bar"], "{facts:?}");
    let plain = facts
        .imports
        .iter()
        .find(|import| !import.reexport && import.path == "crate::foo")
        .expect("use crate::foo");
    assert_eq!(plain.bindings, ["foo"], "{facts:?}");
    let glob = facts
        .imports
        .iter()
        .find(|import| import.path.ends_with("::*"))
        .expect("use crate::*");
    assert!(glob.bindings.is_empty(), "{facts:?}");
}

#[test]
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
        WITHIN_CRATE,
        Some(sysroot),
    )
    .expect("host sysroot is optional, not a pin");
    assert!(
        facts
            .definitions
            .iter()
            .any(|definition| definition.name == "foo"),
        "{facts:?}"
    );
}
