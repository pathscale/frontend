//! A library read (`CrateRead::library`): a crate its own compiler already compiled, of any
//! version, read for its facts and its metadata without being judged.
//!
//! Three tests always run, on `no_core` crates written here. The same crate is refused by a strict
//! read and read by a library read, once for a type error in a body its metadata carries and once
//! for two overlapping impls; the library read records what it met, refuses nothing, writes
//! metadata a strict check then loads, and reports its facts complete, since only judging was
//! skipped. The third reads crates that lose input (an import that does not resolve, a macro
//! that does not expand, a module file that is missing) and one that only meets an unknown
//! attribute, and checks `CrateFacts::complete` tells the two apart.
//!
//! One test is ignored unless `FRONTEND_RUST_SRC_ROOTS` names toolchains' `rust-src` trees,
//! colon-separated, each the directory that holds `library/` (a toolchain's
//! `lib/rustlib/src/rust`). From each it reads `std` and every crate `std` depends on on this
//! host, in dependency order, each a library read that loads the metadata the reads before it
//! wrote, and it requires every read to come back with facts and metadata:
//!
//! ```text
//! FRONTEND_RUST_SRC_ROOTS=$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust:... \
//!     cargo test --test library_read -- --ignored --nocapture
//! ```
//!
//! Which crates, with which features, is what cargo would resolve for `std` with its
//! `backtrace` and `panic-unwind` features on this host, worked out here from the tree's own
//! `library/Cargo.lock` and each crate's manifest (see `plan`). Registry crates are taken from
//! the tree's `library/vendor` when it has one, and otherwise from cargo's registry cache,
//! `$CARGO_HOME/registry/src`, at the version the lock file names.
//!
//! A `std` program, because the catcher needs `std`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
impl Copy for u8 {}
impl Copy for u32 {}
";

/// A scratch directory of the test's own, removed when it is dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("frontend-library-read-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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

/// `source` read as crate `name` with its metadata written, strictly or as a library.
fn read_fixture(
    scratch: &Scratch,
    name: &str,
    source: &str,
    library: bool,
) -> (Result<frontend::frontend_facts::CrateFacts, frontend::frontend_facts::Refused>, String) {
    let root = scratch.write(&format!("{name}.rs"), source);
    let metadata = scratch.path(&format!("lib{name}.rmeta"));
    let read = CrateRead {
        edition: Some("2021"),
        items_only: true,
        write_metadata: Some(eko::path::Path::new(&metadata)),
        ..CrateRead::new(name, eko::path::Path::new(&root))
    }
    .library(library);
    (read_crate(&read), metadata)
}

/// A type error in a `const fn`, whose body the metadata carries so other crates can evaluate
/// it: the strict read refuses the crate and writes nothing; the library read records the error
/// and writes metadata a strict check against it loads.
#[test]
fn a_type_error_is_refused_by_a_strict_read_and_recorded_by_a_library_read() {
    frontend::unwind_janky::install_catcher(catcher);
    let source = format!("{LANG}pub const fn widen(x: u8) -> u32 {{ x }}\n");

    let strict_dir = Scratch::new("strict-type-error");
    let (strict, metadata) = read_fixture(&strict_dir, "base", &source, false);
    let refused = strict.expect_err("a strict read refuses a crate with a type error");
    assert_eq!(refused.crate_name, "base");
    assert!(refused.diagnostics.iter().any(|d| d.contains("E0308")), "{:?}", refused.diagnostics);
    assert!(!Path::new(&metadata).exists(), "a refused crate writes no metadata");

    let library_dir = Scratch::new("library-type-error");
    let (library, metadata) = read_fixture(&library_dir, "base", &source, true);
    let facts = library.expect("a library read refuses nothing it can read");
    assert!(facts.diagnostics.iter().any(|d| d.contains("E0308")), "{:?}", facts.diagnostics);
    // A type error only judges the body; reading lost nothing, so the facts are whole.
    assert!(facts.complete, "a type error loses no fact: {:?}", facts.diagnostics);
    assert!(facts.definitions.iter().any(|d| &*d.def_path == "widen"));
    assert!(Path::new(&metadata).is_file(), "a library read writes its metadata");

    // The metadata is a crate another read loads: a strict check of a file that calls it.
    let dependencies = [Dependency::new("base", metadata)];
    let loaded = Loaded { dependencies: &dependencies, ..Loaded::default() };
    let checked = check_source_against(
        "user",
        Arc::new("#![feature(no_core)]\n#![no_core]\npub fn f() -> u32 { base::widen(1) }\n".into()),
        Some("2021"),
        loaded,
        1,
    );
    assert!(checked.is_clean(), "{:?}", checked.errors);
}

/// Two impls this build finds overlapping: coherence refuses them in a strict read; a library read
/// does not judge coherence, keeps both impls, and reports nothing.
#[test]
fn overlapping_impls_are_refused_by_a_strict_read_and_not_judged_by_a_library_read() {
    frontend::unwind_janky::install_catcher(catcher);
    let source = format!("{LANG}pub trait Mark {{}}\nimpl<T> Mark for T {{}}\nimpl Mark for u32 {{}}\n");

    let strict_dir = Scratch::new("strict-overlap");
    let (strict, _) = read_fixture(&strict_dir, "marks", &source, false);
    let refused = strict.expect_err("a strict read refuses overlapping impls");
    assert!(refused.diagnostics.iter().any(|d| d.contains("E0119")), "{:?}", refused.diagnostics);

    let library_dir = Scratch::new("library-overlap");
    let (library, metadata) = read_fixture(&library_dir, "marks", &source, true);
    let facts = library.expect("a library read does not judge overlap");
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    assert!(facts.complete);
    let marks = facts.impls.iter().filter(|i| i.trait_def_path.as_deref() == Some("Mark")).count();
    assert_eq!(marks, 2, "{:?}", facts.impls);
    assert!(Path::new(&metadata).is_file());
}

