//! Compiler-true facts extracted from `TyCtxt`.
//!
//! An additive module: no caller's concepts live here, and nothing under `rustc_*` is
//! edited to serve it. A fact this module reports is a fact rustc would use.
//! Capabilities in a caller's adapter stay false until those facts are proven there.
//!
//! `run_compiler` is still batch-shaped. A caller that indexes immutable snapshots
//! needs one analysis per snapshot, so that is the first milestone: extract, then drop the
//! `TyCtxt`. A resident holder that outlives one call is a later increment, not
//! required to emit definitions, references, imports, impls, or trait impls.
//!
//! Fatal rustc errors used to abort the process. [`analyze_source`] wraps the
//! run in [`crate::rustc_span::fatal_error::catch_fatal_errors`] so a program
//! with no HIR is `Err`, not a dead daemon, and wraps each body's type check the
//! same way so a program that merely has errors still yields its facts. Both
//! need `panic = "unwind"`.
//!
//! These types serialize, so a caller can also take them as JSON from `frontend-facts`.
//! The crate builds on stable 1.97.1, so a caller can equally link it.
//!
//! Within-crate first. A sysroot is optional: when present it is the library
//! tree this session reads, when absent rustc's default search is used. The
//! `force_pinned_sysroot` cargo feature is the opt-in vintage pin; without it
//! the session claims the version the chosen sysroot actually carries.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them.
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;

use crate::rustc_hir::def::DefKind;
use crate::rustc_hir::def_id::{LOCAL_CRATE, LocalDefId};
use crate::rustc_hir::intravisit::{self, Visitor};
use crate::rustc_hir::{self as hir, ExprKind, ItemKind, Node, UseKind};
use crate::rustc_interface::{Config, create_and_enter_global_ctxt, parse, run_compiler};
#[cfg(not(feature = "force_pinned_sysroot"))]
use crate::rustc_interface::util::rustc_version_of_sysroot;
use crate::rustc_middle::ty::{TyCtxt, TypeVisitableExt};
use crate::rustc_feature::UnstableFeatures;
use crate::rustc_session::config::{Input, Options, Sysroot};
use crate::rustc_span::fatal_error::{FatalError, catch_fatal_errors};
use crate::rustc_span::{FileName, Span};
use crate::rustc_structures::CrateType;
use serde::{Deserialize, Serialize};

/// Byte range inside one source file, relative to that file's start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ByteSpan {
    pub file: String,
    pub start: u32,
    pub end: u32,
}

/// Kind of a named definition. Anonymous compiler items are omitted, not guessed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Fn,
    AssocFn,
    Struct,
    Enum,
    Union,
    Variant,
    Trait,
    TraitAlias,
    Mod,
    Const,
    Static,
    TyAlias,
    AssocTy,
    AssocConst,
    Macro,
    Field,
    Closure,
    Other,
}

/// One named definition rustc can prove.
///
/// The shape fields are read from the HIR and the source map, never from inference, so they
/// are present for a definition whose bodies do not type check. Each is filled only for the
/// kinds it describes and is empty for every other kind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub def_path: String,
    pub name: String,
    pub kind: FactKind,
    pub span: ByteSpan,
    /// For `Fn` and `AssocFn`: the signature as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<FnSignature>,
    /// For `Struct` and `Union`: each field in declaration order. A tuple struct's fields are
    /// named `0`, `1` and so on, which is how a field access names them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldSignature>,
    /// For `Enum`: each variant's name in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
}

/// A function's signature, as source text rather than as types.
///
/// Text is what a reader of the signature sees, and it exists before and without type
/// checking: an unresolved parameter type still has a spelling. Types that no source text
/// covers, such as one a macro assembled, are printed from the HIR instead.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FnSignature {
    /// The `self` parameter, when there is one. It is not repeated in `params`.
    pub receiver: Option<Receiver>,
    /// Every parameter after the receiver, in order.
    pub params: Vec<FnParam>,
    /// The written return type. `None` for a function that writes none, which returns `()`.
    pub ret: Option<String>,
}

/// One non-receiver parameter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FnParam {
    /// The pattern as written, `x` or `(a, b)` or `mut n`. A declaration with no body (a
    /// required trait method, a foreign function) has only a name, or `_` when it has none.
    pub pat: String,
    pub ty: String,
}

/// How a method takes `self`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiverKind {
    /// `self` or `mut self`.
    Value,
    /// `&self`, with or without a lifetime.
    Ref,
    /// `&mut self`, with or without a lifetime.
    RefMut,
    /// `self: T`, with the type written out. `Receiver::ty` says what `T` is.
    Typed,
}

