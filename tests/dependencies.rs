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

// Two crates of one name in one read: `dup` v1 and `dup` v2, each read under its own
// disambiguator (cargo's `-C metadata`). `top` names v1 in its prelude and loads `middle`, which
// was read against v2, so both are loaded at once, as the registry's `libc` and the one std was
// built with are when tokio is read.

/// No item of its own but the lang items, so the two `dup`s share one set of them.
const LANGS: &str = LANG;

const DUP_V1: &str = "\
#![feature(no_core)]
#![no_core]
extern crate langs;
pub struct One;
pub fn version() -> u32 { 1 }
";

const DUP_V2: &str = "\
#![feature(no_core)]
#![no_core]
extern crate langs;
pub struct Two;
pub fn version() -> u32 { 2 }
";

/// Read against v2, which it names; the crate that makes v2 reachable to `top`.
const MIDDLE: &str = "\
#![feature(no_core)]
#![no_core]
pub use dup::Two;
pub use dup::version as two;
";

/// Names v1 by `dup`, and v2 only through `middle`.
const TOP_SAME: &str = "\
#![feature(no_core)]
#![no_core]
pub use dup::One;
pub use middle::Two;
";

/// Reads `crate_name` from `source` under `disambiguator`, with `dependencies` loaded, and
/// writes its metadata to `<dir>/lib<crate_name>.rmeta`, a directory of its own because the file
/// name is the crate's name and two `dup`s cannot share one.
fn write_crate(
    scratch: &Scratch,
    dir: &str,
    crate_name: &str,
    source: &str,
    disambiguator: Option<&str>,
    dependencies: &[Dependency],
) -> String {
    std::fs::create_dir_all(scratch.0.join(dir)).expect("a crate's directory");
    let root = scratch.write(&format!("{dir}/{crate_name}.rs"), source);
    let metadata = scratch.path(&format!("{dir}/lib{crate_name}.rmeta"));
    let facts = read_crate(&CrateRead {
        edition: Some("2021"),
        items_only: true,
        loaded: Loaded { dependencies, ..Loaded::default() },
        write_metadata: Some(eko::path::Path::new(&metadata)),
        disambiguator,
        ..CrateRead::new(crate_name, eko::path::Path::new(&root))
    })
    .unwrap_or_else(|refused| panic!("{crate_name} in {dir} reads: {refused:?}"));
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    metadata
}

/// The two `dup`s, `langs` and `middle`, each under `disambiguators` (`dup` v1, `dup` v2,
/// `middle`), and what `top` loads: v1 and `middle` in its prelude, v2 and `langs` not.
fn write_two_dups(scratch: &Scratch, disambiguators: [Option<&str>; 3]) -> Vec<Dependency> {
    let [v1, v2, middle_id] = disambiguators;
    let langs = write_crate(scratch, "langs", "langs", LANGS, Some("langs"), &[]);
    let langs_dependency = [Dependency::new("langs", langs.as_str())];
    let dup_v1 = write_crate(scratch, "dup-1", "dup", DUP_V1, v1, &langs_dependency);
    let dup_v2 = write_crate(scratch, "dup-2", "dup", DUP_V2, v2, &langs_dependency);
    let middle_loads =
        [Dependency::new("dup", dup_v2.as_str()), Dependency::transitive("langs", langs.as_str())];
    let middle = write_crate(scratch, "middle", "middle", MIDDLE, middle_id, &middle_loads);
    vec![
        Dependency::new("dup", dup_v1),
        Dependency::new("middle", middle),
        Dependency::transitive("dup", dup_v2),
        Dependency::transitive("langs", langs),
    ]
}

/// Read `crate_name` from `source` with `dependencies` loaded, not writing metadata: its facts'
/// diagnostics, or the refusal's.
fn read_with(
    scratch: &Scratch,
    crate_name: &str,
    source: &str,
    disambiguator: &str,
    dependencies: &[Dependency],
) -> Result<frontend::frontend_facts::CrateFacts, Vec<String>> {
    let root = scratch.write(&format!("{crate_name}-{disambiguator}.rs"), source);
    read_crate(
        &CrateRead {
            edition: Some("2021"),
            items_only: true,
            loaded: Loaded { dependencies, ..Loaded::default() },
            ..CrateRead::new(crate_name, eko::path::Path::new(&root))
        }
        .with_disambiguator(disambiguator),
    )
    .map_err(|refused| refused.diagnostics)
}

/// The root module's name `name` and where it resolves to.
fn root_target(facts: &frontend::frontend_facts::CrateFacts, name: &str) -> Option<String> {
    let root = facts.modules.iter().find(|module| module.module_def_path.is_empty())?;
    root.names.iter().find(|n| n.name == name).map(|n| n.target_def_path.to_string())
}

#[test]
fn two_crates_of_one_name_load_in_one_read_under_their_disambiguators() {
    frontend::unwind_janky::install_catcher(catcher);
    let scratch = Scratch::new("same-name");
    let dependencies = write_two_dups(&scratch, [Some("v1"), Some("v2"), Some("middle")]);

    // `dup` is v1, the prelude one; v2 comes in through `middle`, which was read against it.
    let facts = read_with(&scratch, "top", TOP_SAME, "top", &dependencies).expect("top reads");
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    assert!(facts.complete);
    assert_eq!(root_target(&facts, "One").as_deref(), Some("dup::One"));
    assert_eq!(root_target(&facts, "Two").as_deref(), Some("dup::Two"));

    // v2 is loaded in that read, and still not reachable by the name `dup`: a lookup by name
    // chooses among the prelude files only, and `Two` is v2's.
    let wrong = "#![feature(no_core)]\n#![no_core]\npub use dup::Two;\n";
    let diagnostics = match read_with(&scratch, "top", wrong, "top-wrong", &dependencies) {
        Ok(facts) => facts.diagnostics,
        Err(diagnostics) => diagnostics,
    };
    assert!(diagnostics.iter().any(|d| d.contains("E0432")), "{diagnostics:?}");

    // A crate read beside a loaded crate of its own name: a third `dup`, loading `middle` and
    // so v2, is told apart from v2 by its own disambiguator.
    let for_v3: Vec<Dependency> =
        dependencies.iter().filter(|d| d.name != "dup" || !d.prelude).cloned().collect();
    let v3 = "#![feature(no_core)]\n#![no_core]\npub use middle::Two;\n";
    let facts = read_with(&scratch, "dup", v3, "v3", &for_v3).expect("a third dup reads");
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    assert_eq!(root_target(&facts, "Two").as_deref(), Some("dup::Two"));
}

#[test]
fn two_crates_of_one_name_without_disambiguators_collide_and_say_so() {
    frontend::unwind_janky::install_catcher(catcher);
    let scratch = Scratch::new("same-name-collide");
    // Today's default: no `-C metadata`, so both `dup`s have one crate id.
    let dependencies = write_two_dups(&scratch, [None, None, None]);
    let diagnostics = match read_with(&scratch, "top", TOP_SAME, "top", &dependencies) {
        Ok(facts) => facts.diagnostics,
        Err(diagnostics) => diagnostics,
    };
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("colliding StableCrateId") || d.contains("same stable crate id")),
        "{diagnostics:?}"
    );
}