/// In a library read `complete` says whether reading lost input, not whether anything was
/// emitted. Every case records an error and is read, `Kept` among its definitions; only the
/// cases where parsing, expansion or name resolution gave up on some input are incomplete.
#[test]
fn a_library_read_is_incomplete_only_where_reading_lost_input() {
    frontend::unwind_janky::install_catcher(catcher);
    let cases: [(&str, &str, bool); 5] = [
        // Only judged: an attribute nothing defines is kept inert, and its item is read.
        ("unknown-attribute", "#[nope]\npub struct Kept;\n", true),
        // Lost: the import brings in nothing.
        ("unresolved-import", "use crate::nope::Thing;\npub struct Kept;\n", false),
        // Lost: no rule matches, so what the macro would have made is missing.
        (
            "unmatched-macro",
            "macro_rules! make { () => { pub struct Made; } }\n\
             make!(unexpected);\npub struct Kept;\n",
            false,
        ),
        // Lost: no macro stands behind the invocation.
        ("unresolved-macro", "nope!();\npub struct Kept;\n", false),
        // Lost: the module's file is not there, so none of its items are.
        ("missing-module", "mod gone;\npub struct Kept;\n", false),
    ];
    for (name, items, complete) in cases {
        let dir = Scratch::new(&format!("loss-{name}"));
        let source = format!("{LANG}{items}");
        let (read, metadata) = read_fixture(&dir, "losses", &source, true);
        let facts = read.unwrap_or_else(|refused| {
            panic!("{name}: a library read refused: {:?}", refused.diagnostics)
        });
        assert!(!facts.diagnostics.is_empty(), "{name}: the error is recorded");
        assert_eq!(facts.complete, complete, "{name}: {:?}", facts.diagnostics);
        assert!(
            facts.definitions.iter().any(|d| &*d.def_path == "Kept"),
            "{name}: {:?}",
            facts.definitions
        );
        assert!(Path::new(&metadata).is_file(), "{name}: a library read writes its metadata");
    }
}