/// A method's `self` parameter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Receiver {
    pub kind: ReceiverKind,
    /// The written type for [`ReceiverKind::Typed`], such as `Box<Self>`; `None` for the three
    /// shorthand forms, whose type the kind already says.
    pub ty: Option<String>,
}

/// One field of a struct or union.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FieldSignature {
    pub name: String,
    pub ty: String,
}

/// One `use` / `pub use`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Import {
    pub module_def_path: String,
    pub path: String,
    pub span: ByteSpan,
    pub reexport: bool,
    /// Names this `use` binds. `use crate::foo as bar` is `["bar"]`.
    /// Empty for a glob. `ListStem` items are omitted, not listed with no names.
    pub bindings: Vec<String>,
}

/// One `impl` block, inherent or trait.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Impl {
    pub def_path: String,
    pub self_type: String,
    pub trait_def_path: Option<String>,
    pub span: ByteSpan,
    /// The associated items this block defines, in source order. Items a trait impl inherits
    /// from the trait's defaults are not here: this block does not define them.
    #[serde(default)]
    pub items: Vec<ImplItem>,
}

/// One associated item an `impl` block defines. Its full [`Definition`] is in
/// [`CrateFacts::definitions`] under the same `def_path`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImplItem {
    pub name: String,
    pub kind: FactKind,
    pub def_path: String,
}

/// Kind of a resolved use of a definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Path,
    Method,
}

/// One resolved reference. Locals and primitives are omitted: they are not defs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    pub from_def_path: String,
    pub to_def_path: String,
    pub kind: RefKind,
    pub span: ByteSpan,
}

/// Facts for one crate after analysis.
///
/// **Facts survive errors.** Definitions, imports and impls come from name resolution and the
/// HIR, which exist for any program that parses and expands; they are reported even when no
/// body type checks. References need a body's type check, so they are collected per body and
/// a body that could not be checked contributes none. `complete` says whether anything was
/// lost that way, and `diagnostics` says why.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CrateFacts {
    pub crate_name: String,
    pub definitions: Vec<Definition>,
    pub imports: Vec<Import>,
    pub impls: Vec<Impl>,
    pub references: Vec<Reference>,
    /// Every error the frontend emitted, one string each, in emission order, with its
    /// location lines. The closing "aborting due to" summary is left out: it counts the others
    /// rather than saying anything of its own.
    #[serde(default)]
    pub diagnostics: Vec<String>,
    /// Def paths of bodies whose type check stopped on a fatal error, so none of their
    /// references are listed. A body whose type check merely reported errors is not here: its
    /// references that did resolve are kept.
    #[serde(default)]
    pub unanalyzed_bodies: Vec<String>,
    /// No error was emitted and every body was type checked. When false, `references` is a
    /// lower bound and the definitions, imports and impls are unaffected.
    ///
    /// Serialized facts from before this field existed came only from runs with no error, so
    /// a missing field reads as true. [`Default`] is false: an empty value nobody filled in is
    /// not a finished analysis.
    #[serde(default = "complete_when_absent")]
    pub complete: bool,
}

fn complete_when_absent() -> bool {
    true
}

/// Map a rustc `DefKind` to a fact kind. `None` means handled by another list
/// (`use` → imports, `impl` → impls) or not an addressable name.
pub fn fact_kind(kind: DefKind) -> Option<FactKind> {
    Some(match kind {
        DefKind::Fn => FactKind::Fn,
        DefKind::AssocFn => FactKind::AssocFn,
        DefKind::Struct => FactKind::Struct,
        DefKind::Enum => FactKind::Enum,
        DefKind::Union => FactKind::Union,
        DefKind::Variant => FactKind::Variant,
        DefKind::Trait => FactKind::Trait,
        DefKind::TraitAlias => FactKind::TraitAlias,
        DefKind::Mod => FactKind::Mod,
        DefKind::Const { .. } => FactKind::Const,
        DefKind::Static { .. } => FactKind::Static,
        DefKind::TyAlias => FactKind::TyAlias,
        DefKind::AssocTy => FactKind::AssocTy,
        DefKind::AssocConst { .. } => FactKind::AssocConst,
        DefKind::Macro(_) => FactKind::Macro,
        DefKind::Field => FactKind::Field,
        DefKind::Closure => FactKind::Closure,
        DefKind::ForeignTy => FactKind::Other,
        DefKind::Use
        | DefKind::Impl { .. }
        | DefKind::ExternCrate
        | DefKind::ForeignMod
        | DefKind::AnonConst
        | DefKind::OpaqueTy
        | DefKind::TyParam
        | DefKind::ConstParam
        | DefKind::LifetimeParam
        | DefKind::GlobalAsm
        | DefKind::Ctor(..)
        | DefKind::SyntheticCoroutineBody
        | DefKind::TestBinderConstraints => return None,
    })
}

