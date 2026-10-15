//! The public analysis entry points, driven from outside the crate with a real catcher.
//!
//! The unit tests beside each module call internals, because a catcher needs `std` and the
//! library does not name it. These call `site_at`, `diagnose_with` and `analyze_source` the way
//! a caller does, on source that uses library items the `no_core` session cannot see, which is
//! the case every real file is.

use frontend::frontend_facts::{CrateFacts, analyze_source, site};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn ready() {
    frontend::unwind_janky::install_catcher(catcher);
}

const SOURCE: &str = "\
fn is_vowel(c: char) -> bool { \"aeiou\".contains(c) }
fn count(s: &str) -> usize { let n = s.chars().filter(|c| is_vowel(*c)).count(); n }
struct Point { x: f64, y: f64 }
impl Point { fn norm(&self) -> f64 { (self.x * self.x + self.y * self.y).sqrt() } }
";

#[test]
fn a_real_file_keeps_its_facts_when_its_bodies_do_not_type_check() {
    ready();
    let facts = match analyze_source("sample", SOURCE) {
        Ok(f) => f,
        Err(e) => panic!("analyze_source refused: {e:?}"),
    };
    let names: Vec<&str> = facts.definitions.iter().map(|d| d.name.as_str()).collect();
    for name in ["is_vowel", "count", "Point", "norm"] {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }
    let count = facts.definitions.iter().find(|d| d.name == "count").expect("count");
    let signature = count.signature.as_ref().expect("a fn has a signature");
    assert_eq!(signature.params.len(), 1, "{signature:?}");
    assert_eq!(signature.ret.as_deref(), Some("usize"));
    let point = facts.definitions.iter().find(|d| d.name == "Point").expect("Point");
    assert_eq!(point.fields.len(), 2, "{point:?}");
    assert!(facts.impls.iter().any(|i| i.items.iter().any(|item| item.name == "norm")));
}

#[test]
fn an_unresolved_trait_impl_keeps_its_method_and_marks_visibility_incomplete() {
    ready();
    let source = "\
struct S;
impl absent::Trait for S { fn f(&self) {} }
";
    let facts = match analyze_source("unresolved_trait_visibility", source) {
        Ok(facts) => facts,
        Err(error) => panic!("analyze_source refused: {error:?}"),
    };

    let method = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "f")
        .expect("unresolved trait impl method remains in definitions");
    assert!(
        facts.unanalyzed_bodies.contains(&method.def_path),
        "the affected method body must be reported as unanalyzed: {:?}",
        facts.unanalyzed_bodies
    );
    assert!(!method.public, "unknown visibility must not be reported as public");
    assert!(!method.exported, "unknown visibility must not be reported as exported");
    assert!(!facts.complete, "an unresolved trait must not yield complete visibility facts");
    assert!(
        facts.diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("visibility for")
                && diagnostic.contains("trait path did not resolve")
                && diagnostic.contains("lower bounds")
        }),
        "missing explicit visibility limitation: {:?}",
        facts.diagnostics
    );
    assert!(
        facts.diagnostics.iter().any(|diagnostic| diagnostic.contains("absent")),
        "the original unresolved-trait compiler diagnostic was lost: {:?}",
        facts.diagnostics
    );
}

#[test]
fn unresolved_trait_impl_descendant_bodies_are_marked_unanalyzed() {
    ready();
    let source = "\
struct S;
impl absent::Trait for S {
    fn f(&self) {
        let _closure = || ();
        let _inline_const = const { 1u8 };
        let _array: [u8; 1] = [0u8];
    }
}
";
    let facts = match analyze_source("unresolved_trait_descendant_bodies", source) {
        Ok(facts) => facts,
        Err(error) => panic!("analyze_source refused: {error:?}"),
    };

    let method = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "f")
        .expect("unresolved trait impl method remains in definitions");
    let method_prefix = format!("{}::", method.def_path);
    let nested_unanalyzed =
        facts.unanalyzed_bodies.iter().filter(|path| path.starts_with(&method_prefix)).count();
    assert_eq!(
        nested_unanalyzed, 3,
        "the closure, inline const, and array-length const bodies must all be reported exactly once: {:?}",
        facts.unanalyzed_bodies,
    );
}