/// `std` and everything it depends on, read from each `rust-src` tree named, as a chain.
#[test]
#[ignore = "reads the rust-src trees FRONTEND_RUST_SRC_ROOTS names, colon-separated"]
fn every_rust_src_reads_core_alloc_and_std_as_a_chain() {
    let roots = std::env::var("FRONTEND_RUST_SRC_ROOTS").expect(
        "FRONTEND_RUST_SRC_ROOTS names rust-src trees, colon-separated, each holding `library/`",
    );
    frontend::unwind_janky::install_catcher(catcher);
    let roots: Vec<PathBuf> = roots.split(':').filter(|r| !r.is_empty()).map(PathBuf::from).collect();
    assert!(!roots.is_empty(), "FRONTEND_RUST_SRC_ROOTS names no tree");

    let mut failures = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let scratch = Scratch::new(&format!("chain-{index}"));
        if let Err(failure) = read_chain(root, &scratch.0) {
            failures.push(format!("{}: {failure}", root.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Read one tree's `std` chain into `out`. `Err` says which crate was refused and why.
fn read_chain(root: &Path, out: &Path) -> Result<(), String> {
    let library = root.join("library");
    let graph = plan::Graph::for_std(&library)?;
    let order = graph.order();
    eprintln!("{}: {} crates: {}", root.display(), order.len(), order.join(", "));

    // Every read's metadata, by package, as it is written.
    let mut written: BTreeMap<String, String> = BTreeMap::new();
    let mut read_names = BTreeSet::new();
    for id in &order {
        let package = &graph.packages[id];
        let crate_name = package.crate_name();
        if !read_names.insert(crate_name.clone()) {
            return Err(format!("two crates of the chain are named `{crate_name}`"));
        }
        let metadata = out.join(format!("lib{crate_name}.rmeta")).to_str().expect("UTF-8").to_string();

        // What cargo hands rustc: each dependency under the name the manifest gives it, and
        // every crate those depend on, which a crate's metadata names and the reader loads.
        // `core` and `compiler_builtins` are within reach of every crate after them, as they are
        // in a standard library build: the injected `extern crate core` of a `#![no_std]`
        // crate and the injected `compiler_builtins` need them whether or not the manifest
        // names them.
        let mut dependencies = Vec::new();
        let direct = &graph.edges[id];
        for (key, target) in direct {
            dependencies.push(Dependency::new(key.replace('-', "_"), written[target].clone()));
        }
        let direct_targets: BTreeSet<&String> = direct.values().collect();
        let mut reach = graph.closure(id);
        for implicit in [graph.core(), graph.compiler_builtins()].into_iter().flatten() {
            if implicit != *id && written.contains_key(&implicit) && !graph.closure(&implicit).contains(id) {
                reach.insert(implicit);
            }
        }
        for target in &reach {
            if !direct_targets.contains(target) {
                let name = graph.packages[target].crate_name();
                dependencies.push(Dependency::transitive(name, written[target].clone()));
            }
        }

        let cfg: Vec<String> = package.cfg(&graph.features[id]);
        let env = package.env(&crate_name);
        let root_file = package.root_file();
        let read = CrateRead {
            edition: Some(package.manifest.edition.as_str()),
            items_only: true,
            standard_library: true,
            loaded: Loaded { dependencies: &dependencies, cfg: &cfg, env: &env },
            write_metadata: Some(eko::path::Path::new(&metadata)),
            ..CrateRead::new(&crate_name, eko::path::Path::new(root_file.to_str().expect("UTF-8")))
        }
        .library(true);
        let facts = read_crate(&read).map_err(|refused| {
            let shown = refused.diagnostics.iter().take(20).cloned().collect::<Vec<_>>().join("\n");
            format!(
                "`{crate_name}` ({id}) refused, {} diagnostics:\n{shown}",
                refused.diagnostics.len()
            )
        })?;
        eprintln!(
            "  {crate_name}: {} definitions, {} impls, {} diagnostics recorded, complete: {}",
            facts.definitions.len(),
            facts.impls.len(),
            facts.diagnostics.len(),
            facts.complete
        );
        for diagnostic in facts.diagnostics.iter().take(5) {
            eprintln!("    {}", diagnostic.lines().next().unwrap_or_default());
        }
        if !Path::new(&metadata).is_file() {
            return Err(format!("`{crate_name}` read, but its metadata was not written"));
        }
        if ["core", "alloc", "std"].contains(&crate_name.as_str()) && facts.definitions.is_empty() {
            return Err(format!("`{crate_name}` read with no definitions"));
        }
        written.insert(id.clone(), metadata);
    }
    for name in ["core", "alloc", "std"] {
        if !read_names.contains(name) {
            return Err(format!("the chain has no `{name}`"));
        }
    }
    Ok(())
}

/// Which crates `std` depends on on this host, with which features, in which order: what cargo
/// resolves from `library/Cargo.lock` and the manifests, as much of it as the standard library's
/// manifests use. Build scripts are not run; what `std`'s prints on every host is supplied.
mod plan {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use super::toml::{self, Value};

    /// Whether a dependency is a normal, dev or build dependency. Only normal ones are read.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum Kind {
        Normal,
        Dev,
        Build,
    }

    /// One dependency as a manifest declares it, for one platform.
    #[derive(Clone, Debug)]
    struct DepDecl {
        /// The name the crate knows it by.
        key: String,
        /// The package it is, which `package = ` renames.
        package: String,
        path: Option<String>,
        optional: bool,
        default_features: bool,
        features: Vec<String>,
        /// `cfg(...)` or a target tuple, from a `[target.X.dependencies]` table.
        platform: Option<String>,
        kind: Kind,
    }

    impl DepDecl {
        fn on_this_host(&self) -> bool {
            self.kind == Kind::Normal && self.platform.as_deref().is_none_or(super::cfg::holds)
        }
    }

    /// What a `Cargo.toml` says that this reads.
    #[derive(Clone, Debug, Default)]
    pub struct Manifest {
        pub name: String,
        pub version: String,
        pub edition: String,
        lib_name: Option<String>,
        lib_path: Option<String>,
        features: BTreeMap<String, Vec<String>>,
        deps: Vec<DepDecl>,
        patches: BTreeMap<String, String>,
    }

    impl Manifest {
        fn read(path: &Path) -> Result<Manifest, String> {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            Ok(Manifest::parse(&text))
        }

        fn parse(text: &str) -> Manifest {
            let mut manifest = Manifest { edition: "2015".to_string(), ..Manifest::default() };
            // Each dependency's fields, gathered from wherever the manifest writes them.
            let mut deps: BTreeMap<(Option<String>, Kind, String), Vec<(String, Value)>> =
                BTreeMap::new();
            for entry in toml::parse(text) {
                let table: Vec<&str> = entry.table.iter().map(String::as_str).collect();
                let key: Vec<&str> = entry.key.iter().map(String::as_str).collect();
                let text = entry.value.as_str().map(str::to_string);
                match (table.as_slice(), key.as_slice()) {
                    (["package"], ["name"]) => manifest.name = text.unwrap_or_default(),
                    (["package"], ["version"]) => manifest.version = text.unwrap_or_default(),
                    (["package"], ["edition"]) => {
                        manifest.edition = text.unwrap_or_else(|| "2015".to_string())
                    }
                    (["lib"], ["name"]) => manifest.lib_name = text,
                    (["lib"], ["path"]) => manifest.lib_path = text,
                    (["features"], [feature]) => {
                        manifest.features.insert(feature.to_string(), entry.value.strings());
                    }
                    (["patch", _], [name]) => {
                        if let Some(path) = entry.value.get("path").and_then(Value::as_str) {
                            manifest.patches.insert(name.to_string(), path.to_string());
                        }
                    }
                    _ => {
                        let Some((platform, kind, rest)) = dependency_table(&table) else {
                            continue;
                        };
                        let (name, fields): (&str, Vec<(String, Value)>) = match (rest, key.as_slice()) {
                            ([], [name]) => match &entry.value {
                                Value::Table(fields) => (*name, fields.clone()),
                                value => (*name, vec![("version".to_string(), value.clone())]),
                            },
                            ([], [name, field]) | ([name], [field]) => {
                                (*name, vec![(field.to_string(), entry.value.clone())])
                            }
                            _ => continue,
                        };
                        deps.entry((platform, kind, name.to_string())).or_default().extend(fields);
                    }
                }
            }
            for ((platform, kind, key), fields) in deps {
                let field = |name: &str| fields.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v);
                let flag = |names: &[&str], default: bool| {
                    names.iter().find_map(|name| field(*name).and_then(Value::as_bool)).unwrap_or(default)
                };
                manifest.deps.push(DepDecl {
                    package: field("package").and_then(Value::as_str).unwrap_or(&key).to_string(),
                    path: field("path").and_then(Value::as_str).map(str::to_string),
                    optional: flag(&["optional"], false),
                    default_features: flag(&["default-features", "default_features"], true),
                    features: field("features").map(Value::strings).unwrap_or_default(),
                    key,
                    platform,
                    kind,
                });
            }
            manifest
        }
    }

    /// `[dependencies]`, `[target.X.dependencies]`, their dev and build forms, and one
    /// dependency's own table under any of them: the platform, the kind, and what follows.
    fn dependency_table<'t>(table: &'t [&'t str]) -> Option<(Option<String>, Kind, &'t [&'t str])> {
        let (platform, rest) = match table {
            ["target", platform, rest @ ..] => (Some(platform.to_string()), rest),
            rest => (None, rest),
        };
        let (kind, rest) = rest.split_first()?;
        let kind = match *kind {
            "dependencies" => Kind::Normal,
            "dev-dependencies" | "dev_dependencies" => Kind::Dev,
            "build-dependencies" | "build_dependencies" => Kind::Build,
            _ => return None,
        };
        Some((platform, kind, rest))
    }

    /// One `[[package]]` of `Cargo.lock`.
    #[derive(Clone, Debug)]
    struct Locked {
        name: String,
        version: String,
        /// From a registry, not a path in the tree.
        registry: bool,
        /// Each dependency as the lock writes it: `name`, or `name version` where the name is
        /// not unique.
        dependencies: Vec<String>,
    }

    impl Locked {
        fn id(&self) -> String {
            format!("{} {}", self.name, self.version)
        }

        fn read(path: &Path) -> Result<Vec<Locked>, String> {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let mut packages: BTreeMap<usize, Locked> = BTreeMap::new();
            for entry in toml::parse(&text) {
                if entry.table != ["package"] {
                    continue;
                }
                let package = packages.entry(entry.occurrence).or_insert_with(|| Locked {
                    name: String::new(),
                    version: String::new(),
                    registry: false,
                    dependencies: Vec::new(),
                });
                match entry.key.first().map(String::as_str) {
                    Some("name") => package.name = entry.value.as_str().unwrap_or_default().to_string(),
                    Some("version") => {
                        package.version = entry.value.as_str().unwrap_or_default().to_string()
                    }
                    Some("source") => package.registry = true,
                    Some("dependencies") => package.dependencies = entry.value.strings(),
                    _ => {}
                }
            }
            Ok(packages.into_values().collect())
        }
    }

    /// One package of the chain: where it is and what its manifest says.
    pub struct Package {
        pub dir: PathBuf,
        pub manifest: Manifest,
    }

    impl Package {
        /// The name rustc knows the crate by.
        pub fn crate_name(&self) -> String {
            self.manifest.lib_name.clone().unwrap_or_else(|| self.manifest.name.replace('-', "_"))
        }

        pub fn root_file(&self) -> PathBuf {
            self.dir.join(self.manifest.lib_path.as_deref().unwrap_or("src/lib.rs"))
        }

        /// `--cfg` for each enabled feature, and what a build script prints that the source reads.
        pub fn cfg(&self, features: &BTreeSet<String>) -> Vec<String> {
            let mut cfg: Vec<String> = features.iter().map(|f| format!("feature=\"{f}\"")).collect();
            // `std`'s build script prints this on every host.
            if self.manifest.name == "std" {
                cfg.push("backtrace_in_libstd".to_string());
            }
            cfg
        }

        /// What cargo sets for `env!`, and what `std`'s build script prints for it on every host.
        pub fn env(&self, crate_name: &str) -> Vec<(String, String)> {
            let version = &self.manifest.version;
            let core = version.split('-').next().unwrap_or_default();
            let mut parts = core.split('.');
            let mut env = vec![
                ("CARGO_PKG_NAME".to_string(), self.manifest.name.clone()),
                ("CARGO_PKG_VERSION".to_string(), version.clone()),
                ("CARGO_PKG_VERSION_MAJOR".to_string(), parts.next().unwrap_or("0").to_string()),
                ("CARGO_PKG_VERSION_MINOR".to_string(), parts.next().unwrap_or("0").to_string()),
                ("CARGO_PKG_VERSION_PATCH".to_string(), parts.next().unwrap_or("0").to_string()),
                (
                    "CARGO_PKG_VERSION_PRE".to_string(),
                    version.split_once('-').map(|(_, pre)| pre.to_string()).unwrap_or_default(),
                ),
                ("CARGO_CRATE_NAME".to_string(), crate_name.to_string()),
                ("CARGO_MANIFEST_DIR".to_string(), self.dir.to_str().unwrap_or_default().to_string()),
            ];
            if self.manifest.name == "std" {
                env.push(("STD_ENV_ARCH".to_string(), std::env::consts::ARCH.to_string()));
            }
            env
        }
    }

    /// The resolved graph: every package reached from `std`, its features, and its edges.
    pub struct Graph {
        library: PathBuf,
        lock: Vec<Locked>,
        patches: BTreeMap<String, String>,
        root: String,
        /// By package id (`name version`).
        pub packages: BTreeMap<String, Package>,
        pub features: BTreeMap<String, BTreeSet<String>>,
        /// Each package's enabled normal dependencies on this host: the name it knows each by,
        /// and the package it is.
        pub edges: BTreeMap<String, BTreeMap<String, String>>,
        /// `dep?/feature` waiting for `dep` to be enabled.
        weak: BTreeMap<(String, String), Vec<String>>,
    }

    impl Graph {
        /// `std` with its `backtrace` and `panic-unwind` features, as a standard library build
        /// enables them, and everything that pulls in on this host.
        pub fn for_std(library: &Path) -> Result<Graph, String> {
            let lock = Locked::read(&library.join("Cargo.lock"))?;
            let workspace = Manifest::read(&library.join("Cargo.toml"))?;
            let std_lock = lock
                .iter()
                .find(|locked| locked.name == "std" && !locked.registry)
                .ok_or("library/Cargo.lock has no `std`")?;
            let root = std_lock.id();
            let mut graph = Graph {
                library: library.to_path_buf(),
                lock: lock.clone(),
                patches: workspace.patches,
                root: root.clone(),
                packages: BTreeMap::new(),
                features: BTreeMap::new(),
                edges: BTreeMap::new(),
                weak: BTreeMap::new(),
            };
            let std_dir = library.join("std");
            let std_manifest = Manifest::read(&std_dir.join("Cargo.toml"))?;
            let wanted: Vec<String> = ["backtrace", "panic-unwind"]
                .into_iter()
                .filter(|f| std_manifest.features.contains_key(*f))
                .map(str::to_string)
                .collect();
            graph.activate(&root, &std_dir, &wanted, true)?;
            Ok(graph)
        }

        pub fn core(&self) -> Option<String> {
            self.by_name("core")
        }

        pub fn compiler_builtins(&self) -> Option<String> {
            self.by_name("compiler_builtins")
        }

        fn by_name(&self, name: &str) -> Option<String> {
            self.packages.iter().find(|(_, p)| p.manifest.name == name).map(|(id, _)| id.clone())
        }

        /// Every package in dependency order: `compiler_builtins` first, which every crate
        /// but `core` loads, then what `std` reaches.
        pub fn order(&self) -> Vec<String> {
            let mut order = Vec::new();
            let mut seen = BTreeSet::new();
            if let Some(builtins) = self.compiler_builtins() {
                self.visit(&builtins, &mut seen, &mut order);
            }
            self.visit(&self.root, &mut seen, &mut order);
            order
        }

        fn visit(&self, id: &String, seen: &mut BTreeSet<String>, order: &mut Vec<String>) {
            if !seen.insert(id.clone()) {
                return;
            }
            for target in self.edges[id].values() {
                self.visit(target, seen, order);
            }
            order.push(id.clone());
        }

        /// Every package `id` depends on, directly or not.
        pub fn closure(&self, id: &String) -> BTreeSet<String> {
            let mut seen = BTreeSet::new();
            let mut stack: Vec<&String> = self.edges[id].values().collect();
            while let Some(next) = stack.pop() {
                if seen.insert(next.clone()) {
                    stack.extend(self.edges[next].values());
                }
            }
            seen
        }

        /// Where a dependency of the package `parent` (at `parent_dir`) is, as the lock file
        /// resolved it: its package id and its directory. `None` when the lock has no such
        /// dependency of `parent`, which is a dependency no feature of the workspace enables.
        fn locate(&self, parent: &str, parent_dir: &Path, decl: &DepDecl) -> Option<(String, PathBuf)> {
            let locked_parent = self.lock.iter().find(|locked| locked.id() == parent)?;
            let version: Option<String> = locked_parent.dependencies.iter().find_map(|entry| {
                let mut words = entry.split(' ');
                (words.next() == Some(decl.package.as_str())).then(|| words.next().map(str::to_string))
            })?;
            let locked = self.lock.iter().find(|locked| {
                locked.name == decl.package && version.as_ref().is_none_or(|v| &locked.version == v)
            })?;
            let dir = if let Some(path) = &decl.path {
                parent_dir.join(path)
            } else if locked.registry {
                registry_dir(&self.library, &locked.name, &locked.version)?
            } else {
                self.library.join(self.patches.get(&decl.package)?)
            };
            Some((locked.id(), std::fs::canonicalize(&dir).unwrap_or(dir)))
        }

        fn activate(
            &mut self,
            id: &String,
            dir: &Path,
            features: &[String],
            default: bool,
        ) -> Result<(), String> {
            if !self.packages.contains_key(id) {
                let manifest = Manifest::read(&dir.join("Cargo.toml"))?;
                let required: Vec<String> = manifest
                    .deps
                    .iter()
                    .filter(|decl| decl.on_this_host() && !decl.optional)
                    .map(|decl| decl.key.clone())
                    .collect();
                self.packages.insert(id.clone(), Package { dir: dir.to_path_buf(), manifest });
                self.features.insert(id.clone(), BTreeSet::new());
                self.edges.insert(id.clone(), BTreeMap::new());
                for key in required {
                    self.enable(id, &key)?;
                }
            }
            if default && self.packages[id].manifest.features.contains_key("default") {
                self.feature(id, "default")?;
            }
            for feature in features {
                self.feature(id, feature)?;
            }
            Ok(())
        }

        /// Enable `id`'s dependency `key` on this host, with the features its declarations ask
        /// for.
        ///
        /// A dependency on `rustc-std-workspace-core` (`-alloc`, `-std`) is a dependency on the
        /// crate it re-exports. That shim only exists so that cargo builds `core` before the
        /// registry crates that name it; it is `pub use core::*`. Read as a crate of its own it
        /// would put two crates behind the name `core` in one session.
        fn enable(&mut self, id: &String, key: &str) -> Result<(), String> {
            if self.edges[id].contains_key(key) {
                return Ok(());
            }
            let package = &self.packages[id];
            let decls: Vec<DepDecl> = package
                .manifest
                .deps
                .iter()
                .filter(|decl| decl.key == key && decl.on_this_host())
                .cloned()
                .collect();
            let Some(first) = decls.first() else {
                return Ok(());
            };
            let Some((mut target, mut dir)) = self.locate(id, &package.dir.clone(), first) else {
                return Ok(());
            };
            if let Some(real) = first.package.strip_prefix("rustc-std-workspace-") {
                let shim = Manifest::read(&dir.join("Cargo.toml"))?;
                let decl = shim
                    .deps
                    .iter()
                    .find(|decl| decl.key == real && decl.kind == Kind::Normal)
                    .ok_or_else(|| format!("{} does not depend on `{real}`", dir.display()))?;
                (target, dir) = self
                    .locate(&target, &dir, decl)
                    .ok_or_else(|| format!("{}: `{real}` is not in the lock file", dir.display()))?;
            }
            self.edges.get_mut(id).expect("an activated package").insert(key.to_string(), target.clone());
            let features: Vec<String> = decls.iter().flat_map(|decl| decl.features.clone()).collect();
            let default = decls.iter().any(|decl| decl.default_features);
            self.activate(&target, &dir, &features, default)?;
            if let Some(pending) = self.weak.remove(&(id.clone(), key.to_string())) {
                for feature in pending {
                    self.feature(&target, &feature)?;
                }
            }
            Ok(())
        }

        /// Enable `feature` of `id`, and what it enables: other features, optional dependencies
        /// (`dep:x`, or an optional dependency's own name), and features of dependencies
        /// (`x/f`, and `x?/f` once `x` is enabled by something else).
        fn feature(&mut self, id: &String, feature: &str) -> Result<(), String> {
            if !self.features.get_mut(id).expect("an activated package").insert(feature.to_string()) {
                return Ok(());
            }
            let manifest = &self.packages[id].manifest;
            let Some(items) = manifest.features.get(feature).cloned() else {
                if manifest.deps.iter().any(|decl| decl.key == feature && decl.optional) {
                    self.enable(id, feature)?;
                }
                return Ok(());
            };
            for item in items {
                if let Some(dep) = item.strip_prefix("dep:") {
                    self.enable(id, dep)?;
                } else if let Some((dep, dep_feature)) = item.split_once('/') {
                    if let Some(dep) = dep.strip_suffix('?') {
                        match self.edges[id].get(dep).cloned() {
                            Some(target) => self.feature(&target, dep_feature)?,
                            None => self
                                .weak
                                .entry((id.clone(), dep.to_string()))
                                .or_default()
                                .push(dep_feature.to_string()),
                        }
                    } else {
                        self.enable(id, dep)?;
                        if let Some(target) = self.edges[id].get(dep).cloned() {
                            self.feature(&target, dep_feature)?;
                        }
                    }
                } else {
                    self.feature(id, &item)?;
                }
            }
            Ok(())
        }
    }

    /// A registry package's source: the tree's own `library/vendor` when it has one, and
    /// cargo's registry cache otherwise.
    fn registry_dir(library: &Path, name: &str, version: &str) -> Option<PathBuf> {
        let dir_name = format!("{name}-{version}");
        let vendored = library.join("vendor").join(&dir_name);
        if vendored.is_dir() {
            return Some(vendored);
        }
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))?;
        let registries = std::fs::read_dir(cargo_home.join("registry").join("src")).ok()?;
        registries
            .filter_map(Result::ok)
            .map(|registry| registry.path().join(&dir_name))
            .find(|dir| dir.is_dir())
    }
}

/// Whether a `[target.X.dependencies]` table applies to this host: `X` is `cfg(...)`, evaluated
/// against the host this test was built for. A bare target tuple does not apply; none of the
/// standard library's dependencies for a mainstream host are written that way.
mod cfg {
    pub fn holds(spec: &str) -> bool {
        let Some(inner) = spec.strip_prefix("cfg(").and_then(|rest| rest.strip_suffix(')')) else {
            return false;
        };
        let tokens = tokens(inner);
        let mut pos = 0;
        predicate(&tokens, &mut pos)
    }

    #[derive(Debug, PartialEq)]
    enum Token {
        Ident(String),
        Str(String),
        Open,
        Close,
        Comma,
        Eq,
    }

    fn tokens(text: &str) -> Vec<Token> {
        let mut out = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '(' => out.push(Token::Open),
                ')' => out.push(Token::Close),
                ',' => out.push(Token::Comma),
                '=' => out.push(Token::Eq),
                '"' => {
                    let mut s = String::new();
                    for c in chars.by_ref() {
                        if c == '"' {
                            break;
                        }
                        s.push(c);
                    }
                    out.push(Token::Str(s));
                }
                c if c.is_alphanumeric() || c == '_' => {
                    let mut s = String::from(c);
                    while let Some(&c) = chars.peek() {
                        if !(c.is_alphanumeric() || c == '_') {
                            break;
                        }
                        s.push(c);
                        chars.next();
                    }
                    out.push(Token::Ident(s));
                }
                _ => {}
            }
        }
        out
    }

    fn predicate(tokens: &[Token], pos: &mut usize) -> bool {
        let Some(Token::Ident(name)) = tokens.get(*pos) else {
            return false;
        };
        *pos += 1;
        match tokens.get(*pos) {
            Some(Token::Open) => {
                *pos += 1;
                let mut values = Vec::new();
                while !matches!(tokens.get(*pos), None | Some(Token::Close)) {
                    let before = *pos;
                    values.push(predicate(tokens, pos));
                    if tokens.get(*pos) == Some(&Token::Comma) {
                        *pos += 1;
                    }
                    if *pos == before {
                        *pos += 1;
                    }
                }
                *pos += 1;
                match name.as_str() {
                    "all" => values.iter().all(|v| *v),
                    "any" => values.iter().any(|v| *v),
                    "not" => !values.first().copied().unwrap_or(false),
                    _ => false,
                }
            }
            Some(Token::Eq) => {
                *pos += 1;
                let value = match tokens.get(*pos) {
                    Some(Token::Str(value)) => value.as_str(),
                    _ => return false,
                };
                *pos += 1;
                key_value(name, value)
            }
            _ => flag(name),
        }
    }

    fn flag(name: &str) -> bool {
        match name {
            "unix" => cfg!(unix),
            "windows" => cfg!(windows),
            _ => false,
        }
    }

    fn key_value(key: &str, value: &str) -> bool {
        match key {
            "target_os" => value == std::env::consts::OS,
            "target_family" => value == std::env::consts::FAMILY,
            "target_arch" => value == std::env::consts::ARCH,
            "target_pointer_width" => value == usize::BITS.to_string(),
            "target_endian" => value == if cfg!(target_endian = "big") { "big" } else { "little" },
            "target_env" => value == host_env(),
            "target_vendor" => value == host_vendor(),
            "panic" => value == "unwind",
            _ => false,
        }
    }

    fn host_env() -> &'static str {
        if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else if cfg!(target_env = "msvc") {
            "msvc"
        } else {
            ""
        }
    }

    fn host_vendor() -> &'static str {
        if cfg!(target_vendor = "apple") {
            "apple"
        } else if cfg!(target_vendor = "pc") {
            "pc"
        } else {
            "unknown"
        }
    }
}