/// Extract facts from an already-built `TyCtxt`. Runs type checking, one body at a time.
///
/// Resolution and HIR facts are taken first and do not depend on any body. Each body's type
/// check then runs inside its own [`catch_fatal_errors`], so one body that stops on a fatal
/// error (a missing lang item under `no_core` is the usual one) costs that body's references
/// and nothing else. The query system poisons a query that unwound, so a later body whose type
/// check needs the same result stops too, and is caught the same way.
///
/// `diagnostics` is left empty here: the emitter belongs to whoever built the session, and
/// only [`analyze_source`] installed one it can read back.
pub fn extract(tcx: TyCtxt<'_>) -> CrateFacts {
    let crate_name = tcx.crate_name(LOCAL_CRATE).to_string();
    let mut facts = CrateFacts { crate_name, ..CrateFacts::default() };

    for local in tcx.iter_local_def_id() {
        let def_id = local.to_def_id();
        let kind = tcx.def_kind(def_id);
        if let DefKind::Impl { .. } = kind {
            facts.impls.push(impl_fact(tcx, local));
            continue;
        }
        let Some(kind) = fact_kind(kind) else {
            continue;
        };
        let Some(name) = tcx.opt_item_name(def_id) else {
            continue;
        };
        let mut definition = Definition {
            def_path: tcx.def_path_str(def_id),
            name: name.to_string(),
            kind,
            span: byte_span(tcx, tcx.def_span(def_id)),
            signature: None,
            fields: Vec::new(),
            variants: Vec::new(),
        };
        describe_shape(tcx, local, &mut definition);
        facts.definitions.push(definition);
    }

    for item_id in tcx.hir_free_items() {
        let item = tcx.hir_item(item_id);
        let ItemKind::Use(path, use_kind) = item.kind else {
            continue;
        };
        let path_str = path
            .segments
            .iter()
            .map(|seg| seg.ident.as_str())
            .filter(|s| *s != "{{root}}")
            .collect::<Vec<_>>()
            .join("::");
        // Degenerate `use foo::{}` exists so rustc can gate features. It binds nothing.
        let (path_str, bindings) = match use_kind {
            UseKind::Glob => (format!("{path_str}::*"), Vec::new()),
            UseKind::Single(ident) => (path_str, vec![ident.as_str().to_string()]),
            UseKind::ListStem => continue,
        };
        let module = tcx.parent_module_from_def_id(item.owner_id.def_id);
        facts.imports.push(Import {
            module_def_path: tcx.def_path_str(module.to_def_id()),
            path: path_str,
            span: byte_span(tcx, item.span),
            reexport: tcx.local_visibility(item.owner_id.def_id).is_public(),
            bindings,
        });
    }

    for owner in tcx.hir_body_owners() {
        let Some(body) = tcx.hir_maybe_body_owned_by(owner) else {
            continue;
        };
        let from = tcx.def_path_str(owner.to_def_id());
        // Results tainted by an error are still read: a path that resolved is a fact whether
        // or not some other expression in the body failed, and one that did not resolve is
        // `Res::Err`, which `RefVisitor` already skips.
        let Ok(typeck) = catch_fatal_errors(|| tcx.typeck(owner)) else {
            facts.unanalyzed_bodies.push(from);
            continue;
        };
        let mut visitor = RefVisitor { tcx, typeck, from: &from, refs: &mut facts.references };
        visitor.visit_expr(body.value);
    }
    facts.complete = tcx.dcx().has_errors().is_none() && facts.unanalyzed_bodies.is_empty();

    facts.definitions.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.def_path.cmp(&b.def_path)));
    facts.imports.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.path.cmp(&b.path)));
    facts.impls.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.def_path.cmp(&b.def_path)));
    facts.references.sort_by(|a, b| {
        a.span.start.cmp(&b.span.start).then(a.to_def_path.cmp(&b.to_def_path))
    });
    facts
}

