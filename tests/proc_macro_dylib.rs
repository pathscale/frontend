//! Proc macros read from source stay declared and are not expanded.

use std::sync::Arc;

use frontend::frontend_facts::{CrateRead, Dependency, Loaded, check_source_against, read_crate};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

const LANG: &str = "\
#![feature(no_core, lang_items)]
#![allow(internal_features)]
#![no_core]
#[lang = \"pointee_sized\"] trait PointeeSized {}
#[lang = \"meta_sized\"] trait MetaSized: PointeeSized {}
#[lang = \"sized\"] trait Sized: MetaSized {}
#[lang = \"copy\"] trait Copy {}
#[lang = \"legacy_receiver\"] trait LegacyReceiver {}
impl<T: ?Sized> LegacyReceiver for &T {}
impl Copy for u32 {}
";

/// A scratch directory of the test's own, removed when it is dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("frontend-proc-macro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        Scratch(dir)
    }

    fn write(&self, file: &str, text: &str) -> String {
        let path = self.0.join(file);
        std::fs::write(&path, text).expect("write a fixture");
        path.to_str().expect("a UTF-8 temp path").to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "a proc-macro crate needs the `proc_macro` library, read from the rust-src tree FRONTEND_RUST_SRC_ROOTS names"]
fn a_proc_macro_dylib_is_never_opened_and_its_macro_stays_unexpanded() {
    frontend::unwind_janky::install_catcher(catcher);
    let scratch = Scratch::new();
    let macro_root = scratch.write(
        "fixture_macros.rs",
        &format!(
            "{LANG}\
#[proc_macro_derive(Unexpanded)]
pub fn unexpanded(_: ()) {{}}
"
        ),
    );
    let metadata = scratch.0.join("libfixture_macros.rmeta");
    read_crate(&CrateRead {
        edition: Some("2021"),
        items_only: true,
        proc_macro: true,
        library: true,
        write_metadata: Some(eko::path::Path::new(
            metadata.to_str().expect("a UTF-8 metadata path"),
        )),
        ..CrateRead::new("fixture_macros", eko::path::Path::new(&macro_root))
    })
    .expect("the proc-macro declarations are read");

    let dependencies = [Dependency::new(
        "fixture_macros",
        metadata.to_str().expect("a UTF-8 metadata path"),
    )];
    let source = format!(
        "{LANG}\
#[derive(fixture_macros::Unexpanded)]
pub struct Marker;
"
    );
    let checked = check_source_against(
        "user",
        Arc::new(source),
        Some("2021"),
        Loaded { dependencies: &dependencies, ..Loaded::default() },
        1,
        false,
    );
    assert!(
        checked.errors.iter().any(|error| error.contains("is not expanded")),
        "{:?}",
        checked.errors
    );
}
