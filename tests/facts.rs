//! The public analysis entry points, driven from outside the crate with a real catcher.
//!
//! The unit tests beside each module call internals, because a catcher needs `std` and the
//! library does not name it. These call `site_at`, `diagnose_with` and `analyze_source` the way
//! a caller does, on source that uses library items the `no_core` session cannot see, which is
//! the case every real file is.

use frontend::frontend_facts::{analyze_source, site};

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
    let facts = match analyze_source("sample", SOURCE) { Ok(f) => f, Err(e) => panic!("analyze_source refused: {e:?}") };
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
    assert!(picked.iter().all(|d| d.code == "unresolved-ident") && !picked.is_empty(), "{picked:?}");
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
    let facts = match analyze_source("staged", source) { Ok(f) => f, Err(e) => panic!("analyze_source refused: {e:?}") };
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