/// One `impl` block's fact.
///
/// `type_of` is a query that can stop on a fatal error of its own, and an impl whose self type
/// did not resolve comes back as an error type that prints as nothing useful. Either way the
/// type is reported as written in the source, so an impl is never dropped for its self type.
fn impl_fact(tcx: TyCtxt<'_>, local: LocalDefId) -> Impl {
    let def_id = local.to_def_id();
    let hir_impl = match tcx.hir_node_by_def_id(local) {
        Node::Item(hir::Item { kind: ItemKind::Impl(hir_impl), .. }) => Some(hir_impl),
        _ => None,
    };
    let self_type = catch_fatal_errors(|| {
        let ty = tcx.type_of(def_id).instantiate_identity().skip_normalization();
        (!ty.references_error()).then(|| ty.to_string())
    })
    .ok()
    .flatten()
    .or_else(|| hir_impl.map(|hir_impl| type_text(tcx, hir_impl.self_ty)))
    .unwrap_or_default();
    let trait_def_path = catch_fatal_errors(|| tcx.impl_opt_trait_id(def_id))
        .ok()
        .flatten()
        .map(|trait_id| tcx.def_path_str(trait_id));
    let items = hir_impl
        .map(|hir_impl| {
            hir_impl
                .items
                .iter()
                .filter_map(|item| {
                    let item_id = item.owner_id.to_def_id();
                    Some(ImplItem {
                        name: tcx.opt_item_name(item_id)?.to_string(),
                        kind: fact_kind(tcx.def_kind(item_id))?,
                        def_path: tcx.def_path_str(item_id),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Impl {
        def_path: tcx.def_path_str(def_id),
        self_type,
        trait_def_path,
        span: byte_span(tcx, tcx.def_span(def_id)),
        items,
    }
}

/// Fill a definition's signature, fields or variants from its HIR node.
fn describe_shape(tcx: TyCtxt<'_>, local: LocalDefId, definition: &mut Definition) {
    match definition.kind {
        FactKind::Fn | FactKind::AssocFn => {
            definition.signature = fn_signature(tcx, tcx.hir_node_by_def_id(local));
        }
        FactKind::Struct | FactKind::Union => {
            if let Node::Item(hir::Item {
                kind: ItemKind::Struct(_, _, data) | ItemKind::Union(_, _, data),
                ..
            }) = tcx.hir_node_by_def_id(local)
            {
                definition.fields = data
                    .fields()
                    .iter()
                    .map(|field| FieldSignature {
                        name: field.ident.as_str().to_string(),
                        ty: type_text(tcx, field.ty),
                    })
                    .collect();
            }
        }
        FactKind::Enum => {
            if let Node::Item(hir::Item { kind: ItemKind::Enum(_, _, def), .. }) =
                tcx.hir_node_by_def_id(local)
            {
                definition.variants =
                    def.variants.iter().map(|variant| variant.ident.as_str().to_string()).collect();
            }
        }
        _ => {}
    }
}

/// A function's signature from its HIR node: a free or foreign `fn`, or a method in an impl
/// or a trait. `None` for any other node.
///
/// Parameter patterns come from the body when there is one, because only the body keeps them
/// intact; a declaration without a body (a required trait method, a foreign function) records
/// just a name per parameter.
fn fn_signature<'tcx>(tcx: TyCtxt<'tcx>, node: Node<'tcx>) -> Option<FnSignature> {
    let sig = node.fn_sig()?;
    let (body, names): (Option<&hir::Body<'_>>, &[Option<crate::rustc_span::Ident>]) = match node {
        Node::Item(hir::Item { kind: ItemKind::Fn { body, .. }, .. })
        | Node::ImplItem(hir::ImplItem { kind: hir::ImplItemKind::Fn(_, body), .. })
        | Node::TraitItem(hir::TraitItem {
            kind: hir::TraitItemKind::Fn(_, hir::TraitFn::Provided(body)),
            ..
        }) => (Some(tcx.hir_body(*body)), &[][..]),
        Node::TraitItem(hir::TraitItem {
            kind: hir::TraitItemKind::Fn(_, hir::TraitFn::Required(names)),
            ..
        })
        | Node::ForeignItem(hir::ForeignItem {
            kind: hir::ForeignItemKind::Fn(_, names, _), ..
        }) => (None, *names),
        _ => return None,
    };
    // The name a parameter binds, when its pattern is a plain name.
    let ident = |index: usize| match body {
        Some(body) => body.params.get(index).and_then(|param| match param.pat.kind {
            hir::PatKind::Binding(_, _, ident, None) => Some(ident),
            _ => None,
        }),
        None => names.get(index).copied().flatten(),
    };
    let pat = |index: usize| match body.and_then(|body| body.params.get(index)) {
        Some(param) => source_text(tcx, param.pat.span).unwrap_or_else(|| {
            let ann: &dyn crate::rustc_hir::intravisit::HirTyCtxt<'_> = &tcx;
            crate::rustc_hir_pretty::pat_to_string(&ann, param.pat)
        }),
        None => ident(index).map_or_else(|| "_".to_string(), |ident| ident.as_str().to_string()),
    };

    let decl = sig.decl;
    let receiver = match decl.implicit_self() {
        hir::ImplicitSelfKind::Imm | hir::ImplicitSelfKind::Mut => {
            Some(Receiver { kind: ReceiverKind::Value, ty: None })
        }
        hir::ImplicitSelfKind::RefImm => Some(Receiver { kind: ReceiverKind::Ref, ty: None }),
        hir::ImplicitSelfKind::RefMut => Some(Receiver { kind: ReceiverKind::RefMut, ty: None }),
        // Lowering records only the shorthand forms. `self: T` is a first parameter named
        // `self` with a type of its own, and a free function cannot have one.
        hir::ImplicitSelfKind::None => match (ident(0), decl.inputs.first()) {
            (Some(ident), Some(ty)) if ident.name == crate::rustc_span::kw::SelfLower => {
                Some(Receiver { kind: ReceiverKind::Typed, ty: Some(type_text(tcx, ty)) })
            }
            _ => None,
        },
    };
    let skip = usize::from(receiver.is_some());
    let params = decl
        .inputs
        .iter()
        .enumerate()
        .skip(skip)
        .map(|(index, ty)| FnParam { pat: pat(index), ty: type_text(tcx, ty) })
        .collect();
    // An `async fn` that writes no return type is lowered to a `Return` of an opaque type
    // whose span is the empty point where the type would go, so empty text means none written.
    let ret = match decl.output {
        hir::FnRetTy::Return(ty) => Some(type_text(tcx, ty)).filter(|text| !text.is_empty()),
        hir::FnRetTy::DefaultReturn(_) => None,
    };
    Some(FnSignature { receiver, params, ret })
}

/// The source text a span covers, or `None` when the source map has none for it.
fn source_text(tcx: TyCtxt<'_>, span: Span) -> Option<String> {
    tcx.sess.source_map().span_to_snippet(span).ok()
}

/// A HIR type as written. Falls back to the HIR pretty printer for a type with no source text,
/// such as one a macro assembled from pieces.
fn type_text(tcx: TyCtxt<'_>, ty: &hir::Ty<'_>) -> String {
    source_text(tcx, ty.span).unwrap_or_else(|| {
        let ann: &dyn crate::rustc_hir::intravisit::HirTyCtxt<'_> = &tcx;
        crate::rustc_hir_pretty::ty_to_string(&ann, ty)
    })
}

/// Analyse one crate from source.
///
/// A program with errors still has facts. Anything that leaves resolution and the HIR intact
/// (a type error, an unresolved name, a body that needs a lang item `no_core` does not have)
/// comes back as `Ok`, with the errors in [`CrateFacts::diagnostics`] and
/// [`CrateFacts::complete`] false. `Err(FatalError)` is kept for a program with no HIR to read:
/// one that does not parse, or whose macros do not expand. Diagnostics are captured, not
/// printed, in both cases. Equivalent to [`analyze_source_with_sysroot`] with `sysroot = None`.
pub fn analyze_source(crate_name: &str, source: &str) -> Result<CrateFacts, FatalError> {
    analyze_source_with_sysroot(crate_name, source, None)
}

/// Analyse one crate from source against an optional sysroot.
///
/// `sysroot` is defined when `Some`: that tree is this session's library root.
/// When `None`, an `FRONTEND_SYSROOT` env value is used if set, otherwise
/// rustc's default sysroot search.
///
/// **The tree has to have been built from this frontend's upstream commit.**
/// Crate metadata encodes every preinterned symbol as a bare index into the
/// `symbols!` table in `rustc_span::symbol` (`SYMBOL_PREDEFINED`, see
/// `rustc_metadata::rmeta::encoder::encode_symbol_or_byte_symbol`), and that
/// table changes commit to commit. A sysroot from any other commit decodes its
/// late symbols as whatever now sits at that index, so a crate whose name is a
/// preinterned symbol is not recognised and `E0463` says only "can't find
/// crate". Crates whose names sit before the first divergence still load, so a
/// mismatched sysroot fails partially rather than cleanly.
///
/// Without the `force_pinned_sysroot` feature, the session claims the version
/// string the chosen sysroot actually carries. With the feature, the compiled-in
/// `CFG_VERSION` is kept and other vintages are refused. Claiming the sysroot's
/// own string only silences the version check; it does not make the symbol table
/// agree, so it turns a loud `E0514` into a silent `E0463`. Prefer
/// `force_pinned_sysroot` unless you know the sysroot is from the pinned commit.
pub fn analyze_source_with_sysroot(
    crate_name: &str,
    source: &str,
    sysroot: Option<&str>,
) -> Result<CrateFacts, FatalError> {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "analyze_source needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let mut opts = Options::default();
    opts.crate_name = Some(crate_name.to_string());
    opts.crate_types = alloc::vec![CrateType::Rlib];
    // Options::default() disallows `#![feature]` the way a stable CLI would.
    // This crate is a nightly frontend; within-crate no_core analysis needs the
    // same gates nightly rustc has.
    opts.unstable_features = UnstableFeatures::Allow;
    // **The sysroot is an optional parameter, because this is a parser.**
    //
    // Definitions, imports, impls and within-crate references are read out of
    // the source this was handed. None of them needs a standard library to
    // exist. A library only matters to a caller who wants paths resolved into
    // it, and that caller says so by naming a sysroot.
    //
    // Nothing named means `no_core`, so no external crate is loaded, no prelude
    // import is resolved, and the facts come back with no library anywhere on
    // the machine. It goes through `crate_attr` rather than being prepended to
    // the source, because prepended text moves every byte span this crate
    // reports and those spans are the whole product.
    //
    // This used to fall back to `rustc --print sysroot`, which fetched a
    // library nobody had asked for and then failed to read it: crate metadata
    // encodes preinterned symbols as bare indices into `rustc_span::symbol`'s
    // table, nothing checks that two tables agree, and a table from another
    // commit decodes `std` as whatever now sits at that index. The result was a
    // bare `E0463` for a library the caller never wanted.
    let named = sysroot
        .map(eko::path::PathBuf::from)
        .or_else(|| eko::env::var_os("FRONTEND_SYSROOT").map(eko::path::PathBuf::from));
    let has_sysroot = named.is_some();
    if has_sysroot {
        opts.sysroot = Sysroot::new(named);
    } else {
        opts.unstable_opts.crate_attr.push("no_core".to_string());
        opts.unstable_opts.crate_attr.push("feature(no_core)".to_string());
    }
    let rustc_version = {
        #[cfg(feature = "force_pinned_sysroot")]
        {
            None
        }
        #[cfg(not(feature = "force_pinned_sysroot"))]
        {
            // Only worth asking when something will actually be read. With
            // `no_core` no metadata is opened, so there is no vintage to agree
            // with and no reason to spawn a compiler to ask about one.
            if has_sysroot {
                rustc_version_of_sysroot(opts.sysroot.path()).or_else(host_rustc_version)
            } else {
                None
            }
        }
    };
    let text = alloc::sync::Arc::new(eko::thread::Mutex::new(String::new()));
    let using_internal_features =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicBool::new(false)));
    let config = Config {
        opts,
        input: Input::Str {
            name: FileName::anon_source_code(source),
            input: source.to_string(),
        },
        psess_created: Some(capture_diagnostics(&text)),
        using_internal_features,
        rustc_version,
    };
    // The facts are handed out through `extracted` rather than returned, because returning is
    // not the way out of a run that emitted an error: `run_compiler` ends every such run in
    // `abort_if_errors`, which unwinds past the return value. What `extract` finished before
    // that is kept here, and the unwind only tells us the run had errors.
    let mut extracted: Option<CrateFacts> = None;
    let finished = catch_fatal_errors(|| {
        run_compiler(config, |compiler| {
            let krate = parse(&compiler.sess);
            create_and_enter_global_ctxt(compiler, krate, |tcx| extracted = Some(extract(tcx)))
        })
    });
    let mut facts = extracted.ok_or(FatalError)?;
    let (errors, _warnings) = split_diagnostics(&text.lock());
    facts.diagnostics =
        errors.into_iter().filter(|error| !error.starts_with("error: aborting due to")).collect();
    facts.complete = facts.complete && finished.is_ok() && facts.diagnostics.is_empty();
    Ok(facts)
}

/// What [`check_source`] found: every error and warning the frontend emitted, one string
/// each, in emission order.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Checked {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// The session stopped on a fatal error before the analysis finished. `errors` says why
    /// when the frontend said anything; when it said nothing this is the only signal.
    pub fatal: bool,
}

impl Checked {
    /// Nothing refused the program: no error, and the analysis ran to its end.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty() && !self.fatal
    }
}

/// A `fmt::Write` into a buffer the caller keeps a handle on, so the emitter can own one end.
struct Sink(alloc::sync::Arc<eko::thread::Mutex<String>>);

impl core::fmt::Write for Sink {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.lock().push_str(s);
        Ok(())
    }
}