#[test]
fn a_resolved_trait_with_a_missing_member_does_not_report_a_visibility_gap() {
    ready();
    let source = "\
trait Contract { fn required(&self); }
struct S;
impl Contract for S { fn extra(&self) {} }
";
    let facts = match analyze_source("resolved_trait_missing_member", source) {
        Ok(facts) => facts,
        Err(error) => panic!("analyze_source refused: {error:?}"),
    };

    let extra = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "extra")
        .expect("the resolved-trait impl-only method remains in definitions");
    assert!(
        !facts.unanalyzed_bodies.contains(&extra.def_path),
        "a resolved trait's missing-member method must not be classified as unanalyzed: {:?}",
        facts.unanalyzed_bodies
    );
    assert!(
        !facts.diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("visibility for")
                && diagnostic.contains("trait path did not resolve")
        }),
        "a resolved trait path must not be classified as unresolved: {:?}",
        facts.diagnostics
    );
}

#[test]
fn analyzer_injected_prelude_facts_are_omitted_with_explicit_gaps() {
    ready();
    let source = "\
mod analysis_lang_item_prelude { pub struct User; }
use crate::analysis_lang_item_prelude::User;
struct Local;
impl Add for Local {
    type Output = Local;
    fn add(self, _rhs: Self) -> Self::Output {
        fn helper() {}
        helper();
        struct Nested;
        impl Nested { fn local() {} }
        Nested::local();
        mod nested_module {
            pub fn inner() {}
            pub use self::inner as alias;
            pub trait Contract { fn run(); }
        }
        nested_module::alias();
        impl nested_module::Contract for Nested { fn run() {} }
        Nested::run();
        macro_rules! local_macro { () => {} }
        local_macro!();
        let _ = User;
        self
    }
}
fn uses_injected_prelude() { let _ = User; let _ = Some(1u8); }
fn calls_add() { let _ = Local.add(Local); }
";
    let facts = match analyze_source("synthetic_prelude_provenance", source) {
        Ok(facts) => facts,
        Err(error) => panic!("analyze_source refused: {error:?}"),
    };

    let authored_module = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "analysis_lang_item_prelude")
        .expect("the authored similarly named module remains a source definition");
    assert!(
        source
            .get(authored_module.span.start as usize..authored_module.span.end as usize)
            .is_some_and(|text| text.starts_with("mod analysis_lang_item_prelude"))
    );

    let authored_import = facts
        .imports
        .iter()
        .find(|import| import.path == "crate::analysis_lang_item_prelude::User")
        .expect("the authored import remains a source fact");
    assert!(
        source
            .get(authored_import.span.start as usize..authored_import.span.end as usize)
            .is_some_and(|text| text.starts_with("use crate::analysis_lang_item_prelude::User;"))
    );
    assert!(
        facts.references.iter().any(|reference| {
            reference.to_def_path.ends_with("analysis_lang_item_prelude::User")
        }),
        "the authored import's independently proven target remains a reference: {:?}",
        facts.references
    );
    assert!(
        facts.imports.iter().all(|import| import.path != "__analysis_lang_item_prelude::*"),
        "the appended analyzer prelude import must not be exposed: {:?}",
        facts.imports
    );

    assert!(
        facts
            .definitions
            .iter()
            .all(|definition| { !matches!(definition.name.as_str(), "Option" | "Some" | "Add") }),
        "appended analyzer definitions must not be reported: {:?}",
        facts
            .definitions
            .iter()
            .map(|definition| (&definition.name, &definition.span))
            .collect::<Vec<_>>()
    );
    assert!(
        facts.modules.iter().flat_map(|module| &module.names).all(|name| {
            !name.target_identity_complete || !name.target_def_path.contains("__analysis_")
        }),
        "module names must not present placeholder-tainted identities as complete: {:?}",
        facts.modules
    );
    assert!(
        facts.references.iter().all(|reference| !reference.to_def_path.ends_with("::Some")),
        "references directly to the injected Some placeholder must be omitted: {:?}",
        facts.references
    );
    let authored_impl = facts
        .impls
        .iter()
        .find(|implementation| implementation.self_type == "Local")
        .expect("the authored impl remains a source fact");
    assert_eq!(authored_impl.trait_ref.as_deref(), Some("Add"));
    assert_eq!(
        authored_impl.trait_def_path, None,
        "the injected trait path must not be presented as the resolved dependency target"
    );
    assert!(!authored_impl.identity_complete);
    assert!(!authored_impl.trait_identity_complete);
    let impl_output = authored_impl
        .items
        .iter()
        .find(|item| item.name == "Output")
        .expect("the authored associated type remains in the impl");
    let impl_add = authored_impl
        .items
        .iter()
        .find(|item| item.name == "add")
        .expect("the authored associated method remains in the impl");
    assert!(!impl_output.identity_complete);
    assert!(!impl_add.identity_complete);

    let authored_output = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "Output")
        .expect("the authored associated type remains a definition");
    let authored_add = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "add")
        .expect("the authored associated method remains a definition");
    assert!(!authored_output.identity_complete);
    assert!(!authored_add.identity_complete);
    assert!(authored_output.def_path.contains("__analysis_lang_items::Add"));
    assert!(authored_add.def_path.contains("__analysis_lang_items::Add"));
    assert_eq!(impl_output.def_path, authored_output.def_path);
    assert_eq!(impl_add.def_path, authored_add.def_path);
    assert!(
        source
            .get(authored_output.span.start as usize..authored_output.span.end as usize)
            .is_some_and(|text| text.starts_with("type Output"))
    );
    assert!(
        source
            .get(authored_add.span.start as usize..authored_add.span.end as usize)
            .is_some_and(|text| text.starts_with("fn add"))
    );

    let authored_user = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "User")
        .expect("the user-authored imported type remains a definition");
    let authored_local = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "Local")
        .expect("the user-authored receiver type remains a definition");
    assert!(authored_user.identity_complete);
    assert!(authored_local.identity_complete);
    let reference_from_placeholder_impl = facts
        .references
        .iter()
        .find(|reference| {
            reference.from_def_path == authored_add.def_path
                && reference.to_def_path == authored_user.def_path
        })
        .expect("the source reference from the authored impl body remains");
    assert!(!reference_from_placeholder_impl.from_identity_complete);
    assert!(reference_from_placeholder_impl.to_identity_complete);
    let helper = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "helper")
        .expect("the authored local helper remains a definition");
    assert!(!helper.identity_complete);
    let local_method = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "local")
        .expect("the authored nested impl method remains a definition");
    assert!(!local_method.identity_complete);
    let nested_impl = facts
        .impls
        .iter()
        .find(|implementation| {
            implementation.trait_ref.is_none()
                && implementation.items.iter().any(|item| item.def_path == local_method.def_path)
        })
        .expect("the authored nested inherent impl remains a fact");
    assert!(!nested_impl.identity_complete);
    let nested_item = nested_impl
        .items
        .iter()
        .find(|item| item.name == "local")
        .expect("the nested inherent method remains joined to its impl");
    assert!(!nested_item.identity_complete);
    assert_eq!(nested_item.def_path, local_method.def_path);
    let nested_module = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "nested_module")
        .expect("the authored nested module remains a definition");
    assert!(!nested_module.identity_complete);
    let nested_trait = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "Contract")
        .expect("the authored nested trait remains a definition");
    assert!(!nested_trait.identity_complete);
    let nested_trait_impl = facts
        .impls
        .iter()
        .find(|implementation| {
            implementation.trait_ref.as_deref() == Some("nested_module::Contract")
        })
        .expect("the authored nested trait impl remains a fact");
    assert!(!nested_trait_impl.identity_complete);
    assert_eq!(nested_trait_impl.trait_def_path.as_deref(), Some(nested_trait.def_path.as_ref()));
    assert!(!nested_trait_impl.trait_identity_complete);
    let module_facts = facts
        .modules
        .iter()
        .find(|module| module.module_def_path == nested_module.def_path)
        .expect("the authored nested module's visible names remain available");
    assert!(!module_facts.module_identity_complete);
    let nested_alias = module_facts
        .names
        .iter()
        .find(|name| name.name == "alias")
        .expect("the authored module alias remains a visible name");
    assert!(!nested_alias.target_identity_complete);
    let nested_import = facts
        .imports
        .iter()
        .find(|import| {
            import.module_def_path == nested_module.def_path
                && import.bindings.iter().any(|binding| binding == "alias")
        })
        .expect("the authored nested module import remains a source fact");
    assert!(!nested_import.module_identity_complete);
    let local_macro = facts
        .macros
        .iter()
        .find(|macro_facts| macro_facts.name == "local_macro")
        .expect("the authored local macro remains a source fact");
    assert!(!local_macro.identity_complete);
    let helper_reference = facts
        .references
        .iter()
        .find(|reference| {
            reference.from_def_path == authored_add.def_path
                && reference.to_def_path == helper.def_path
        })
        .expect("the authored call to the local helper remains a reference");
    assert!(!helper_reference.from_identity_complete);
    assert!(!helper_reference.to_identity_complete);
    let nested_method_reference = facts
        .references
        .iter()
        .find(|reference| {
            reference.from_def_path == authored_add.def_path
                && reference.to_def_path == local_method.def_path
        })
        .expect("the authored call to the nested method remains a reference");
    assert!(!nested_method_reference.from_identity_complete);
    assert!(!nested_method_reference.to_identity_complete);
    let nested_module_reference = facts
        .references
        .iter()
        .find(|reference| {
            reference.from_def_path == authored_add.def_path
                && reference.to_def_path == nested_alias.target_def_path
        })
        .expect("the call to the nested module alias remains a reference");
    assert!(!nested_module_reference.from_identity_complete);
    assert!(!nested_module_reference.to_identity_complete);
    let calls_add = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "calls_add")
        .expect("the authored trait-call body remains a definition");
    assert!(
        !facts.references.iter().any(|reference| {
            reference.from_def_path == calls_add.def_path
                && reference.to_def_path == authored_add.def_path
        }),
        "a method call resolved through the injected Add trait must not be rewritten as dispatch to the source impl: {:?}",
        facts.references
    );

    for span in facts
        .definitions
        .iter()
        .map(|definition| &definition.span)
        .chain(facts.imports.iter().map(|import| &import.span))
        .chain(facts.impls.iter().map(|implementation| &implementation.span))
        .chain(facts.references.iter().map(|reference| &reference.span))
    {
        assert!(span.start <= span.end, "reversed source span: {span:?}");
        assert!(
            span.end as usize <= source.len(),
            "fact span extends beyond the original input: {span:?}, source bytes {}",
            source.len()
        );
    }

    assert!(!facts.complete, "using injected placeholders is not a complete source analysis");
    assert!(
        facts.diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("module names")
                && diagnostic.contains("source-visible names may be missing")
        }),
        "removed injected names must not be treated as proving absence: {:?}",
        facts.diagnostics
    );
    assert!(
        facts.diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("references resolved to analyzer-injected lang-item placeholders")
        }),
        "the omitted source-to-placeholder reference must make incompleteness explicit: {:?}",
        facts.diagnostics
    );
    let identity_diagnostic = facts
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.contains(
                "compiler identities affected by analyzer-injected lang-item placeholders",
            )
        })
        .expect("the compiler path limitation is explicit");
    for path in [
        &authored_impl.def_path,
        &authored_output.def_path,
        &authored_add.def_path,
        &nested_impl.def_path,
        &nested_item.def_path,
        &nested_module.def_path,
        &nested_trait.def_path,
        nested_trait_impl.trait_def_path.as_ref().expect("resolved nested trait path"),
        &nested_alias.target_def_path,
        &nested_import.module_def_path,
        &local_macro.def_path,
        &helper.def_path,
        &local_method.def_path,
    ] {
        assert!(
            identity_diagnostic.contains(path.as_ref()),
            "{path} absent from {identity_diagnostic}"
        );
    }
}

