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
//! run in [`crate::rustc_span::fatal_error::catch_fatal_errors`] so a refused
//! program is `Err`, not a dead daemon. That wrap needs `panic = "unwind"`.
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
use crate::rustc_hir::def_id::LOCAL_CRATE;
use crate::rustc_hir::intravisit::{self, Visitor};
use crate::rustc_hir::{ExprKind, ItemKind, UseKind};
use crate::rustc_interface::{Config, create_and_enter_global_ctxt, parse, run_compiler};
#[cfg(not(feature = "force_pinned_sysroot"))]
use crate::rustc_interface::util::rustc_version_of_sysroot;
use crate::rustc_middle::ty::TyCtxt;
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub def_path: String,
    pub name: String,
    pub kind: FactKind,
    pub span: ByteSpan,
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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
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

/// Analyse one crate from source.
///
/// Fatal rustc errors are `Err(FatalError)`: this is rustc, not an IDE recovery
/// engine, but the error kills the compilation, not the process. Equivalent to
/// [`analyze_source_with_sysroot`] with `sysroot = None`.
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
    let using_internal_features =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicBool::new(false)));
    let config = Config {
        opts,
        input: Input::Str {
            name: FileName::anon_source_code(source),
            input: source.to_string(),
        },
        psess_created: None,
        using_internal_features,
        rustc_version,
    };
    catch_fatal_errors(|| {
        run_compiler(config, |compiler| {
            let krate = parse(&compiler.sess);
            create_and_enter_global_ctxt(compiler, krate, extract)
        })
    })
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