/// The `psess_created` hook that sends every diagnostic to `text`, one short line each.
///
/// A library has no terminal to print to, and a diagnostic written to stderr is one the caller
/// cannot read back. So the session's emitter is replaced before anything is parsed, and what
/// it would have printed lands in a buffer the caller still holds.
fn capture_diagnostics(
    text: &alloc::sync::Arc<eko::thread::Mutex<String>>,
) -> alloc::boxed::Box<dyn FnOnce(&mut crate::rustc_session::parse::ParseSess) + Send> {
    let sink = text.clone();
    alloc::boxed::Box::new(move |psess: &mut crate::rustc_session::parse::ParseSess| {
        let emitter = crate::rustc_errors::plain_emitter::PlainEmitter::new()
            .sm(Some(psess.clone_source_map()))
            .short_message(true)
            .dst(alloc::boxed::Box::new(Sink(sink)));
        psess.set_emitter(alloc::boxed::Box::new(emitter));
    })
}

/// Split captured emitter output into errors and warnings, in emission order.
///
/// One diagnostic starts at a line with no leading space; its `-->` location lines follow.
/// Anything that is neither an error nor a warning (a `note`, "For more information") is
/// dropped: it annotates a diagnostic rather than being one.
fn split_diagnostics(captured: &str) -> (Vec<String>, Vec<String>) {
    fn flush(entry: Option<String>, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
        if let Some(entry) = entry {
            if entry.starts_with("error") {
                errors.push(entry);
            } else if entry.starts_with("warning") {
                warnings.push(entry);
            }
        }
    }
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut current: Option<String> = None;
    for line in captured.lines() {
        if line.starts_with(' ') {
            if let Some(entry) = current.as_mut() {
                entry.push('\n');
                entry.push_str(line);
            }
        } else {
            flush(current.take(), &mut errors, &mut warnings);
            current = Some(line.to_string());
        }
    }
    flush(current.take(), &mut errors, &mut warnings);
    (errors, warnings)
}