#[test]
fn a_field_keeps_its_first_doc_line_without_type_checking_its_owner() {
    ready();
    let source = "\
struct Record {
    /// The first line.
    /// More detail.
    documented: u8,
    undocumented: u16,
}
fn broken() -> u8 { 1u16 }
";
    let facts = match analyze_source("field_docs", source) {
        Ok(facts) => facts,
        Err(error) => panic!("analyze_source refused: {error:?}"),
    };
    assert!(facts.definitions.iter().any(|definition| definition.name == "Record"));
    assert!(facts.definitions.iter().any(|definition| definition.name == "broken"));
    assert!(facts.diagnostics.iter().any(|diagnostic| diagnostic.contains("mismatched types")));
    let record =
        facts.definitions.iter().find(|definition| definition.name == "Record").expect("Record");
    assert_eq!(record.fields[0].doc.as_deref(), Some("The first line."));
    assert_eq!(record.fields[1].doc, None);
}

#[test]
fn the_site_of_an_offset_names_its_scope() {
    ready();
    let offset = SOURCE.find("n }").expect("marker") as u32;
    let found = site::site_at(SOURCE, offset).expect("a site");
    let names: Vec<&str> = found.scope.iter().map(|b| b.name.as_str()).collect();
    for name in ["s", "n", "is_vowel", "count"] {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }
    assert_eq!(found.enclosing_fn.as_ref().and_then(|f| f.ret.as_deref()), Some("usize"));
}