/// As much of TOML as `Cargo.toml` and `Cargo.lock` use here: tables, arrays of tables, dotted
/// and quoted keys, strings of all four kinds, arrays and inline tables across lines, booleans.
/// Anything else (numbers, dates) reads as `Other`.
mod toml {
    #[derive(Clone, Debug)]
    pub enum Value {
        Str(String),
        Bool(bool),
        Array(Vec<Value>),
        Table(Vec<(String, Value)>),
        Other,
    }

    impl Value {
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::Str(text) => Some(text),
                _ => None,
            }
        }

        pub fn as_bool(&self) -> Option<bool> {
            match self {
                Value::Bool(value) => Some(*value),
                _ => None,
            }
        }

        /// An inline table's field.
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Table(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        /// An array's strings; nothing for anything else.
        pub fn strings(&self) -> Vec<String> {
            match self {
                Value::Array(items) => {
                    items.iter().filter_map(Value::as_str).map(str::to_string).collect()
                }
                _ => Vec::new(),
            }
        }
    }

    /// One `key = value`: the table it is in, which `[[table]]` it is in when that is an array
    /// of tables (counted over the document), its key, and its value.
    pub struct Entry {
        pub table: Vec<String>,
        pub occurrence: usize,
        pub key: Vec<String>,
        pub value: Value,
    }

    pub fn parse(text: &str) -> Vec<Entry> {
        let mut p = Parser { s: text.as_bytes(), i: 0 };
        let mut entries = Vec::new();
        let mut table = Vec::new();
        let mut occurrence = 0;
        loop {
            p.space();
            let start = p.i;
            match p.peek() {
                None => return entries,
                Some(b'[') => {
                    let array = p.starts("[[");
                    p.i += if array { 2 } else { 1 };
                    table = p.key();
                    p.blank();
                    p.skip(if array { "]]" } else { "]" });
                    if array {
                        occurrence += 1;
                    }
                }
                _ => {
                    let key = p.key();
                    p.blank();
                    p.skip("=");
                    p.blank();
                    let value = p.value();
                    if !key.is_empty() {
                        entries.push(Entry { table: table.clone(), occurrence, key, value });
                    }
                }
            }
            if p.i == start {
                p.i += 1;
            }
        }
    }

    struct Parser<'a> {
        s: &'a [u8],
        i: usize,
    }

    impl Parser<'_> {
        fn peek(&self) -> Option<u8> {
            self.s.get(self.i).copied()
        }

        fn starts(&self, pattern: &str) -> bool {
            self.s.get(self.i..).is_some_and(|rest| rest.starts_with(pattern.as_bytes()))
        }

        fn skip(&mut self, pattern: &str) {
            if self.starts(pattern) {
                self.i += pattern.len();
            }
        }

        fn text(&self, start: usize, end: usize) -> String {
            String::from_utf8_lossy(&self.s[start..end.min(self.s.len())]).into_owned()
        }

        /// Spaces and tabs.
        fn blank(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t')) {
                self.i += 1;
            }
        }

        /// Blanks, line breaks and comments.
        fn space(&mut self) {
            loop {
                match self.peek() {
                    Some(b' ' | b'\t' | b'\r' | b'\n') => self.i += 1,
                    Some(b'#') => {
                        while !matches!(self.peek(), None | Some(b'\n')) {
                            self.i += 1;
                        }
                    }
                    _ => return,
                }
            }
        }

        /// A key, bare or quoted, dotted into parts.
        fn key(&mut self) -> Vec<String> {
            let mut parts = Vec::new();
            loop {
                self.blank();
                let part = match self.peek() {
                    Some(b'"') => self.basic_string(),
                    Some(b'\'') => self.literal_string(),
                    _ => {
                        let start = self.i;
                        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                        {
                            self.i += 1;
                        }
                        if self.i == start {
                            return parts;
                        }
                        self.text(start, self.i)
                    }
                };
                parts.push(part);
                self.blank();
                if self.peek() == Some(b'.') {
                    self.i += 1;
                } else {
                    return parts;
                }
            }
        }

        fn basic_string(&mut self) -> String {
            if self.starts("\"\"\"") {
                self.i += 3;
                self.skip("\r");
                self.skip("\n");
                let start = self.i;
                while self.peek().is_some() && !self.starts("\"\"\"") {
                    if self.peek() == Some(b'\\') {
                        self.i += 1;
                    }
                    self.i += 1;
                }
                let text = self.text(start, self.i);
                self.i = (self.i + 3).min(self.s.len());
                return text;
            }
            self.i += 1;
            let mut out = Vec::new();
            while let Some(c) = self.peek() {
                self.i += 1;
                match c {
                    b'"' => break,
                    b'\\' => {
                        let Some(escaped) = self.peek() else { break };
                        self.i += 1;
                        out.push(match escaped {
                            b'n' => b'\n',
                            b't' => b'\t',
                            b'r' => b'\r',
                            other => other,
                        });
                    }
                    _ => out.push(c),
                }
            }
            String::from_utf8_lossy(&out).into_owned()
        }

        fn literal_string(&mut self) -> String {
            if self.starts("'''") {
                self.i += 3;
                self.skip("\r");
                self.skip("\n");
                let start = self.i;
                while self.peek().is_some() && !self.starts("'''") {
                    self.i += 1;
                }
                let text = self.text(start, self.i);
                self.i = (self.i + 3).min(self.s.len());
                return text;
            }
            self.i += 1;
            let start = self.i;
            while !matches!(self.peek(), None | Some(b'\'')) {
                self.i += 1;
            }
            let text = self.text(start, self.i);
            self.skip("'");
            text
        }

        fn value(&mut self) -> Value {
            match self.peek() {
                Some(b'"') => Value::Str(self.basic_string()),
                Some(b'\'') => Value::Str(self.literal_string()),
                Some(b'[') => {
                    self.i += 1;
                    let mut items = Vec::new();
                    loop {
                        self.space();
                        let start = self.i;
                        match self.peek() {
                            None => break,
                            Some(b']') => {
                                self.i += 1;
                                break;
                            }
                            Some(b',') => self.i += 1,
                            _ => items.push(self.value()),
                        }
                        if self.i == start {
                            self.i += 1;
                        }
                    }
                    Value::Array(items)
                }
                Some(b'{') => {
                    self.i += 1;
                    let mut fields = Vec::new();
                    loop {
                        self.space();
                        let start = self.i;
                        match self.peek() {
                            None => break,
                            Some(b'}') => {
                                self.i += 1;
                                break;
                            }
                            Some(b',') => self.i += 1,
                            _ => {
                                let key = self.key();
                                self.blank();
                                self.skip("=");
                                self.blank();
                                let value = self.value();
                                fields.push((key.join("."), value));
                            }
                        }
                        if self.i == start {
                            self.i += 1;
                        }
                    }
                    Value::Table(fields)
                }
                _ => {
                    let start = self.i;
                    while !matches!(self.peek(), None | Some(b',' | b']' | b'}' | b'\n' | b'\r' | b'#')) {
                        self.i += 1;
                    }
                    match self.text(start, self.i).trim() {
                        "true" => Value::Bool(true),
                        "false" => Value::Bool(false),
                        _ => Value::Other,
                    }
                }
            }
        }
    }
}
