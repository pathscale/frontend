//! Data structures for representing parsed attributes in the Rust compiler.
//!
//! For detailed documentation about attribute processing,
//! see [rustc_attr_parsing](../rustc_attr_parsing/index.html).

// tidy-alphabetical-start
// tidy-alphabetical-end

// Real paths, not the diagnostics trait parameter: `impl PrintAttribute for PathBuf`, and
// a `PathBuf` field on the graphviz-output option. Goes when those do.

// ---------------------------------------------------------------------------------------------
// STD IS BANNED IN THIS CRATE.
//
// `#![no_std]` above is the ban and the compiler is the enforcer: without `extern crate std;`
// there is no `std` in the extern prelude, so any `std::` path fails to resolve and the build
// stops. Do not add that line back to make an error go away - the error is the point. Whatever
// needed `std` either has a `core`/`alloc` equivalent, belongs in `ekostd`, or is a
// dependency that has to be replaced.
//
// The prelude is the part a grep cannot see: `Vec`, `String`, `Box`, `format!`, `vec!`,
// `thread_local!` and `println!` name no path. Under `#![no_std]` they resolve through `alloc`
// and `eko` instead, which is why those imports appear at the top of every file here.
// ---------------------------------------------------------------------------------------------
#[macro_use]
pub use attr::*;
pub use data_structures::*;
pub use encode_cross_crate::EncodeCrossCrate;
pub use lang_items::*;
pub use pretty_printing::PrintAttribute;
pub use stability::*;

mod attr;
mod canonical_symbols;
mod data_structures;
pub mod diagnostic;
pub mod diagnostic_items;
mod encode_cross_crate;
pub mod lang_items;
mod pretty_printing;
mod stability;
pub mod target;
pub mod weak_lang_items;

/// A trait for types that can provide a list of attributes given a `TyCtxt`.
///
/// It is an implementation detail of the [`find_attr!`] macro to be able to accept either a
/// [`DefId`], [`LocalDefId`], [`OwnerId`], or [`HirId`]. It is defined here with a generic `Tcx`
/// because this crate can't depend on `rustc_middle`. The concrete implementations are in
/// `rustc_middle`.
///
/// Not to be confused with [`crate::rustc_ast::ast_traits::HasAttrs`].
///
/// [`DefId`]: crate::rustc_span::def_id::DefId
/// [`LocalDefId`]: crate::rustc_span::def_id::LocalDefId
/// [`OwnerId`]: ../rustc_hir/struct.OwnerId.html
/// [`HirId`]: ../rustc_hir/struct.HirId.html
pub trait HasAttrs<'tcx, Tcx> {
    fn get_attrs(self, tcx: &Tcx) -> &'tcx [crate::rustc_attr_ir::Attribute];
}

/// Finds attributes by pattern matching.
///
/// A little like `matches` but for attributes.
///
/// Note that this macro accepts several "id" types: [`DefId`], [`LocalDefId`], [`OwnerId`] and
/// [`HirId`].
///
/// # Examples
///
/// It is most commonly used to check whether something has an attribute or to get its contents
/// if it is present:
/// ```rust,ignore (illustrative)
/// let is_naked: bool = find_attr!(tcx, def_id, Naked(..));
///
/// let is_visible: bool = find_attr!(tcx, def_id, Doc(doc) if doc.hidden.is_none());
///
/// let link_name: Option<Symbol> = find_attr!(tcx, def_id, LinkName { name, .. } => *name);
/// ```
///
/// Another common case is finding attributes applied to the root of the current crate.
/// For that, use the shortcut:
///
/// ```rust, ignore (illustrative)
/// find_attr!(tcx, crate, <pattern>)
/// ```
///
/// If you already have a list of attributes in scope, you can also use that:
///
/// ```rust,ignore (illustrative)
/// let attrs = <list of attributes>;
///
/// // finds the repr attribute
/// if let Some(r) = find_attr!(attrs, Repr(r) => r) {
///
/// }
///
/// // checks if one has matched
/// if find_attr!(attrs, Repr(_)) {
///
/// }
/// ```
///
/// [`DefId`]: crate::rustc_span::def_id::DefId
/// [`LocalDefId`]: crate::rustc_span::def_id::LocalDefId
/// [`OwnerId`]: ../rustc_hir/struct.OwnerId.html
/// [`HirId`]: ../rustc_hir/struct.HirId.html
#[macro_export]
macro_rules! find_attr {
    ($tcx: expr, crate, $pattern: pat $(if $guard: expr)?) => {
        $crate::find_attr!($tcx, crate, $pattern $(if $guard)? => ()).is_some()
    };
    ($tcx: expr, crate, $pattern: pat $(if $guard: expr)? => $e: expr) => {
        $crate::find_attr!($tcx.hir_krate_attrs(), $pattern $(if $guard)? => $e)
    };

    ($tcx: expr, $id: expr, $pattern: pat $(if $guard: expr)?) => {
        $crate::find_attr!($tcx, $id, $pattern $(if $guard)? => ()).is_some()
    };

    ($tcx: expr, $id: expr, $pattern: pat $(if $guard: expr)? => $e: expr) => {{
        $crate::find_attr!(
            $crate::rustc_attr_ir::HasAttrs::get_attrs($id, &$tcx),
            $pattern $(if $guard)? => $e
        )
    }};

    ($attributes_list: expr, $pattern: pat $(if $guard: expr)?) => {{
        $crate::find_attr!($attributes_list, $pattern $(if $guard)? => ()).is_some()
    }};

    ($attributes_list: expr, $pattern: pat $(if $guard: expr)? => $e: expr) => {{
        'done: {
            for i in $attributes_list {
                #[allow(unused_imports)]
                use $crate::rustc_attr_ir::AttributeKind::*;
                let i: &$crate::rustc_attr_ir::Attribute = i;
                match i {
                    $crate::rustc_attr_ir::Attribute::Parsed($pattern) $(if $guard)? => {
                        break 'done Some($e);
                    }
                    $crate::rustc_attr_ir::Attribute::Unparsed(..) => {}
                    // In lint emitting, there's a specific exception for this warning.
                    // It's not usually emitted from inside macros from other crates
                    // (see https://github.com/rust-lang/rust/issues/110613)
                    // But this one is!
                    #[deny(unreachable_patterns)]
                    _ => {}
                }
            }

            None
        }
    }};
}
pub use crate::find_attr;
