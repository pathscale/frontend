//! Compiler-true facts extracted from `TyCtxt`.
//!
//! This is the additive module the rust-analyzer handover asked for: agentcode's
//! concepts do not live here, and nothing under `rustc_*` is edited. A fact this
//! module reports is a fact rustc would use. Capabilities in the adapter stay
//! false until those facts are proven there.
//!
//! `run_compiler` is still batch-shaped. Agentcode indexes immutable snapshots,
//! so one analysis per snapshot is the first milestone: extract, then drop the
//! `TyCtxt`. A resident holder that outlives one call is a later increment, not
//! required to emit definitions, references, imports, impls, or trait impls.
//!
//! Within-crate first. Cross-crate facts need a sysroot built from the same
//! upstream commit; this module does not invent one.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them.
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;

use crate::rustc_hir::def::DefKind;
use crate::rustc_hir::def_id::LOCAL_CRATE;
use crate::rustc_hir::intravisit::{self, Visitor};
use crate::rustc_hir::{ExprKind, ItemKind, UseKind};
use crate::rustc_interface::{Config, create_and_enter_global_ctxt, parse, run_compiler};
use crate::rustc_middle::ty::TyCtxt;
use crate::rustc_session::config::{Input, Options};
use crate::rustc_span::{FileName, Span};

/// Byte range inside one source file, relative to that file's start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ByteSpan {
    pub file: String,
    pub start: u32,
    pub end: u32,
}

/// Kind of a named definition. Anonymous compiler items are omitted, not guessed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Definition {
    pub def_path: String,
    pub name: String,
    pub kind: FactKind,
    pub span: ByteSpan,
}

/// One `use` / `pub use`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Import {
    pub module_def_path: String,
    pub path: String,
    pub span: ByteSpan,
    pub reexport: bool,
}

/// One `impl` block, inherent or trait.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Impl {
    pub def_path: String,
    pub self_type: String,
    pub trait_def_path: Option<String>,
    pub span: ByteSpan,
}

/// Kind of a resolved use of a definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefKind {
    Path,
    Method,
}

/// One resolved reference. Locals and primitives are omitted: they are not defs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    pub from_def_path: String,
    pub to_def_path: String,
    pub kind: RefKind,
    pub span: ByteSpan,
}

/// Facts for one crate after analysis.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CrateFacts {
    pub crate_name: String,
    pub definitions: Vec<Definition>,
    pub imports: Vec<Import>,
    pub impls: Vec<Impl>,
    pub references: Vec<Reference>,
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

/// Extract facts from an already-built `TyCtxt`. Runs analysis.
pub fn extract(tcx: TyCtxt<'_>) -> CrateFacts {
    let crate_name = tcx.crate_name(LOCAL_CRATE).to_string();
    let mut facts = CrateFacts { crate_name, ..CrateFacts::default() };

    for local in tcx.iter_local_def_id() {
        let def_id = local.to_def_id();
        let kind = tcx.def_kind(def_id);
        if let DefKind::Impl { .. } = kind {
            let self_type =
                tcx.type_of(def_id).instantiate_identity().skip_normalization().to_string();
            let trait_def_path =
                tcx.impl_opt_trait_id(def_id).map(|trait_id| tcx.def_path_str(trait_id));
            facts.impls.push(Impl {
                def_path: tcx.def_path_str(def_id),
                self_type,
                trait_def_path,
                span: byte_span(tcx, tcx.def_span(def_id)),
            });
            continue;
        }
        let Some(kind) = fact_kind(kind) else {
            continue;
        };
        let Some(name) = tcx.opt_item_name(def_id) else {
            continue;
        };
        facts.definitions.push(Definition {
            def_path: tcx.def_path_str(def_id),
            name: name.to_string(),
            kind,
            span: byte_span(tcx, tcx.def_span(def_id)),
        });
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
        let path_str = match use_kind {
            UseKind::Glob => format!("{path_str}::*"),
            UseKind::Single(_) | UseKind::ListStem => path_str,
        };
        let module = tcx.parent_module_from_def_id(item.owner_id.def_id);
        facts.imports.push(Import {
            module_def_path: tcx.def_path_str(module.to_def_id()),
            path: path_str,
            span: byte_span(tcx, item.span),
            reexport: tcx.local_visibility(item.owner_id.def_id).is_public(),
        });
    }

    for owner in tcx.hir_body_owners() {
        let Some(body) = tcx.hir_maybe_body_owned_by(owner) else {
            continue;
        };
        let typeck = tcx.typeck(owner);
        let from = tcx.def_path_str(owner.to_def_id());
        let mut visitor = RefVisitor { tcx, typeck, from: &from, refs: &mut facts.references };
        visitor.visit_expr(body.value);
    }

    facts.definitions.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.def_path.cmp(&b.def_path)));
    facts.imports.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.path.cmp(&b.path)));
    facts.impls.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.def_path.cmp(&b.def_path)));
    facts.references.sort_by(|a, b| {
        a.span.start.cmp(&b.span.start).then(a.to_def_path.cmp(&b.to_def_path))
    });
    facts
}

/// Analyse one crate from source. Fatal rustc errors abort: this is rustc, not
/// an IDE recovery engine. Matching sysroot required to type-check `core`.
pub fn analyze_source(crate_name: &str, source: &str) -> CrateFacts {
    let mut opts = Options::default();
    opts.crate_name = Some(crate_name.to_string());
    let using_internal_features = alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicBool::new(false)));
    let config = Config {
        opts,
        input: Input::Str {
            name: FileName::anon_source_code(source),
            input: source.to_string(),
        },
        psess_created: None,
        using_internal_features,
        rustc_version: None,
    };
    run_compiler(config, |compiler| {
        let krate = parse(&compiler.sess);
        create_and_enter_global_ctxt(compiler, krate, extract)
    })
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
}
