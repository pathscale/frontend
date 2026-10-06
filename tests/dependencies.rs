//! A crate read with its dependencies loaded, and one file checked against them.
//!
//! The dependency is a `no_core` crate of its own, with the lang items it needs, so nothing
//! outside this file is read: it is read from source once, its metadata written, and the crates
//! after it load that metadata as rustc loads an `--extern`.
//!
//! A `std` program, because the catcher needs `std`.

use std::sync::Arc;

use frontend::frontend_facts::{CrateRead, Dependency, Loaded, check_source_against, read_crate};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

const LANG: &str = "\
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

const BASE: &str = "
pub mod sync {
    pub struct Mutex<T> { value: T }
    impl<T> Mutex<T> {
        pub fn new(value: T) -> Mutex<T> { Mutex { value } }
        pub fn lock(&self) -> &T { &self.value }
    }
}
pub use sync::Mutex;
#[macro_export]
macro_rules! make_fn { ($name:ident) => { pub fn $name() -> u32 { 7 } }; }
";

const TOP: &str = "\
#![feature(no_core)]
#![no_core]
pub use base::Mutex;
base::make_fn!(seven);
pub fn get(m: &base::Mutex<u32>) -> u32 { *m.lock() }
";

/// A scratch directory of the test's own, removed when it is dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("frontend-deps-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        Scratch(dir)
    }

    fn write(&self, file: &str, text: &str) -> String {
        let path = self.0.join(file);
        std::fs::write(&path, text).expect("write a fixture");
        path.to_str().expect("a UTF-8 temp path").to_string()
    }

    fn path(&self, file: &str) -> String {
        self.0.join(file).to_str().expect("a UTF-8 temp path").to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Read `base` from source and write its metadata; the dependency every test below loads.
fn write_base(scratch: &Scratch) -> Dependency {
    let root = scratch.write("base.rs", &format!("{LANG}{BASE}"));
    let metadata = scratch.path("libbase.rmeta");
    let facts = read_crate(&CrateRead {
        edition: Some("2021"),
        items_only: true,
        write_metadata: Some(eko::path::Path::new(&metadata)),
        ..CrateRead::new("base", eko::path::Path::new(&root))
    })
    .expect("base reads");
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    assert!(std::path::Path::new(&metadata).is_file(), "base's metadata was written");
    Dependency::new("base", metadata)
}

#[test]
fn a_crate_reads_with_its_dependency_loaded_from_the_metadata_frontend_wrote() {
    frontend::unwind_janky::install_catcher(catcher);
    let scratch = Scratch::new("read");
    let base = write_base(&scratch);
    let root = scratch.write("top.rs", TOP);
    let dependencies = [base];
    let facts = read_crate(&CrateRead {
        edition: Some("2021"),
        items_only: true,
        loaded: Loaded { dependencies: &dependencies, ..Loaded::default() },
        ..CrateRead::new("top", eko::path::Path::new(&root))
    })
    .expect("top reads");
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    assert!(facts.complete);
    // A macro exported by the dependency expanded here and wrote an item.
    assert!(facts.definitions.iter().any(|definition| &*definition.def_path == "seven"));
    // The re-export resolves into the dependency, where the struct is defined, under its name.
    let root_names = facts
        .modules
        .iter()
        .find(|module| module.module_def_path.is_empty())
        .expect("the root module");
    assert!(
        root_names
            .names
            .iter()
            .any(|name| name.name == "Mutex" && &*name.target_def_path == "base::sync::Mutex"),
        "{:?}",
        root_names.names
    );
}

#[test]
fn a_dependency_with_an_error_is_refused_by_name_and_writes_no_metadata() {
    frontend::unwind_janky::install_catcher(catcher);
    let scratch = Scratch::new("refused");
    let root = scratch.write("broken.rs", &format!("{LANG}use nowhere::Thing;\n"));
    let metadata = scratch.path("libbroken.rmeta");
    let refused = read_crate(&CrateRead {
        edition: Some("2021"),
        items_only: true,
        write_metadata: Some(eko::path::Path::new(&metadata)),
        ..CrateRead::new("broken", eko::path::Path::new(&root))
    })
    .expect_err("a crate with an error is no one's dependency");
    assert_eq!(refused.crate_name, "broken");
    assert!(refused.diagnostics.iter().any(|d| d.contains("E0432")), "{:?}", refused.diagnostics);
    assert!(!std::path::Path::new(&metadata).exists());
}

#[test]
fn one_file_is_checked_against_its_dependency() {
    frontend::unwind_janky::install_catcher(catcher);
    let scratch = Scratch::new("check");
    let dependencies = [write_base(&scratch)];
    let loaded = Loaded { dependencies: &dependencies, ..Loaded::default() };
    let check = |body: &str| {
        let source = format!("#![feature(no_core)]\n#![no_core]\n{body}\n");
        check_source_against("user", Arc::new(source), Some("2021"), loaded, 1)
    };

    let clean = check("pub fn f(m: &base::Mutex<u32>) -> u32 { *m.lock() }");
    assert!(clean.is_clean(), "{:?}", clean.errors);

    let missing = check("pub fn f(m: &base::Mutex<u32>) -> u32 { *m.lock_all() }");
    assert!(
        missing.errors.iter().any(|e| e.starts_with("error[E0599]") && e.contains("lock_all")),
        "{:?}",
        missing.errors
    );

    let arity = check("pub fn f() -> base::Mutex<u32> { base::Mutex::new(0, 1) }");
    assert!(arity.errors.iter().any(|e| e.starts_with("error[E0061]")), "{:?}", arity.errors);

    // A path into the file's own missing siblings is told apart by its code.
    let sibling = check("use crate::other::Thing;");
    assert!(sibling.errors.iter().any(|e| e.starts_with("error[E0432]")), "{:?}", sibling.errors);
}
