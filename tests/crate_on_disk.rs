//! A crate read from disk: its root file, a module in a file of its own, a `macro_rules!` that
//! writes an item, and the facts an API index reads from it (visibility, what a module makes
//! visible, variant shapes, macro rules).
//!
//! A `std` program, because the catcher needs `std`.

use frontend::frontend_facts::{Namespace, VariantKind, analyze_crate};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

const ROOT: &str = "\
#![no_std]
pub mod inner;
pub use inner::Thing;
macro_rules! make { ($n:ident) => { pub fn $n(a: u8, b: u8) -> u8 { a } }; }
make!(twice);
pub enum E { A(u8, u8), B, C { x: u8 } }
";

const INNER: &str = "\
pub struct Thing;
impl Thing { pub fn go(&self, x: u8) {} }
struct Hidden;
";

#[test]
fn a_crate_on_disk_is_read_with_its_modules_and_its_macros_expanded() {
    frontend::unwind_janky::install_catcher(catcher);
    let dir = std::env::temp_dir().join(format!("frontend-crate-on-disk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    std::fs::write(dir.join("lib.rs"), ROOT).expect("write lib.rs");
    std::fs::write(dir.join("inner.rs"), INNER).expect("write inner.rs");
    let root = dir.join("lib.rs");
    let facts = analyze_crate(
        "tiny",
        eko::path::Path::new(root.to_str().expect("a UTF-8 temp path")),
        None,
        None,
        Some("2021"),
        true,
        1,
    )
    .expect("the crate reads");
    std::fs::remove_dir_all(&dir).expect("remove the temp dir");

    let definition = |path: &str| {
        facts
            .definitions
            .iter()
            .find(|definition| &*definition.def_path == path)
            .unwrap_or_else(|| panic!("no definition {path}"))
    };
    // The module file was read, and its spans say so.
    assert!(definition("inner::Thing").span.file.ends_with("inner.rs"));
    // The macro wrote a function with two parameters.
    let twice = definition("twice");
    assert_eq!(twice.signature.as_ref().expect("a signature").params.len(), 2);
    assert!(twice.public && twice.exported);
    // A private type is neither.
    let hidden = definition("inner::Hidden");
    assert!(!hidden.public && !hidden.exported);
    // Variant shapes.
    let shapes = &definition("E").variant_shapes;
    assert_eq!(shapes.len(), 3);
    assert_eq!((shapes[0].kind, shapes[0].fields), (VariantKind::Tuple, 2));
    assert_eq!(shapes[1].kind, VariantKind::Unit);
    assert_eq!((shapes[2].kind, shapes[2].fields), (VariantKind::Struct, 1));
    // The root makes `Thing` visible, resolved to where it is defined.
    let root_names = facts
        .modules
        .iter()
        .find(|module| module.module_def_path.is_empty())
        .expect("the root module");
    assert!(root_names.names.iter().any(|name| name.name == "Thing"
        && name.namespace == Namespace::Type
        && &*name.target_def_path == "inner::Thing"
        && name.public));
    // The macro's rules, as written.
    let make = facts.macros.iter().find(|m| m.name == "make").expect("the macro");
    assert!(!make.exported && !make.builtin);
    assert!(make.rules.contains("$n:ident"), "{}", make.rules);
    // Items only: no body was checked.
    assert!(facts.references.is_empty() && facts.unanalyzed_bodies.is_empty());
}