/// Type check, borrow check and lint one crate from source, and return what was said.
///
/// **Reading, never running.** This is `tcx.analysis(())`: typeck, then borrowck, then the
/// builtin lints (skipped when typeck or borrowck already failed). Nothing is compiled to a
/// binary and nothing is executed. It is how a caller establishes whether a program compiles
/// and lints clean without rustc, cargo or clippy.
///
/// Runs as `no_core`, like [`analyze_source`] with no sysroot: see rule zero in `AGENTS.md`.
/// Diagnostics go to this crate's `PlainEmitter`, one line each, captured rather than printed.
/// Needs a catcher installed through [`crate::unwind_janky::install_catcher`], because a
/// refused program ends in `abort_if_errors`, which unwinds.
pub fn check_source(crate_name: &str, source: &str) -> Checked {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "check_source needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let mut opts = Options::default();
    opts.crate_name = Some(crate_name.to_string());
    opts.crate_types = alloc::vec![CrateType::Rlib];
    opts.unstable_features = UnstableFeatures::Allow;
    opts.unstable_opts.crate_attr.push("no_core".to_string());
    opts.unstable_opts.crate_attr.push("feature(no_core)".to_string());

    let text = alloc::sync::Arc::new(eko::thread::Mutex::new(String::new()));
    let using_internal_features =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicBool::new(false)));
    let config = Config {
        opts,
        input: Input::Str {
            name: FileName::anon_source_code(source),
            input: source.to_string(),
        },
        psess_created: Some(capture_diagnostics(&text)),
        using_internal_features,
        rustc_version: None,
    };
    let finished = catch_fatal_errors(|| {
        run_compiler(config, |compiler| {
            let krate = parse(&compiler.sess);
            create_and_enter_global_ctxt(compiler, krate, |tcx| tcx.analysis(()))
        })
    });

    let (errors, warnings) = split_diagnostics(&text.lock());
    Checked { errors, warnings, fatal: finished.is_err() }
}

