use std::path::{Path, PathBuf};

use frontend::frontend_facts::{CrateRead, Dependency, FactOrigin, Loaded, read_crate};
use frontend::rustc_expand::proc_macro_schema::{ExactPart, ProcMacroSchema, SchemaHole, analyze_source};
use frontend::rustc_session::parse::ParseSess;

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn schema(source: &str, parameters: &[String], syn_projections: bool) -> ProcMacroSchema {
    frontend::rustc_span::create_session_if_not_set_then(
        frontend::rustc_span::edition::Edition::Edition2024,
        |_| analyze_source(&ParseSess::new(), source, parameters, syn_projections),
    )
}

#[test]
fn schema_analysis_does_not_create_interpreter_contexts() {
    frontend::unwind_janky::install_catcher(catcher);
    let before = frontend::rustc_const_eval::interp_cx_new_count();

    let schema = schema(
        "fn generated(input: TokenStream) -> TokenStream { quote! { #input } }",
        &["input".to_string()],
        false,
    );
    assert!(matches!(schema, ProcMacroSchema::Template { .. }));
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
}

#[test]
fn schema_format_ident_literal() {
    frontend::unwind_janky::install_catcher(catcher);
    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let schema = schema(
        "fn generated() -> TokenStream { let name = format_ident!(\"Generated\"); quote! { fn #name() {} } }",
        &[],
        false,
    );
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
    let ProcMacroSchema::Template { rules } = schema else {
        panic!("expected a quote template");
    };
    assert!(matches!(
        rules[0].holes.as_slice(),
        [SchemaHole::Exact(ExactPart::NameTemplate { prefix, suffix })]
            if prefix == "Generated" && suffix.is_empty()
    ));
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("frontend-proc-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temporary directory");
        Scratch(root)
    }

    fn write(&self, name: &str, source: &str) -> String {
        let path = self.0.join(format!("{name}.rs"));
        std::fs::write(&path, source).expect("write a source fixture");
        path.to_str().expect("a UTF-8 temporary path").to_string()
    }

    fn metadata(&self, name: &str) -> String {
        self.0
            .join(format!("lib{name}.rmeta"))
            .to_str()
            .expect("a UTF-8 temporary path")
            .to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_metadata(
    scratch: &Scratch,
    name: &str,
    source: &str,
    dependencies: &[Dependency],
    proc_macro: bool,
) -> String {
    let root = scratch.write(name, source);
    let metadata = scratch.metadata(name);
    let facts = read_crate(
        &CrateRead {
            edition: Some("2021"),
            items_only: true,
            proc_macro,
            loaded: Loaded { dependencies, ..Loaded::default() },
            write_metadata: Some(eko::path::Path::new(&metadata)),
            ..CrateRead::new(name, eko::path::Path::new(&root))
        }
        .library(true),
    )
    .unwrap_or_else(|failure| panic!("{name} fixture was refused: {:?}", failure.diagnostics));
    assert!(Path::new(&metadata).is_file(), "metadata for {name} was written");
    assert!(facts.complete, "{name} fixture lost input: {:?}", facts.diagnostics);
    metadata
}

#[test]
fn exact_and_opaque_derives_transcribe_from_loaded_metadata_without_an_interpreter() {
    frontend::unwind_janky::install_catcher(catcher);
    let scratch = Scratch::new();

    let proc_macro_metadata = write_metadata(
        &scratch,
        "proc_macro",
        "#![feature(no_core, lang_items)]\n#![allow(internal_features)]\n#![no_core]\n\
         #[lang = \"pointee_sized\"] pub trait PointeeSized {}\n\
         #[lang = \"meta_sized\"] pub trait MetaSized: PointeeSized {}\n\
         #[lang = \"sized\"] pub trait Sized: MetaSized {}\n\
         #[lang = \"legacy_receiver\"] pub trait LegacyReceiver {}\n\
         impl<T: ?Sized> LegacyReceiver for &T {}\n\
         impl<T: ?Sized> LegacyReceiver for &mut T {}\n\
         pub struct TokenStream;\n\
         impl TokenStream {\n\
             pub fn new() -> TokenStream { TokenStream }\n\
             pub fn extend(&mut self, _: TokenStream) {}\n\
         }\n\
         pub mod bridge { pub mod client { pub struct Client; impl Client {\n\
             pub const fn expand1(_: fn(TokenStream) -> TokenStream) -> Client { Client }\n\
             pub const fn expand2(_: fn(TokenStream, TokenStream) -> TokenStream) -> Client { Client }\
         } } }\n\
         #[lang = \"clone\"] pub trait Clone: Sized { #[lang = \"clone_fn\"] fn clone(&self) -> Self; }\n\
         #[lang = \"copy\"] pub trait Copy: Clone {}\n\
         impl Clone for TokenStream { fn clone(&self) -> TokenStream { TokenStream } }\n",
        &[],
        false,
    );
    let proc_macro_dependency = [Dependency::new("proc_macro", proc_macro_metadata.clone())];
    let syn_metadata = write_metadata(
        &scratch,
        "syn",
        "#![feature(no_core, lang_items)]\n#![allow(internal_features)]\n#![no_core]\nextern crate proc_macro;\n\
         pub struct Ident;\n\
         pub struct Visibility;\n\
         #[lang = \"Option\"] pub enum Option<T> { #[lang = \"None\"] None, #[lang = \"Some\"] Some(T) }\n\
         pub struct Fields;\n\
         pub struct DeriveInput { pub ident: Ident, pub vis: Visibility, pub generics: Generics, pub fields: Fields }\n\
         pub struct Field { pub ident: Option<Ident> }\n\
         #[lang = \"iterator\"] pub trait Iterator { type Item; #[lang = \"next\"] fn next(&mut self) -> Option<Self::Item>; }\n\
         pub trait IntoIterator { type Item; type IntoIter: Iterator<Item = Self::Item>; #[lang = \"into_iter\"] fn into_iter(self) -> Self::IntoIter; }\n\
         pub struct FieldIter;\n\
         impl Iterator for FieldIter { type Item = Field; fn next(&mut self) -> Option<Field> { Option::None } }\n\
         impl<'a> IntoIterator for &'a Fields { type Item = Field; type IntoIter = FieldIter; fn into_iter(self) -> FieldIter { loop {} } }\n\
         pub struct Generics;\n\
         pub struct ImplGenerics;\n\
         pub struct TypeGenerics;\n\
         pub struct WhereClause;\n\
         impl Generics { pub fn split_for_impl(&self) -> (ImplGenerics, TypeGenerics, Option<WhereClause>) { loop {} } }\n",
        &proc_macro_dependency,
        false,
    );
    let macro_dependencies = [
        Dependency::new("proc_macro", proc_macro_metadata.clone()),
        Dependency::new("syn", syn_metadata.clone()),
    ];
    let macro_metadata = write_metadata(
        &scratch,
        "fixture_macros",
        "#![feature(no_core)]\n#![no_core]\n\
         extern crate proc_macro;\n\
         extern crate syn;\n\
         macro_rules! parse_macro_input { ($input:ident as $ty:path) => { $input }; }\n\
         macro_rules! quote { ($($tokens:tt)*) => { proc_macro::TokenStream::new() }; }\n\
         #[proc_macro_derive(Exact)]\n\
         pub fn exact(input: proc_macro::TokenStream) -> proc_macro::TokenStream {\n\
             let input = parse_macro_input!(input as syn::DeriveInput);\n\
             let name = &input.ident;\n\
             let vis = &input.vis;\n\
             let generics = &input.generics;\n\
             let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();\n\
             let fields = &input.fields;\n\
             let mut output = proc_macro::TokenStream::new();\n\
             for field in fields { let field = field.ident; output.extend(quote! { impl #impl_generics #name #ty_generics #where_clause { #vis fn #field() {} } }); }\n\
             output\n\
         }\n\
         #[proc_macro_derive(Opaque)]\n\
         pub fn opaque(input: proc_macro::TokenStream) -> proc_macro::TokenStream {\n\
             let input = parse_macro_input!(input as syn::DeriveInput);\n\
             let name = &input.ident;\n\
             let fields = &input.fields;\n\
             let mut output = proc_macro::TokenStream::new();\n\
             for field in fields { let field = field.ident; output.extend(quote! { impl #name { pub fn #field() { #body } } }); }\n\
             output\n\
         }\n",
        &macro_dependencies,
        true,
    );
    let before = frontend::rustc_const_eval::interp_cx_new_count();
    let consumer_dependencies = [
        Dependency::new("fixture_macros", macro_metadata),
        Dependency::transitive("proc_macro", proc_macro_metadata),
        Dependency::transitive("syn", syn_metadata),
    ];
    let exact_root = scratch.write(
        "exact_consumer",
        "#![feature(no_core, lang_items, rustc_attrs, decl_macro)]\n#![allow(internal_features)]\n#![no_core]\n#[rustc_builtin_macro] macro derive($item:item) {}\n\
         extern crate fixture_macros;\n\
         #[derive(fixture_macros::Exact)] pub struct ExactItem<T: 'static = u8> where T: 'static { pub first: T, pub second: T }\n",
    );
    let exact_facts = read_crate(
        &CrateRead {
            edition: Some("2021"),
            items_only: true,
            loaded: Loaded { dependencies: &consumer_dependencies, ..Loaded::default() },
            ..CrateRead::new("exact_consumer", eko::path::Path::new(&exact_root))
        }
        .library(true),
    )
    .expect("the exact derive consumer reads");
    assert!(exact_facts.complete, "the exact derive is fully represented");
    let generated_methods = exact_facts
        .impls
        .iter()
        .filter(|implementation| {
            implementation.self_type.starts_with("ExactItem")
                && implementation.origin == FactOrigin::Schema
        })
        .flat_map(|implementation| implementation.items.iter().map(|item| item.name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        generated_methods,
        ["first", "second"],
        "exact impls and origins: {:#?}",
        exact_facts.impls,
    );

    let opaque_root = scratch.write(
        "opaque_consumer",
        "#![feature(no_core, lang_items, rustc_attrs, decl_macro)]\n#![allow(internal_features)]\n#![no_core]\n#[rustc_builtin_macro] macro derive($item:item) {}\n\
         extern crate fixture_macros;\n\
         #[derive(fixture_macros::Opaque)] pub struct OpaqueItem { pub field: u8 }\n",
    );
    let opaque_facts = read_crate(
        &CrateRead {
            edition: Some("2021"),
            items_only: true,
            loaded: Loaded { dependencies: &consumer_dependencies, ..Loaded::default() },
            ..CrateRead::new("opaque_consumer", eko::path::Path::new(&opaque_root))
        }
        .library(true),
    )
    .expect("the opaque derive consumer reads as a library");
    assert!(!opaque_facts.complete, "the opaque body is recorded as lost input");
    assert_eq!(frontend::rustc_const_eval::interp_cx_new_count(), before);
}

fn package_dirs(root: &Path, package: &str, found: &mut Vec<PathBuf>) {
    let package_name = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == package || name.strip_prefix(package).is_some_and(|suffix| suffix.starts_with('-')))
    };
    if package_name(root) {
        found.push(root.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if package_name(&path) {
            found.push(path);
            continue;
        }
        let Ok(nested) = std::fs::read_dir(path) else { continue };
        for entry in nested.flatten() {
            let path = entry.path();
            if path.is_dir() && package_name(&path) {
                found.push(path);
            }
        }
    }
}

fn registry_sources(package: &str, relative: &str) -> Vec<String> {
    let roots = std::env::var_os("FRONTEND_REGISTRY_SRC")
        .expect("FRONTEND_REGISTRY_SRC names registry source trees, separated by the platform path separator");
    let mut packages = Vec::new();
    for root in std::env::split_paths(&roots) {
        package_dirs(&root, package, &mut packages);
    }
    packages
        .into_iter()
        .filter_map(|package| std::fs::read_to_string(package.join(relative)).ok())
        .collect()
}

#[test]
#[ignore = "reads syn and quote from the registry tree FRONTEND_REGISTRY_SRC names"]
fn schema_registry_projection_sources() {
    let generics = registry_sources("syn", "src/generics.rs");
    let derive = registry_sources("syn", "src/derive.rs");
    let data = registry_sources("syn", "src/data.rs");
    let format = registry_sources("quote", "src/format.rs");

    assert!(generics.iter().any(|source| {
        source.contains("ImplGenerics(self)")
            && source.contains("TypeGenerics(self)")
            && source.contains("self.where_clause.as_ref()")
    }));
    assert!(derive.iter().any(|source| {
        source.contains("pub struct DeriveInput") && source.contains("pub ident: Ident")
    }));
    assert!(data.iter().any(|source| {
        source.contains("pub struct Field") && source.contains("pub ident: Option<Ident>")
    }));
    assert!(format.iter().any(|source| {
        source.contains("format_ident") && source.contains("Ident::new")
    }));
}