#[cfg(feature = "diagnostics")]
#[test]
fn diagnostics_refuse_only_what_the_file_decides() {
    use frontend::frontend_facts::diagnostics::{Options, diagnose_with};
    ready();
    let clean = diagnose_with(SOURCE, &Options::default()).expect("parses");
    assert!(
        !clean.iter().any(|d| d.code == "unresolved-ident"),
        "library calls must not read as unresolved: {clean:?}"
    );
    let broken = "fn f(a: u8) -> u8 { a }\nfn g() -> u8 { f(1, 2) + missing }\n";
    let found = diagnose_with(broken, &Options::default()).expect("parses");
    let codes: Vec<&str> = found.iter().map(|d| d.code.as_str()).collect();
    assert!(codes.contains(&"mismatched-arg-count"), "one spelling on the way out: {found:?}");
    assert!(codes.contains(&"unresolved-ident"), "one spelling on the way out: {found:?}");
    let by_number = Options { codes: Some(vec!["E0425".into()]), ..Options::default() };
    let picked = diagnose_with(broken, &by_number).expect("parses");
    assert!(
        picked.iter().all(|d| d.code == "unresolved-ident") && !picked.is_empty(),
        "{picked:?}"
    );
}

/// A staged-api crate's stability marks, as rustc's `lookup_stability` reads them: a stable
/// item carries none, an unstable one its feature, a trait's function inherits the trait's, and
/// a trait impl's function is the trait item's (what a call resolves to and what rustc checks),
/// whatever the impl itself says.
#[test]
fn a_definition_carries_the_feature_rustc_says_a_use_of_it_needs() {
    ready();
    let source = "\
#![feature(staged_api)]
#![stable(feature = \"t\", since = \"1.0.0\")]
#[unstable(feature = \"pat\", issue = \"none\")]
pub trait Pat { fn is_in(&self, at: u32) -> bool; }
#[stable(feature = \"t\", since = \"1.0.0\")]
pub struct S;
impl S {
    #[stable(feature = \"t\", since = \"1.0.0\")]
    pub fn plain(&self) {}
    #[unstable(feature = \"later\", issue = \"none\")]
    pub fn later(&self) {}
}
#[stable(feature = \"t\", since = \"1.0.0\")]
impl Pat for S { fn is_in(&self, at: u32) -> bool { true } }
";
    let facts = match analyze_source("staged", source) {
        Ok(f) => f,
        Err(e) => panic!("analyze_source refused: {e:?}"),
    };
    let unstable = |name: &str| -> Vec<Option<&str>> {
        facts.definitions.iter().filter(|d| d.name == name).map(|d| d.unstable.as_deref()).collect()
    };
    assert_eq!(unstable("S"), [None]);
    assert_eq!(unstable("plain"), [None]);
    assert_eq!(unstable("later"), [Some("later")]);
    assert_eq!(unstable("Pat"), [Some("pat")]);
    // The trait's own `is_in` and the impl's: both need `pat`.
    assert_eq!(unstable("is_in"), [Some("pat"), Some("pat")]);
}