fn host_rustc_version() -> Option<alloc::string::String> {
    let out = eko::command::Command::new("rustc").arg("--version").output()?;
    if !out.success() {
        return None;
    }
    let line = alloc::string::String::from_utf8(out.stdout).ok()?;
    let line = line.trim();
    let version = line.strip_prefix("rustc ").unwrap_or(line).trim();
    (!version.is_empty()).then(|| version.to_string())
}

fn byte_span(tcx: TyCtxt<'_>, span: Span) -> ByteSpan {
    let sm = tcx.sess.source_map();
    let lo = sm.lookup_byte_offset(span.lo());
    let hi = sm.lookup_byte_offset(span.hi());
    ByteSpan {
        file: lo.sf.name.prefer_local_unconditionally().to_string(),
        start: lo.pos.0,
        end: hi.pos.0,
    }
}

struct RefVisitor<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    typeck: &'tcx crate::rustc_middle::ty::TypeckResults<'tcx>,
    from: &'a str,
    refs: &'a mut Vec<Reference>,
}

impl<'a, 'tcx> Visitor<'tcx> for RefVisitor<'a, 'tcx> {
    type NestedFilter = intravisit::IgnoreNested;
    type Result = ();
    fn visit_expr(&mut self, expr: &'tcx crate::rustc_hir::Expr<'tcx>) {
        match expr.kind {
            ExprKind::Path(ref qpath) => {
                if let Some(def_id) = self.typeck.qpath_res(qpath, expr.hir_id).opt_def_id() {
                    self.push(def_id, RefKind::Path, expr.span);
                }
            }
            ExprKind::MethodCall(_, _, _, _) => {
                if let Some(def_id) = self.typeck.type_dependent_def_id(expr.hir_id) {
                    self.push(def_id, RefKind::Method, expr.span);
                }
            }
            _ => {}
        }
        intravisit::walk_expr(self, expr);
    }
}