#[test]
fn a_small_crate_records_written_generics_docs_and_impl_items() {
    ready();
    let source = r#"
pub trait Plain {}
pub struct Holder;
pub trait HasItem<T> {
    type Item;

    ///
    /// Converts a value to its item.
    fn convert<U: Plain>(&self, value: U) -> Self::Item
    where
        T: Plain;
}

impl<T> HasItem<T> for Holder
where
    T: Plain,
{
    type Item = char;

    fn convert<U: Plain>(&self, _: U) -> char
    where
        T: Plain,
    {
        'x'
    }
}

impl Holder {
    pub unsafe fn peek(&self, pointer: *const u8) -> u8 {
        unsafe { *pointer }
    }

    pub const fn zero() -> u8 {
        0
    }
}
"#;
    let facts = match analyze_source("written", source) {
        Ok(facts) => facts,
        Err(error) => panic!("analyze_source refused: {error:?}"),
    };

    let method = facts
        .definitions
        .iter()
        .find(|definition| definition.name == "convert" && definition.doc.is_some())
        .expect("documented trait method");
    assert_eq!(method.doc.as_deref(), Some("Converts a value to its item."));
    let signature = method.signature.as_ref().expect("method signature");
    assert_eq!(signature.generics, ["U"], "{signature:?}");
    // The inline bound and the `where` clause, each as written.
    assert_eq!(signature.predicates, ["U: Plain", "T: Plain"], "{signature:?}");
    assert!(!signature.unsafety && !signature.constness, "{signature:?}");

    // The header's `unsafe` and `const`, as written.
    let header = |name: &str| {
        let definition = facts
            .definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("{name} missing"));
        let signature = definition.signature.as_ref().expect("fn signature");
        (signature.unsafety, signature.constness)
    };
    assert_eq!(header("peek"), (true, false));
    assert_eq!(header("zero"), (false, true));

    let implementation = facts
        .impls
        .iter()
        .find(|implementation| implementation.items.iter().any(|item| item.name == "Item"))
        .expect("HasItem impl");
    assert_eq!(implementation.self_type, "Holder", "{implementation:?}");
    assert_eq!(implementation.trait_ref.as_deref(), Some("HasItem<T>"), "{implementation:?}");
    assert_eq!(implementation.generics, ["T"], "{implementation:?}");
    assert_eq!(implementation.predicates, ["T: Plain"], "{implementation:?}");
    let item =
        implementation.items.iter().find(|item| item.name == "Item").expect("associated type");
    assert_eq!(item.value.as_deref(), Some("char"));
}

#[test]
fn legacy_identity_flags_fail_closed_and_serialize_explicitly() {
    let mut facts: CrateFacts = serde_json::from_value(serde_json::json!({
        "crate_name": "legacy",
        "definitions": [{
            "def_path": "f",
            "name": "f",
            "kind": "fn",
            "span": { "file": "input.rs", "start": 0, "end": 1 }
        }],
        "imports": [{
            "module_def_path": "m",
            "path": "local::name",
            "span": { "file": "input.rs", "start": 0, "end": 1 },
            "reexport": false,
            "bindings": []
        }],
        "impls": [{
            "def_path": "impl",
            "self_type": "T",
            "span": { "file": "input.rs", "start": 0, "end": 1 },
            "items": [{ "name": "f", "kind": "assoc_fn", "def_path": "impl::f" }]
        }],
        "references": [{
            "from_def_path": "f",
            "to_def_path": "impl::f",
            "kind": "path",
            "span": { "file": "input.rs", "start": 0, "end": 1 }
        }],
        "modules": [{
            "module_def_path": "m",
            "names": [{
                "name": "T",
                "namespace": "type",
                "target_def_path": "T",
                "public": false
            }]
        }],
        "macros": [{
            "def_path": "m!",
            "name": "m",
            "exported": false,
            "builtin": false,
            "rules": ""
        }]
    }))
    .expect("pre-identity-flag facts still deserialize");

    assert!(facts.definitions[0].signature_settled);
    assert!(!facts.definitions[0].identity_complete);
    assert!(!facts.imports[0].module_identity_complete);
    assert!(!facts.impls[0].identity_complete);
    assert!(!facts.impls[0].trait_identity_complete);
    assert!(!facts.impls[0].items[0].identity_complete);
    assert!(!facts.references[0].from_identity_complete);
    assert!(!facts.references[0].to_identity_complete);
    assert!(!facts.modules[0].module_identity_complete);
    assert!(!facts.modules[0].names[0].target_identity_complete);
    assert!(!facts.macros[0].identity_complete);

    let serialized = serde_json::to_value(&facts).expect("identity flags serialize");
    for (object, key) in [
        (&serialized["definitions"][0], "identity_complete"),
        (&serialized["imports"][0], "module_identity_complete"),
        (&serialized["impls"][0], "identity_complete"),
        (&serialized["impls"][0], "trait_identity_complete"),
        (&serialized["impls"][0]["items"][0], "identity_complete"),
        (&serialized["references"][0], "from_identity_complete"),
        (&serialized["references"][0], "to_identity_complete"),
        (&serialized["modules"][0], "module_identity_complete"),
        (&serialized["modules"][0]["names"][0], "target_identity_complete"),
        (&serialized["macros"][0], "identity_complete"),
    ] {
        assert_eq!(object[key].as_bool(), Some(false), "missing explicit false field {key}");
    }

    facts.definitions[0].identity_complete = true;
    facts.imports[0].module_identity_complete = true;
    facts.impls[0].identity_complete = true;
    facts.impls[0].trait_identity_complete = true;
    facts.impls[0].items[0].identity_complete = true;
    facts.references[0].from_identity_complete = true;
    facts.references[0].to_identity_complete = true;
    facts.modules[0].module_identity_complete = true;
    facts.modules[0].names[0].target_identity_complete = true;
    facts.macros[0].identity_complete = true;
    let serialized = serde_json::to_value(&facts).expect("true identity flags serialize");
    for (object, key) in [
        (&serialized["definitions"][0], "identity_complete"),
        (&serialized["imports"][0], "module_identity_complete"),
        (&serialized["impls"][0], "identity_complete"),
        (&serialized["impls"][0], "trait_identity_complete"),
        (&serialized["impls"][0]["items"][0], "identity_complete"),
        (&serialized["references"][0], "from_identity_complete"),
        (&serialized["references"][0], "to_identity_complete"),
        (&serialized["modules"][0], "module_identity_complete"),
        (&serialized["modules"][0]["names"][0], "target_identity_complete"),
        (&serialized["macros"][0], "identity_complete"),
    ] {
        assert_eq!(object[key].as_bool(), Some(true), "missing explicit true field {key}");
    }
}