impl<'a, 'tcx> RefVisitor<'a, 'tcx> {
    fn push(&mut self, def_id: crate::rustc_hir::def_id::DefId, kind: RefKind, span: Span) {
        self.refs.push(Reference {
            from_def_path: self.from.to_string(),
            to_def_path: self.tcx.def_path_str(def_id),
            kind,
            span: byte_span(self.tcx, span),
        });
    }
}

// Nothing here runs a session. What `extract` reads out of the HIR (signatures, fields,
// variants, impl items) and how it survives a body that stops on an error are not tested in
// this module: exercising them means compiling source, and no_core source that type checks
// has to declare its own lang items, which is a fixture rather than a case anyone has.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_kinds_are_facts_and_use_impl_are_not() {
        assert_eq!(fact_kind(DefKind::Fn), Some(FactKind::Fn));
        assert_eq!(fact_kind(DefKind::AssocFn), Some(FactKind::AssocFn));
        assert_eq!(fact_kind(DefKind::Trait), Some(FactKind::Trait));
        assert_eq!(fact_kind(DefKind::Use), None);
        assert_eq!(fact_kind(DefKind::Impl { of_trait: true }), None);
        assert_eq!(fact_kind(DefKind::TyParam), None);
    }

    // Facts serialized before the error-tolerance fields existed came only from runs with no
    // error, so they have to read back as complete, not as a partial analysis.
    #[test]
    fn facts_without_the_tolerance_fields_read_as_complete() {
        let json = r#"{"crate_name":"c","definitions":[],"imports":[],"impls":[],"references":[]}"#;
        let facts: CrateFacts = serde_json::from_str(json).unwrap();
        assert!(facts.complete);
        assert!(facts.diagnostics.is_empty());
        assert!(facts.unanalyzed_bodies.is_empty());
    }

    // A definition serialized before the shape fields existed still reads, with them empty,
    // and one with no shape writes no shape fields, so the older form round-trips unchanged.
    #[test]
    fn definitions_without_shape_fields_round_trip() {
        let json = r#"{"def_path":"m::f","name":"f","kind":"fn","span":{"file":"lib.rs","start":0,"end":9}}"#;
        let definition: Definition = serde_json::from_str(json).unwrap();
        assert_eq!(definition.signature, None);
        assert!(definition.fields.is_empty());
        assert!(definition.variants.is_empty());
        assert_eq!(serde_json::to_string(&definition).unwrap(), json);
    }

    #[test]
    fn impls_serialized_without_items_read_with_none() {
        let json = r#"{"def_path":"m::<impl S>","self_type":"S","trait_def_path":null,"span":{"file":"lib.rs","start":0,"end":9}}"#;
        let fact: Impl = serde_json::from_str(json).unwrap();
        assert!(fact.items.is_empty());
    }

    #[test]
    fn receiver_kinds_serialize_in_snake_case() {
        let receiver = Receiver { kind: ReceiverKind::RefMut, ty: None };
        assert_eq!(serde_json::to_string(&receiver).unwrap(), r#"{"kind":"ref_mut","ty":null}"#);
    }

    // Emitter output is written by hand here: producing it for real means running a session,
    // which is exactly what these tests stay clear of. The shape is the `short_message` one.
    #[test]
    fn captured_output_splits_into_errors_and_warnings_with_their_locations() {
        let captured = "error[E0425]: cannot find value `x` in this scope\n  --> src/lib.rs:1:14\nwarning: unused variable: `y`\nnote: a note on its own\nFor more information about this error, try `rustc --explain E0425`.\nerror: aborting due to 1 previous error\n";
        let (errors, warnings) = split_diagnostics(captured);
        assert_eq!(
            errors,
            vec![
                "error[E0425]: cannot find value `x` in this scope\n  --> src/lib.rs:1:14".to_string(),
                "error: aborting due to 1 previous error".to_string(),
            ]
        );
        assert_eq!(warnings, vec!["warning: unused variable: `y`".to_string()]);
    }
}
