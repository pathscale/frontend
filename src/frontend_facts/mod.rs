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

pub mod syntax;

mod interpreter;

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them.
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;

use crate::rustc_data_structures::fx::FxHashMap;
use crate::rustc_data_structures::sync::{cost, run_stage, run_stage_weighted};
use crate::rustc_feature::UnstableFeatures;
use crate::rustc_hir::def::DefKind;
use crate::rustc_hir::def_id::{CRATE_DEF_ID, DefId, LOCAL_CRATE, LocalDefId};
use crate::rustc_hir::intravisit::{self, Visitor};
use crate::rustc_hir::{self as hir, ExprKind, ItemKind, Node, UseKind};
#[cfg(not(feature = "force_pinned_sysroot"))]
use crate::rustc_interface::util::rustc_version_of_sysroot;
use crate::rustc_interface::{Config, create_and_enter_global_ctxt, parse, run_compiler};
use crate::rustc_middle::middle::privacy::EffectiveVisibilities;
use crate::rustc_middle::ty::{self, TyCtxt, TypeVisitableExt};
use crate::rustc_session::config::{Input, Options, Sysroot};
use crate::rustc_span::fatal_error::{FatalError, catch_fatal_errors};
use crate::rustc_span::{FileName, SourceFile, Span};
use crate::rustc_structures::CrateType;
use serde::{Deserialize, Serialize};
pub mod site;

#[cfg(feature = "diagnostics")]
pub mod diagnostics;

/// Byte range inside one source file, relative to that file's start.
///
/// `file` is shared: every span in the session's input file holds the one name [`extract`]
/// printed for it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ByteSpan {
    pub file: Arc<str>,
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
    /// Printed once; every other fact that names this definition shares it.
    pub def_path: Arc<str>,
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
    /// For `Enum`: each variant's shape, in the same order as `variants`. A field of its own
    /// rather than a change to `variants`, whose shape callers already deserialise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variant_shapes: Vec<VariantShape>,
    /// Declared plain `pub`. `pub(crate)` and the like are not.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub public: bool,
    /// Reachable from outside the crate, re-exports included: rustc's effective visibility,
    /// which is what a path written in another crate can name.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub exported: bool,
    /// The library feature a use of it needs, when rustc's stability says it is unstable:
    /// what a stable compiler refuses a path or a call to it for (E0658). Read by rustc's own
    /// `lookup_stability`, so an item inherits its unstable parent's mark as rustc says it
    /// does; for an item of a trait impl it is the trait item's, which is what a call resolves
    /// to and what rustc checks. `None` for a stable item and in a crate that marks no
    /// stability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstable: Option<String>,
}

/// One enum variant's shape: how it is constructed, and with how many fields.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VariantShape {
    pub name: String,
    pub kind: VariantKind,
    pub fields: u32,
}

/// How a variant is written.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantKind {
    /// `A`.
    Unit,
    /// `A(x, y)`: called like a function of `fields` arguments.
    Tuple,
    /// `A { x, y }`.
    Struct,
}

/// The names one module makes visible, as the resolver settled them: its own items, its
/// imports and re-exports, and what its globs bring in, each followed to what it names.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModuleNames {
    pub module_def_path: Arc<str>,
    pub names: Vec<ModuleName>,
}

/// One name a module makes visible.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModuleName {
    pub name: String,
    pub namespace: Namespace,
    /// What the name resolves to, printed as a def path.
    pub target_def_path: Arc<str>,
    /// Visible outside the crate, as `pub` makes it.
    pub public: bool,
}

/// The namespace a name lives in: `u8` the type and `u8` a module coexist, as do a function
/// and a macro of one name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Namespace {
    Type,
    Value,
    Macro,
}

/// One declarative macro the crate defines, including one an expansion defined.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacroFacts {
    pub def_path: Arc<str>,
    pub name: String,
    /// `#[macro_export]`: callable from other crates as `crate_name::name!`.
    pub exported: bool,
    /// `#[rustc_builtin_macro]`: its matchers are real and its meaning is the compiler's.
    pub builtin: bool,
    /// The rules as tokens, printed: `(matcher) => { transcriber };` for each rule. A
    /// `macro name(..) { .. }` reads as its one rule.
    pub rules: String,
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
    pub module_def_path: Arc<str>,
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
    pub def_path: Arc<str>,
    pub self_type: String,
    pub trait_def_path: Option<Arc<str>>,
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
    pub def_path: Arc<str>,
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
    /// Shared by every reference out of the same body.
    pub from_def_path: Arc<str>,
    /// Shared by every reference to the same definition out of the same body, and with the
    /// definition's own fact when it is local.
    pub to_def_path: Arc<str>,
    pub kind: RefKind,
    pub span: ByteSpan,
}

/// Facts for one crate after analysis.
///
/// **Facts survive errors.** Definitions, imports and impls come from name resolution and the
/// HIR, which exist for any program that parses and expands; they are reported even when no
/// body type checks. References need a body's type check, so they are collected per body and
/// a body that could not be checked contributes none. `complete` says whether anything was
/// lost, and `diagnostics` says why; in a library read it says whether reading lost input,
/// which a diagnostic alone does not (see `complete`).
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
    pub unanalyzed_bodies: Vec<Arc<str>>,
    /// The facts are whole: reading lost nothing. What that is measured by depends on the read.
    ///
    /// **A strict read** is complete when no error was emitted and every body was type checked.
    /// When false, `references` is a lower bound and the definitions, imports and impls are
    /// unaffected.
    ///
    /// **A library read** (the `library` field of [`CrateRead`]) judges nothing, and most of
    /// what it still emits is one compiler version's bookkeeping about another's source: an ABI,
    /// an attribute, a lang item or a stability mark this build does not know, on an item that
    /// is read all the same. None of that loses a fact, so none of it makes a library read
    /// incomplete; it is all in `diagnostics` still.
    ///
    /// A library read is complete when every body it checked was type checked, the privacy
    /// pass's exports were read, and reading lost none of the crate's input where input turns
    /// into names: no parser run (the crate root, a module's file, a macro's output) emitted an
    /// error, no module file failed to load, no macro invocation was left without its output
    /// (a bang or derive macro that could not be found counts; an unknown attribute does not,
    /// its item is kept), and every import resolved. When true, the modules' names, the
    /// definitions, the imports and the re-exports are all the crate has, so a name missing
    /// from them is absent. A path in a signature or a body that did not resolve names nothing
    /// the crate has, and loses none of its names. Where each loss is recorded is listed at
    /// `Session::record_loss`.
    ///
    /// Serialized facts from before this field existed came only from runs with no error, so
    /// a missing field reads as true. [`Default`] is false: an empty value nobody filled in is
    /// not a finished analysis.
    #[serde(default = "complete_when_absent")]
    pub complete: bool,
    /// Every module's visible names, resolved. Empty from facts taken before this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<ModuleNames>,
    /// Every declarative macro the crate defines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macros: Vec<MacroFacts>,
}

fn complete_when_absent() -> bool {
    true
}

fn true_when_absent() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
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

/// A session's `jobs.frontend` for a stage width of `width`: how many stage items may run at
/// once on nagoya's pool.
///
/// One or none leaves it unset, which is the serial compiler exactly as it was. Two or more set
/// it, which `run_compiler` reads to latch the session's shared-state mode and to size its
/// per-worker registry. Without the `parallel` cargo feature there is no pool, so it stays unset.
fn frontend_jobs(width: usize) -> Option<core::num::NonZero<usize>> {
    if cfg!(feature = "parallel") && width > 1 { core::num::NonZero::new(width) } else { None }
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
///
/// **Three independent walks, each spread over the session's workers when it has more than one.** The
/// definitions, the imports and the bodies are each a list whose items do not read one
/// another's results: every item asks `tcx` its own questions and returns owned data. Each list
/// is one stage (`sync::run_stage`) over its frozen input, read in place, which runs serially
/// when the session is serial and on the pool otherwise, and which returns one output per item,
/// in input order, either way. The lists are then
/// assembled exactly as the serial loops assembled them, so a fact's position, ties in the final
/// sorts included, is the same at any worker count.
///
/// **Data forward, not recomputed.** The definitions stage prints each local definition's path
/// once. Its output is frozen before the imports and bodies stages start, and they, like the
/// impls' trait and item paths, read a local definition's path from it and share the `Arc`
/// rather than printing it again (see [`Names`]).
///
/// What makes the items safe to run side by side is the query system, not anything here: every
/// `tcx` call is a query, a query's result is memoised behind the session's locks, and those
/// locks synchronise because `jobs.frontend` turned the thread-safe mode on for this session.
/// Two workers that want the same result either find it done or wait for the one computing it.
/// A query that unwound is poisoned for both, which is how the serial walk already behaved.
pub fn extract(tcx: TyCtxt<'_>) -> CrateFacts {
    extract_with(tcx, true)
}

/// [`extract`] without the bodies: definitions, imports, impls, module names and macros, and
/// no references. What indexing a crate's API needs: no body is type checked, which is most of
/// the cost, and nothing a body's check could stop on is reached. `references` and
/// `unanalyzed_bodies` stay empty, and `complete` says only that no error was emitted, or in a
/// library read that reading lost nothing (see [`CrateFacts::complete`]).
pub fn extract_items(tcx: TyCtxt<'_>) -> CrateFacts {
    extract_with(tcx, false)
}

fn extract_with(tcx: TyCtxt<'_>, bodies: bool) -> CrateFacts {
    let crate_name = tcx.crate_name(LOCAL_CRATE).to_string();
    let mut facts = CrateFacts { crate_name, ..CrateFacts::default() };

    // Not `tcx.iter_local_def_id()`: that depends on the `analysis` query so that it lists the
    // definitions of a finished compilation, and `analysis` runs well-formedness checking over
    // the whole crate first. In a `no_core` session a real file's first `fn` that returns a
    // value asks for a lang item that is not there, which is fatal, so the facts of every file
    // that uses a library type were lost before one definition was read. The table is read
    // directly instead, counted once: definitions created while the bodies are checked below
    // are the compiler's own and are not facts about the source.
    //
    // Lowering is forced first, and only lowering: it is what feeds each local definition its
    // `def_kind`, which `analysis` used to force as a side effect. Without it the first
    // `def_kind` below is an unprovided query and an internal compiler error.
    let _ = tcx.hir_crate_items(());
    let count = tcx.untracked().def_table.num_definitions();

    // The input file's name, printed once and shared by every span in it.
    let input = input_file(tcx);

    // What is reachable from outside the crate: rustc's own table, the `effective_visibilities`
    // query of `rustc_privacy`. The resolver's table, which it starts from, covers modules,
    // items and imports but no associated item, so a method read from it was never exported;
    // the privacy pass extends it through impls, traits, fields and `impl Trait` returns (the
    // last type-checks those functions' bodies, as a full compile does). When it stops on a
    // fatal error (a lang item a `no_core` session lacks), the resolver's table is what there
    // is, and associated items read as not exported.
    let exported = catch_fatal_errors(|| tcx.effective_visibilities(()));
    // Whether the privacy pass's table is the one read: a library read that fell back to the
    // resolver's has lost the associated items' exports.
    let exports_whole = exported.is_ok();
    let exported = exported.unwrap_or(&tcx.resolutions(()).effective_visibilities);

    // **Definitions and impls, one item per local definition.** Each index is read on its own:
    // its kind, its name, its span and its HIR shape, or for an impl its self type, trait and
    // items. Nothing one index computes is read by another, so the walk is a stage whose input
    // is the index itself, and it hands back one `DefFact` per index in index order. Pushing
    // them in that order below is the serial loop's order exactly, impls and definitions each
    // keeping their own sequence.
    //
    // No definition's path is printed yet when this stage runs, so each item prints its own.
    let printing = Names { tcx, exported, local: &[], input: input.clone() };
    let def_facts: Vec<DefFact> = run_stage((), count, |_, i| {
        let local_def_index = crate::rustc_span::def_id::DefIndex::from_usize(i);
        def_fact(tcx, &printing, LocalDefId { local_def_index })
    });

    // From here on a local definition's path is the one the stage above printed, handed
    // forward: `def_facts` is frozen, indexed by `DefIndex`, and read in place.
    let names = Names { tcx, exported, local: &def_facts, input };

    // An impl's trait and items are local definitions the stage above may have reached after
    // the impl, so their paths are taken now, in impl order.
    let impl_paths: Vec<ImplPaths> = def_facts
        .iter()
        .filter_map(|fact| match fact {
            DefFact::Impl(pending) => Some(pending.paths(&names)),
            DefFact::Definition(_) | DefFact::Skipped => None,
        })
        .collect();

    // **Imports, one item per free item**, read in place from the crate's frozen id list. Each
    // `use` reads only its own item, its parent module and its visibility; `None` is anything
    // that is not a binding `use`, dropped here exactly where the serial loop's `continue`
    // dropped it.
    let free_items = tcx.hir_crate_items(()).free_item_ids();
    let imports: Vec<Option<Import>> =
        run_stage(free_items, free_items.len(), |ids, i| import_fact(tcx, &names, ids[i]));
    facts.imports.extend(imports.into_iter().flatten());

    // **Bodies, one item per body owner: the expensive walk.** Each owner's type check runs in
    // its own `catch_fatal_errors` on whichever worker takes it, and its references are
    // collected into a list of its own rather than pushed into a shared one. A body that
    // stopped on a fatal error comes back as its printed path instead. The results come back in
    // owner order and are appended in that order, so `references` before the sort and
    // `unanalyzed_bodies`, which is never sorted, are the serial walk's sequences exactly.
    let owners: &[LocalDefId] = if bodies { tcx.hir_body_owner_ids() } else { &[] };
    //
    // In rustc's order, in two stages. The first takes every body rustc's own body loop
    // (`rustc_hir_analysis::check_crate`) type-checks. That loop skips an anon const of the type
    // system (an array length, a const generic argument): such a const's type is fed while the
    // signature or body around it is lowered, and rustc lowers every signature before it checks
    // any body. This walk does not run that crate-wide lowering (it would stop a `no_core` file
    // on its first missing lang item), so checking those consts in index order reached one before
    // its type was fed: core, which has no other error to stop at first, ended in delayed bugs.
    // The second stage takes them after every other body, each after its owner's signature is
    // lowered (`lower_signature`), which is the state rustc checks them in.
    //
    // A body weighs its source at type checking's rate (`sync::cost::TYPECK`), which is what
    // most of its fact costs, so a small file's bodies run serially, as width one runs them.
    // The two stages above stay unweighted, cut by count: their items cost a path printed per
    // definition, which is not a cost per byte, and nothing has measured it.
    let (bodies_first, consts): (Vec<usize>, Vec<usize>) =
        (0..owners.len()).partition(|&i| !is_type_system_anon_const(tcx, owners[i]));
    let first: Vec<Option<BodyFact>> = run_stage_weighted(
        &bodies_first[..],
        bodies_first.len(),
        |order, i| tcx.stage_weight(owners[order[i]], cost::TYPECK),
        |order, i| body_fact(tcx, &names, owners[order[i]]),
    );
    let second: Vec<Option<BodyFact>> = run_stage_weighted(
        &consts[..],
        consts.len(),
        |order, i| tcx.stage_weight(owners[order[i]], cost::TYPECK),
        |order, i| {
            let owner = owners[order[i]];
            lower_signature(tcx, tcx.hir_get_parent_item(tcx.local_def_id_to_hir_id(owner)).def_id);
            body_fact(tcx, &names, owner)
        },
    );
    // Back in owner order, which is the order the facts were always appended in.
    let mut bodies: Vec<Option<BodyFact>> = (0..owners.len()).map(|_| None).collect();
    for (i, fact) in bodies_first.into_iter().zip(first).chain(consts.into_iter().zip(second)) {
        bodies[i] = fact;
    }
    for body in bodies.into_iter().flatten() {
        match body {
            BodyFact::Checked(references) => facts.references.extend(references),
            BodyFact::Unanalyzed(from) => facts.unanalyzed_bodies.push(from),
        }
    }

    // **Module names and macro definitions**, after the stages above so each target's path is
    // the one they printed. Serial: one pass over the definition table, and what each module
    // makes visible is the resolver's own answer, already computed, only read here.
    for i in 0..count {
        let local = LocalDefId { local_def_index: crate::rustc_span::def_id::DefIndex::from_usize(i) };
        match tcx.def_kind(local) {
            DefKind::Mod => facts.modules.push(module_names(tcx, &names, local)),
            DefKind::Macro(_) => facts.macros.extend(macro_facts(tcx, &names, local)),
            _ => {}
        }
    }

    // Every later stage has read `def_facts`; now they move into the facts, in index order.
    drop(names);
    let mut impl_paths = impl_paths.into_iter();
    for fact in def_facts {
        match fact {
            DefFact::Impl(pending) => {
                let paths = impl_paths.next().expect("one `ImplPaths` per impl, in impl order");
                facts.impls.push(pending.finish(paths));
            }
            DefFact::Definition(definition) => facts.definitions.push(definition),
            DefFact::Skipped => {}
        }
    }
    // What `complete` measures depends on the read; see its doc. A strict read is whole when
    // nothing was emitted. A library read emits what it no longer judges and loses nothing by it,
    // so it counts what reading lost where that happened (`Session::record_loss`): input a parser
    // could not read, a macro invocation left without its output, an import that did not
    // resolve. Every library read's loss comes before this point: parsing, expansion and name
    // resolution are over once the HIR the stages above read exists.
    let whole = if tcx.sess.is_library_read() {
        tcx.sess.losses() == 0 && exports_whole
    } else {
        tcx.dcx().has_errors().is_none()
    };
    facts.complete = whole && facts.unanalyzed_bodies.is_empty();

    facts
        .definitions
        .sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.def_path.cmp(&b.def_path)));
    facts.imports.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.path.cmp(&b.path)));
    facts.impls.sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.def_path.cmp(&b.def_path)));
    facts
        .references
        .sort_by(|a, b| a.span.start.cmp(&b.span.start).then(a.to_def_path.cmp(&b.to_def_path)));
    facts
}

/// The session's input file, the one the crate root's span is in, and its name.
///
/// Only the sharing depends on this being the right file: [`Names::span`] compares a span's
/// file to it by pointer and prints the name of any other file itself.
fn input_file(tcx: TyCtxt<'_>) -> (Arc<SourceFile>, Arc<str>) {
    let root = tcx.def_span(CRATE_DEF_ID.to_def_id());
    let sf = tcx.sess.source_map().lookup_byte_offset(root.lo()).sf;
    let name = file_name(&sf);
    (sf, name)
}

/// A source file's name as a [`ByteSpan`] reports it.
fn file_name(sf: &SourceFile) -> Arc<str> {
    Arc::from(sf.name.prefer_local_unconditionally().to_string())
}

/// Where [`extract`] gets the def paths and file names its facts hold, as shared values.
///
/// No cache. `local` is the definitions stage's output, frozen before any later stage starts
/// and read in place, so a local definition's path is printed once, by the stage item that
/// reports the definition, and every later fact that names it holds the same `Arc`. Anything
/// else (a library item, a closure, a constructor) is printed where it is asked for.
struct Names<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    /// Which local definitions are reachable from outside the crate (`Definition::exported`).
    exported: &'a EffectiveVisibilities,
    /// One entry per local `DefIndex`, from the definitions stage. Empty while that stage runs.
    local: &'a [DefFact],
    /// The input file, compared by pointer, and its name.
    input: (Arc<SourceFile>, Arc<str>),
}

impl<'a, 'tcx> Names<'a, 'tcx> {
    /// `def_id`'s printed path: the definitions stage's, when it reported `def_id`, and printed
    /// here otherwise. Both are `def_path_str` of the same id.
    ///
    /// A definition in another crate is printed where it is defined, under that crate's own name
    /// (`alloc::fmt`), not by the path this crate happens to see it through: std names alloc
    /// `alloc_crate`, and a re-export chain is followed by joining one crate's facts to the next's,
    /// whose own paths are where each item is defined.
    fn path(&self, def_id: DefId) -> Arc<str> {
        self.reported(def_id).unwrap_or_else(|| {
            let printed = if def_id.is_local() {
                self.tcx.def_path_str(def_id)
            } else {
                crate::rustc_middle::ty::print::with_no_visible_paths!(
                    self.tcx.def_path_str(def_id)
                )
            };
            Arc::from(printed)
        })
    }

    fn reported(&self, def_id: DefId) -> Option<Arc<str>> {
        let local = def_id.as_local()?;
        match self.local.get(local.local_def_index.as_usize())? {
            DefFact::Definition(definition) => Some(Arc::clone(&definition.def_path)),
            DefFact::Impl(pending) => Some(Arc::clone(&pending.fact.def_path)),
            DefFact::Skipped => None,
        }
    }

    /// `span` as a byte range in its file. A span in the input file shares its one name.
    fn span(&self, span: Span) -> ByteSpan {
        let sm = self.tcx.sess.source_map();
        let (lo_sf, lo) = sm.lookup_byte_offset_in(span.lo());
        let (_, hi) = sm.lookup_byte_offset_in(span.hi());
        let (input, name) = &self.input;
        let file =
            if core::ptr::eq(Arc::as_ptr(input), lo_sf) { Arc::clone(name) } else { file_name(lo_sf) };
        ByteSpan { file, start: lo.0, end: hi.0 }
    }
}

/// The names a module makes visible, from the resolver: its items, its imports and re-exports,
/// and what its globs bring in, each with what it resolves to. A name that resolves to no
/// definition (a primitive type, a built-in attribute) is left out.
fn module_names<'tcx>(tcx: TyCtxt<'tcx>, names: &Names<'_, 'tcx>, local: LocalDefId) -> ModuleNames {
    use crate::rustc_hir::def::{Namespace as Ns, Res};
    let visible = tcx
        .module_children_local(local)
        .iter()
        .filter_map(|child| {
            let Res::Def(kind, def_id) = child.res else {
                return None;
            };
            let namespace = match kind.ns()? {
                Ns::TypeNS => Namespace::Type,
                Ns::ValueNS => Namespace::Value,
                Ns::MacroNS => Namespace::Macro,
            };
            Some(ModuleName {
                name: child.ident.as_str().to_string(),
                namespace,
                target_def_path: names.path(def_id),
                public: child.vis.is_public(),
            })
        })
        .collect();
    ModuleNames { module_def_path: names.path(local.to_def_id()), names: visible }
}

/// A declarative macro's definition, its rules printed from its tokens. `None` for a macro
/// that is not an item (one a proc macro crate declares).
fn macro_facts<'tcx>(tcx: TyCtxt<'tcx>, names: &Names<'_, 'tcx>, local: LocalDefId) -> Option<MacroFacts> {
    let Node::Item(hir::Item { kind: ItemKind::Macro(ident, def, _), .. }) = tcx.hir_node_by_def_id(local)
    else {
        return None;
    };
    Some(MacroFacts {
        def_path: names.path(local.to_def_id()),
        name: ident.as_str().to_string(),
        exported: crate::find_attr!(tcx, local, MacroExport { .. }),
        builtin: crate::find_attr!(tcx, local, RustcBuiltinMacro { .. }),
        rules: crate::rustc_ast_pretty::pprust::tts_to_string(&def.body.tokens),
    })
}

/// What one local definition contributes to [`CrateFacts`]: an impl, a named definition, or
/// nothing (a `use`, an anonymous item, a definition with no name).
enum DefFact {
    Impl(PendingImpl),
    Definition(Definition),
    Skipped,
}

/// An impl as the definitions stage leaves it: everything but the paths of its local trait and
/// its items, which name other local definitions and are taken from their facts afterwards.
struct PendingImpl {
    /// `items` is empty, and `trait_def_path` is filled only for a trait that is not local.
    fact: Impl,
    /// The implemented trait, when it is local.
    local_trait: Option<LocalDefId>,
    /// Each item's name, kind and id, in source order.
    items: Vec<(String, FactKind, LocalDefId)>,
}

/// The paths a [`PendingImpl`] was waiting for, in the same order.
struct ImplPaths {
    local_trait: Option<Arc<str>>,
    items: Vec<Arc<str>>,
}

impl PendingImpl {
    fn paths(&self, names: &Names<'_, '_>) -> ImplPaths {
        ImplPaths {
            local_trait: self.local_trait.map(|id| names.path(id.to_def_id())),
            items: self.items.iter().map(|(_, _, id)| names.path(id.to_def_id())).collect(),
        }
    }

    fn finish(self, paths: ImplPaths) -> Impl {
        let mut fact = self.fact;
        if paths.local_trait.is_some() {
            fact.trait_def_path = paths.local_trait;
        }
        fact.items = self
            .items
            .into_iter()
            .zip(paths.items)
            .map(|((name, kind, _), def_path)| ImplItem { name, kind, def_path })
            .collect();
        fact
    }
}

/// One local definition's fact. The body of the serial loop that used to sit in [`extract`],
/// moved out unchanged so each index can run on its own worker.
fn def_fact<'tcx>(tcx: TyCtxt<'tcx>, names: &Names<'_, 'tcx>, local: LocalDefId) -> DefFact {
    let def_id = local.to_def_id();
    let kind = tcx.def_kind(def_id);
    if let DefKind::Impl { .. } = kind {
        return DefFact::Impl(impl_fact(tcx, names, local));
    }
    let Some(kind) = fact_kind(kind) else {
        return DefFact::Skipped;
    };
    let Some(name) = tcx.opt_item_name(def_id) else {
        return DefFact::Skipped;
    };
    let mut definition = Definition {
        def_path: names.path(def_id),
        name: name.to_string(),
        kind,
        span: names.span(tcx.def_span(def_id)),
        signature: None,
        fields: Vec::new(),
        variants: Vec::new(),
        variant_shapes: Vec::new(),
        public: tcx.local_visibility(local).is_public(),
        exported: names.exported.is_exported(local),
        unstable: unstable_feature(tcx, def_id),
    };
    describe_shape(tcx, local, &mut definition);
    DefFact::Definition(definition)
}

/// The feature rustc's stability check asks of a use of `def_id`, when it is unstable. An item
/// of a trait impl is checked as the trait item it implements (a method call resolves to it,
/// and an impl's own mark is not what rustc enforces), which may be another crate's, read from
/// its metadata.
fn unstable_feature(tcx: TyCtxt<'_>, def_id: DefId) -> Option<String> {
    catch_fatal_errors(|| {
        let checked = tcx.trait_item_of(def_id).unwrap_or(def_id);
        tcx.lookup_stability(checked)
            .filter(|stability| stability.is_unstable())
            .map(|stability| stability.feature.to_string())
    })
    .ok()
    .flatten()
}

/// One free item's import, or `None` when it is not a `use` that binds anything. The body of
/// the serial loop that used to sit in [`extract`], with each `continue` now a `None`.
fn import_fact<'tcx>(
    tcx: TyCtxt<'tcx>,
    names: &Names<'_, 'tcx>,
    item_id: hir::ItemId,
) -> Option<Import> {
    let item = tcx.hir_item(item_id);
    let ItemKind::Use(path, use_kind) = item.kind else {
        return None;
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
        UseKind::ListStem => return None,
    };
    let module = tcx.parent_module_from_def_id(item.owner_id.def_id);
    Some(Import {
        module_def_path: names.path(module.to_def_id()),
        path: path_str,
        span: names.span(item.span),
        reexport: tcx.local_visibility(item.owner_id.def_id).is_public(),
        bindings,
    })
}

/// What one body contributes: the references its type check resolved, or, when the type check
/// stopped on a fatal error, the body's own path for [`CrateFacts::unanalyzed_bodies`].
enum BodyFact {
    Checked(Vec<Reference>),
    Unanalyzed(Arc<str>),
}

/// One body owner's fact, or `None` for an owner with no body. The body of the serial loop that
/// used to sit in [`extract`], collecting into a list of its own instead of a shared one.
///
/// The walk collects each reference as the `DefId` it resolved to. Only then are paths
/// printed: each distinct callee once for this body, its `Arc` shared by every reference to it
/// here, and the map that dedups them ends with the body.
/// An anon const of the type system: one whose type rustc feeds while lowering what contains
/// it, and which rustc's body loop therefore does not type-check as a body of its own.
fn is_type_system_anon_const(tcx: TyCtxt<'_>, owner: LocalDefId) -> bool {
    tcx.def_kind(owner) == DefKind::AnonConst
        && tcx.anon_const_kind(owner.to_def_id()) != ty::AnonConstKind::NonTypeSystemInline
}

/// Lower `item`'s signature, as rustc's collection does for every item before any body is type
/// checked: its generics and where clauses, and its type, function signature or impl header. That
/// is what feeds the type of an anon const written in them. Each query is one rustc runs for
/// the item anyway; one that stops on a fatal error stops only this lowering.
fn lower_signature(tcx: TyCtxt<'_>, item: LocalDefId) {
    let def_id = item.to_def_id();
    let _ = catch_fatal_errors(|| {
        let kind = tcx.def_kind(item);
        if kind.has_generics() {
            tcx.ensure_ok().generics_of(def_id);
            tcx.ensure_ok().explicit_clauses_of(def_id);
        }
        match kind {
            DefKind::Fn | DefKind::AssocFn => tcx.ensure_ok().fn_sig(def_id),
            DefKind::Struct | DefKind::Union | DefKind::Enum => {
                tcx.ensure_ok().type_of(def_id);
                for field in tcx.adt_def(def_id).all_fields() {
                    tcx.ensure_ok().type_of(field.did);
                }
            }
            DefKind::TyAlias
            | DefKind::Const { .. }
            | DefKind::Static { .. }
            | DefKind::AssocConst { .. } => tcx.ensure_ok().type_of(def_id),
            DefKind::Impl { of_trait } => {
                tcx.ensure_ok().type_of(def_id);
                if of_trait {
                    tcx.ensure_ok().impl_trait_header(def_id);
                }
            }
            _ => {}
        }
    });
}

fn body_fact<'tcx>(
    tcx: TyCtxt<'tcx>,
    names: &Names<'_, 'tcx>,
    owner: LocalDefId,
) -> Option<BodyFact> {
    let body = tcx.hir_maybe_body_owned_by(owner)?;
    let from = names.path(owner.to_def_id());
    // Results tainted by an error are still read: a path that resolved is a fact whether or not
    // some other expression in the body failed, and one that did not resolve is `Res::Err`,
    // which `RefVisitor` already skips.
    let Ok(typeck) = catch_fatal_errors(|| tcx.typeck(owner)) else {
        return Some(BodyFact::Unanalyzed(from));
    };
    let mut found = Vec::new();
    let mut visitor = RefVisitor { typeck, found: &mut found };
    visitor.visit_expr(body.value);
    let mut callees: FxHashMap<DefId, Arc<str>> = FxHashMap::default();
    let refs = found
        .into_iter()
        .map(|(def_id, kind, span)| Reference {
            from_def_path: Arc::clone(&from),
            to_def_path: Arc::clone(callees.entry(def_id).or_insert_with(|| names.path(def_id))),
            kind,
            span: names.span(span),
        })
        .collect();
    Some(BodyFact::Checked(refs))
}

/// One `impl` block's fact, with its local trait and its items left for [`PendingImpl::finish`].
///
/// `type_of` is a query that can stop on a fatal error of its own, and an impl whose self type
/// did not resolve comes back as an error type that prints as nothing useful. Either way the
/// type is reported as written in the source, so an impl is never dropped for its self type.
fn impl_fact<'tcx>(tcx: TyCtxt<'tcx>, names: &Names<'_, 'tcx>, local: LocalDefId) -> PendingImpl {
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
    let trait_id = catch_fatal_errors(|| tcx.impl_opt_trait_id(def_id)).ok().flatten();
    let local_trait = trait_id.and_then(DefId::as_local);
    // A trait from another crate has no fact here to share, so it is printed now.
    let trait_def_path = match (trait_id, local_trait) {
        (Some(trait_id), None) => Some(names.path(trait_id)),
        _ => None,
    };
    let items = hir_impl
        .map(|hir_impl| {
            hir_impl
                .items
                .iter()
                .filter_map(|item| {
                    let item_id = item.owner_id.to_def_id();
                    Some((
                        tcx.opt_item_name(item_id)?.to_string(),
                        fact_kind(tcx.def_kind(item_id))?,
                        item.owner_id.def_id,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    PendingImpl {
        fact: Impl {
            def_path: names.path(def_id),
            self_type,
            trait_def_path,
            span: names.span(tcx.def_span(def_id)),
            items: Vec::new(),
        },
        local_trait,
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
                definition.variant_shapes = def
                    .variants
                    .iter()
                    .map(|variant| {
                        let (kind, fields) = match &variant.data {
                            hir::VariantData::Struct { fields, .. } => (VariantKind::Struct, fields.len()),
                            hir::VariantData::Tuple(fields, ..) => (VariantKind::Tuple, fields.len()),
                            hir::VariantData::Unit(..) => (VariantKind::Unit, 0),
                        };
                        VariantShape {
                            name: variant.ident.as_str().to_string(),
                            kind,
                            fields: u32::try_from(fields).unwrap_or(u32::MAX),
                        }
                    })
                    .collect();
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

/// The flag every session here is handed as `Config::using_internal_features`.
///
/// `Config` wants a `&'static AtomicBool`, and each call used to `Box::leak` a fresh one: one
/// allocation per call that was never freed, which a daemon or a corpus pass turns into
/// unbounded growth. One static serves every call on every thread instead.
///
/// Sharing it is sound because nothing reads it. Upstream it is set when a crate enables an
/// `internal` feature and read by `rustc_driver`'s ICE hook, to drop "please report a bug" from
/// an ICE that internal features probably caused. This tree has no such hook: the only access is
/// the `store(true)` in `rustc_expand::config`, so the flag is write-only and a value one call
/// leaves behind cannot change what another call reports. If an ICE message ever starts reading
/// it, it has to become per session again, owned by the call rather than leaked by it.
static USING_INTERNAL_FEATURES: AtomicBool = AtomicBool::new(false);

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
    analyze_source_with_width(crate_name, source, sysroot, 1)
}

/// [`analyze_source_with_sysroot`], with up to `width` of this analysis's independent stage
/// items (per-definition facts, per-body type and borrow checks, lowering) running at once on
/// nagoya's pool. `1` (or `0`) is the serial compiler.
///
/// The answer does not depend on `width`; `tests/parallel.rs` holds it to that. Without the
/// `parallel` cargo feature every stage runs serially whatever is asked.
pub fn analyze_source_with_width(
    crate_name: &str,
    source: &str,
    sysroot: Option<&str>,
    width: usize,
) -> Result<CrateFacts, FatalError> {
    analyze_shared_source_with_width(crate_name, Arc::new(String::from(source)), sysroot, width)
}

/// [`analyze_source_with_width`] over text the caller already shares. The session's source
/// file keeps `source` itself, so the text is never copied (unless it has a BOM or a `\r\n`,
/// which the source map normalizes away in its own copy).
pub fn analyze_shared_source_with_width(
    crate_name: &str,
    source: Arc<String>,
    sysroot: Option<&str>,
    width: usize,
) -> Result<CrateFacts, FatalError> {
    let input = Input::Str { name: FileName::anon_source_code(&source), input: source };
    let setup = Setup { sysroot, width, ..Setup::plain(crate_name) };
    analyze_input(&setup, input, false, None).map_err(|_| FatalError)
}

/// Why [`analyze_crate`] has no facts: the crate did not parse or its macros did not expand,
/// so there is no HIR to read. Every error the frontend emitted, as [`CrateFacts::diagnostics`]
/// holds them, so a refusal says what refused.
///
/// A crate read to be a dependency ([`CrateRead::write_metadata`]) is also refused for any error
/// at all: a crate that does not compile is no crate's dependency, and its metadata is not
/// written. A library read ([`CrateRead`]'s `library`) is not: what it records refuses nothing,
/// and it is refused only when it has no HIR or its metadata could not be written. `crate_name`
/// says which crate of a chain it was.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Refused {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub crate_name: String,
    pub diagnostics: Vec<String>,
}

/// A crate a session depends on, read from source by frontend earlier: the name the dependent
/// crate knows it by, and the metadata file that earlier read wrote
/// ([`CrateRead::write_metadata`]).
///
/// **The handoff between crates is rustc's own crate metadata**, written by this build from
/// source and read back by this build, never a library from a toolchain. It is what rustc's
/// `--extern name=path` hands a session, and it lives on disk, so a caller that keeps it (per
/// crate and version) reads a crate's dependencies once and each later read of anything that
/// depends on them costs only the loading of their files.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    /// The `.rmeta` file. Its file name has to be `lib<name>.rmeta`, as rustc's loader expects.
    pub metadata: String,
    /// In the extern prelude, so the crate being read can name it: a dependency its manifest
    /// lists. `false` is rustc's `--extern noprelude:`, for a dependency's dependency, which is
    /// loaded when a loaded crate needs it and which the crate being read cannot name.
    ///
    /// **Two crates of one name** (the registry's `libc` beside the one std was built with) may
    /// both be loaded, when each was read with its own [`CrateRead::disambiguator`]. A lookup by
    /// name for the crate being read (`libc::`, `extern crate libc`, an injected `std`) chooses
    /// only among the prelude files of that name when there is one, and among the `noprelude`
    /// ones only when there is none, as rustc's `extern crate` of a `noprelude` crate does. A
    /// loaded crate's own dependency is found among every file of the name, by the hash its
    /// metadata recorded, so the build it was read against is the one it gets. Two prelude
    /// files of one name are ambiguous and refused, as rustc refuses two `--extern` files.
    #[serde(default = "true_when_absent", skip_serializing_if = "is_true")]
    pub prelude: bool,
    /// For a proc-macro crate ([`CrateRead::proc_macro`]): the shared object a compiler built
    /// from the same source for the host, as cargo leaves it
    /// (`target/<profile>/deps/lib<name>-<hash>.dylib`, `.so` on Linux). With it the crate's
    /// macros run when the crate being read uses them; without it they are declared and every
    /// expansion is an error that says so.
    ///
    /// **This runs the dylib's code in this process**, as rustc does with every proc macro it
    /// expands. What is checked before anything runs, from the file itself: the compiler that
    /// wrote it is one whose proc-macro bridge frontend's server speaks (stable 1.97), it
    /// exports one proc-macro table, and that table has every macro the crate's source declares,
    /// under the same kind and name. A dylib that fails any of the three makes the crate fail to
    /// load, with a message naming the file and the reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_macro_dylib: Option<String>,
}

impl Dependency {
    /// A dependency the crate being read names.
    pub fn new(name: impl Into<String>, metadata: impl Into<String>) -> Self {
        Dependency {
            name: name.into(),
            metadata: metadata.into(),
            prelude: true,
            proc_macro_dylib: None,
        }
    }

    /// A dependency only other dependencies name.
    pub fn transitive(name: impl Into<String>, metadata: impl Into<String>) -> Self {
        Dependency { prelude: false, ..Dependency::new(name, metadata) }
    }

    /// This proc-macro dependency, with its macros run from `dylib`
    /// ([`Dependency::proc_macro_dylib`]).
    pub fn with_proc_macro_dylib(self, dylib: impl Into<String>) -> Self {
        Dependency { proc_macro_dylib: Some(dylib.into()), ..self }
    }
}

/// The macros a proc-macro dylib exports, as `(kind, name)` with kind `derive`, `attr` or
/// `bang`, read with exactly the checks a [`Dependency::proc_macro_dylib`] gets before its
/// macros run: which compiler wrote it, and its one proc-macro table. For a caller to confirm a
/// dylib before handing it over, and to say which file it was when one is refused.
///
/// This opens the dylib, which runs its initializers in this process.
pub fn proc_macro_dylib_macros(dylib: &str) -> Result<Vec<(&'static str, String)>, String> {
    crate::rustc_metadata::dylib::proc_macro_dylib_macros(eko::path::Path::new(dylib))
}

/// What a session reads besides its source: the crates it depends on and the configuration its
/// build would give it. [`Loaded::default`] is none of either, the `no_core` session every
/// entry point without dependencies has always run.
#[derive(Clone, Copy, Debug, Default)]
pub struct Loaded<'a> {
    /// Every crate the session may load, direct and transitive. When there is at least one,
    /// nothing is injected: a crate that says `#![no_std]` gets `extern crate core` and core's
    /// prelude, and one that does not gets std's, exactly as rustc reads it.
    pub dependencies: &'a [Dependency],
    /// `--cfg` specs: the features its manifest enables (`feature="std"`) and what its build
    /// script prints as `cargo:rustc-cfg`. The caller decides these; frontend reads no manifest.
    pub cfg: &'a [String],
    /// What `env!` and `option_env!` read: `CARGO_PKG_VERSION` and a build script's
    /// `cargo:rustc-env`. rustc's `--env-set`: the process environment is not consulted for them.
    pub env: &'a [(String, String)],
}

/// One crate on disk to read, and how. [`CrateRead::new`] is [`analyze_crate`]'s defaults:
/// host target, edition 2015, bodies checked, serial, no dependencies (`no_core`).
#[derive(Clone, Copy, Debug)]
pub struct CrateRead<'a> {
    pub crate_name: &'a str,
    /// The crate's root file, `src/lib.rs` or wherever its manifest says.
    pub root: &'a eko::path::Path,
    pub sysroot: Option<&'a str>,
    pub target: Option<&'a str>,
    pub edition: Option<&'a str>,
    /// [`extract_items`]: no body is type checked and no reference is reported.
    pub items_only: bool,
    pub width: usize,
    /// A `proc-macro` crate. Its macros are declared to the crates that load its metadata, and
    /// they are not run: running one means compiling it, and frontend compiles nothing. An
    /// expansion of one is an error that says so, unless the crate that loads it is handed the
    /// dylib a compiler built from this source ([`Dependency::proc_macro_dylib`]).
    pub proc_macro: bool,
    /// One of the standard library's crates or a crate it depends on (hashbrown, libc), read as
    /// rustc's bootstrap reads them: `-Zforce-unstable-if-unmarked`, so an item not marked stable
    /// is unstable. It is what lets std's stable `const fn`s call hashbrown's.
    pub standard_library: bool,
    pub loaded: Loaded<'a>,
    /// Write the crate's metadata here, for later reads to name it as a [`Dependency`]. Written
    /// only when the crate read with no error; otherwise the read is [`Refused`]. A
    /// `library` read writes it whatever was recorded.
    pub write_metadata: Option<&'a eko::path::Path>,
    /// A library read: the crate is a library its own compiler already compiled, of any version
    /// (a toolchain's `rust-src`, a registry crate), and frontend reads it for its facts and its
    /// metadata without judging it. `false`, the default, is the strict compiler.
    ///
    /// A library read runs what extracting facts and writing metadata need (parsing, expansion,
    /// name resolution, lowering, signatures, impls, predicates, and the bodies the metadata
    /// carries) and no pass whose only job is to reject a program: feature gates, stability,
    /// coherence and overlap, well-formedness, const checking, lints, and every body's type and
    /// borrow check that the metadata does not need (`Session::is_library_read` lists them).
    ///
    /// Whatever is still emitted is recorded in [`CrateFacts::diagnostics`], and none of it makes
    /// the read [`Refused`]: that is kept for a crate with no HIR to read (one that does not
    /// parse), and for metadata that was asked for and could not be written. An internal
    /// compiler error is not caught: it panics out of the read, as it does in any other.
    ///
    /// Nor does what is emitted make the facts incomplete. [`CrateFacts::complete`] says, in a
    /// library read, whether reading lost any of the crate's input where input turns into names:
    /// a parser run that emitted an error (the root, a module's file, a macro's output), a
    /// module file that could not be loaded, a macro invocation left without its output, an
    /// import that did not resolve; and a body whose type check stopped, or exports read from
    /// the resolver's table because the privacy pass stopped. A diagnostic from a check that
    /// only judges (an attribute's or an ABI's validation, a stability mark, a lang item, a type
    /// error, an overlap) loses nothing, so a library read of a toolchain's `core` that records
    /// thousands of those is complete when nothing was lost. Each loss is recorded where it
    /// happens, by the mechanism that gave up, never by matching what was emitted
    /// (`Session::record_loss` lists them).
    pub library: bool,
    /// What tells this crate apart from another of the same name: cargo's `-C metadata=<hash>`,
    /// hashed into the crate's `StableCrateId` with its name exactly as rustc hashes it. `None`
    /// is no `-C metadata`, the id every read had before this existed.
    ///
    /// Two crates of one name loaded in one read (the registry's `libc` and the one std was
    /// built with; `cfg-if` 1.0.4 and 1.0.5) need two ids, and a crate read beside a loaded
    /// crate of its own name needs one of its own too, or rustc's loader refuses the pair
    /// ("colliding StableCrateId values" between two loaded crates, E0519 when one of them is
    /// the crate being read). So a caller that may load two builds of one name gives every
    /// crate it reads a value that differs whenever the build does: a digest of the crate, its
    /// version, its features and what it loads, as cargo's is. The metadata this read writes
    /// carries the id (and a crate hash that covers it), so every later read that loads the
    /// file gets the same id without being told it.
    pub disambiguator: Option<&'a str>,
    /// Write every function's MIR into the metadata, not only what other crates' compiles need
    /// (generic and inline functions). [`evaluate`] steps into any library function a call
    /// reaches, and it can only run a function whose MIR the metadata carries; rustc's own
    /// switch for this is `-Zalways-encode-mir`, which is how Miri's standard library is built.
    ///
    /// It costs the read an optimized MIR body for every function, so it is off by default and a
    /// caller that keeps metadata keeps the two kinds apart. Only [`evaluate`] needs it.
    pub all_mir: bool,
}

impl<'a> CrateRead<'a> {
    pub fn new(crate_name: &'a str, root: &'a eko::path::Path) -> Self {
        CrateRead {
            crate_name,
            root,
            sysroot: None,
            target: None,
            edition: None,
            items_only: false,
            width: 1,
            proc_macro: false,
            standard_library: false,
            loaded: Loaded::default(),
            write_metadata: None,
            library: false,
            disambiguator: None,
            all_mir: false,
        }
    }

    /// This read, as a library read or not: sets the `library` field.
    pub fn library(self, library: bool) -> Self {
        CrateRead { library, ..self }
    }

    /// This read, writing every function's MIR or not: sets the `all_mir` field.
    pub fn with_all_mir(self, all_mir: bool) -> Self {
        CrateRead { all_mir, ..self }
    }

    /// The same read under `disambiguator` ([`CrateRead::disambiguator`]).
    pub fn with_disambiguator(self, disambiguator: &'a str) -> Self {
        CrateRead { disambiguator: Some(disambiguator), ..self }
    }
}

/// Analyse a crate on disk from its root file, as rustc reads one: `mod x;` is `x.rs` or
/// `x/mod.rs` beside the file that declares it, `#[path]` and `include!` are followed, and
/// every span says which file it is in. Otherwise exactly [`analyze_source_with_width`]:
/// `no_core` unless a sysroot is named, and errors do not cost the facts that survive them.
///
/// `target` is a target tuple (`x86_64-pc-windows-msvc`) whose `cfg` the crate is read under;
/// `None` is the host's. A caller that wants every item a library has on any platform reads it
/// once per target and unions the facts. `edition` is the one the crate's manifest declares
/// (`2024`); `None` is 2015, as rustc's own default is. `items_only` reads what [`extract_items`]
/// reads: no body is type checked and no reference is reported.
pub fn analyze_crate(
    crate_name: &str,
    root: &eko::path::Path,
    sysroot: Option<&str>,
    target: Option<&str>,
    edition: Option<&str>,
    items_only: bool,
    width: usize,
) -> Result<CrateFacts, Refused> {
    read_crate(&CrateRead {
        sysroot,
        target,
        edition,
        items_only,
        width,
        ..CrateRead::new(crate_name, root)
    })
}

/// [`analyze_crate`] with everything a crate of a dependency chain needs: the crates it depends
/// on, loaded from the metadata frontend wrote when it read them, its `cfg` and build
/// environment, and its own metadata written for the crates that depend on it.
///
/// A chain is read in dependency order, each crate once: core with nothing loaded, then alloc
/// with core, std with core and alloc (and std's own dependencies), then a registry crate with
/// std and its dependencies. Paths, macros and re-exports into a loaded crate resolve as they
/// do in rustc, because it is rustc's loader reading rustc's metadata.
///
/// **Two crates of one name** coexist as they do under cargo: each is read with its own
/// [`CrateRead::disambiguator`] (cargo's `-C metadata`), the crate being read names the one in
/// its prelude, and a loaded crate gets the one it was read against ([`Dependency::prelude`]).
///
/// **Only source.** Every crate in the chain was read from source by this function; no
/// toolchain's library is opened, and the metadata this build writes is refused by any other
/// build (it carries this build's version string).
///
/// **What writing metadata costs.** Metadata holds what another crate's type check asks of this
/// one: every item's signature, and the bodies another crate evaluates or looks through, which
/// are each `const fn` and `const` (evaluated at compile time) and each function returning
/// `impl Trait` (an `async fn` among them, whose auto traits leak). Those bodies are type
/// checked when the metadata is written, and no other body is, with `items_only`.
pub fn read_crate(read: &CrateRead<'_>) -> Result<CrateFacts, Refused> {
    let setup = Setup {
        crate_name: read.crate_name,
        sysroot: read.sysroot,
        target: read.target,
        edition: read.edition,
        width: read.width,
        proc_macro: read.proc_macro,
        standard_library: read.standard_library,
        loaded: read.loaded,
        library: read.library,
        disambiguator: read.disambiguator,
        test: false,
        all_mir: read.all_mir,
    };
    let input = Input::File(read.root.to_path_buf());
    let refused = |diagnostics| Refused { crate_name: read.crate_name.to_string(), diagnostics };
    let facts =
        analyze_input(&setup, input, read.items_only, read.write_metadata).map_err(refused)?;
    // A library read is not judged, so what it recorded refuses nothing.
    if read.write_metadata.is_some() && !read.library && !facts.diagnostics.is_empty() {
        return Err(refused(facts.diagnostics));
    }
    Ok(facts)
}

/// Everything a session is built from except its input.
#[derive(Clone, Copy)]
struct Setup<'a> {
    crate_name: &'a str,
    sysroot: Option<&'a str>,
    target: Option<&'a str>,
    edition: Option<&'a str>,
    width: usize,
    proc_macro: bool,
    standard_library: bool,
    loaded: Loaded<'a>,
    /// A library read: `CrateRead`'s `library`.
    library: bool,
    /// `-C metadata`; see [`CrateRead::disambiguator`].
    disambiguator: Option<&'a str>,
    /// rustc's `--test`; see [`check_source_against`].
    test: bool,
    /// `-Zalways-encode-mir`; see [`CrateRead::all_mir`].
    all_mir: bool,
}

impl<'a> Setup<'a> {
    /// One crate, `no_core`, host target, edition 2015, serial, strict.
    fn plain(crate_name: &'a str) -> Self {
        Setup {
            crate_name,
            sysroot: None,
            target: None,
            edition: None,
            width: 1,
            proc_macro: false,
            standard_library: false,
            loaded: Loaded::default(),
            library: false,
            disambiguator: None,
            test: false,
            all_mir: false,
        }
    }

    /// The session's options, and whether its input may be read `no_core`: nothing to load
    /// (no sysroot, no dependency) and a root that does not declare it already.
    fn options(&self, input: &Input) -> Result<Options, Vec<String>> {
        let mut opts = Options::default();
        opts.crate_name = Some(self.crate_name.to_string());
        opts.crate_types =
            alloc::vec![if self.proc_macro { CrateType::ProcMacro } else { CrateType::Rlib }];
        // rustc's `--test`, which `collect_crate_types` reads as the one crate type `bin`,
        // whatever `crate_types` says, as rustc does.
        opts.test = self.test;
        // Options::default() disallows `#![feature]` the way a stable CLI would.
        // This crate is a nightly frontend; within-crate no_core analysis needs the
        // same gates nightly rustc has.
        opts.unstable_features = UnstableFeatures::Allow;
        opts.jobs.frontend = frontend_jobs(self.width);
        opts.unstable_opts.force_unstable_if_unmarked = self.standard_library;
        opts.unstable_opts.always_encode_mir = self.all_mir;
        // The one switch a library read sets (`Session::is_library_read`), and the lint cap
        // that goes with it: no lint judges a library, whatever levels its source sets.
        if self.library {
            opts.unstable_opts.library_read = true;
            opts.lint_cap = Some(crate::rustc_lint_defs::Level::Allow);
        }
        if let Some(target) = self.target {
            opts.target_triple = crate::rustc_target::spec::TargetTuple::from_tuple(target);
        }
        if let Some(edition) = self.edition {
            opts.edition = edition
                .parse()
                .map_err(|()| alloc::vec![format!("error: unknown edition `{edition}`")])?;
        }
        opts.externs = externs(self.loaded.dependencies);
        // Keyed by the metadata file, not the name: two crates of one name have two builds.
        opts.proc_macro_dylibs = self
            .loaded
            .dependencies
            .iter()
            .filter_map(|dependency| {
                let dylib = dependency.proc_macro_dylib.as_deref()?;
                Some((
                    eko::path::PathBuf::from(dependency.metadata.as_str()),
                    eko::path::PathBuf::from(dylib),
                ))
            })
            .collect();
        // rustc's `-C metadata`, which `StableCrateId::new` hashes with the crate's name.
        opts.cg.metadata = self.disambiguator.map(str::to_string).into_iter().collect();
        opts.logical_env = self.loaded.env.iter().cloned().collect();
        // **The sysroot is an optional parameter, because this is a parser.**
        //
        // Definitions, imports, impls and within-crate references are read out of
        // the source this was handed. None of them needs a standard library to
        // exist. A library only matters to a caller who wants paths resolved into
        // it, and that caller says so by naming a sysroot, or by handing over the
        // dependencies frontend itself read from source (`Loaded`).
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
        // A session handed its dependencies reads those and nothing else, so the environment's
        // sysroot is not asked for.
        let named = self.sysroot.map(eko::path::PathBuf::from).or_else(|| {
            self.loaded
                .dependencies
                .is_empty()
                .then(|| eko::env::var_os("FRONTEND_SYSROOT").map(eko::path::PathBuf::from))
                .flatten()
        });
        if named.is_some() {
            opts.sysroot = Sysroot::new(named);
        } else if self.loaded.dependencies.is_empty()
            && !matches!(input, Input::File(root) if syntax::declares_no_core(root))
        {
            // A crate that is `no_core` already (core itself) gets nothing added.
            opts.unstable_opts.crate_attr.push("no_core".to_string());
            opts.unstable_opts.crate_attr.push("feature(no_core)".to_string());
        }
        Ok(opts)
    }

    /// The version string the session claims: the named sysroot's, when there is one to agree
    /// with; otherwise the compiled-in one, which is also what metadata this build writes
    /// carries and what it expects of metadata it reads.
    fn rustc_version(&self, opts: &Options) -> Option<String> {
        #[cfg(feature = "force_pinned_sysroot")]
        {
            let _ = opts;
            None
        }
        #[cfg(not(feature = "force_pinned_sysroot"))]
        {
            // Only worth asking when something will actually be read. With
            // `no_core` no metadata is opened, so there is no vintage to agree
            // with and no reason to spawn a compiler to ask about one.
            if opts.sysroot.explicit.is_some() {
                rustc_version_of_sysroot(opts.sysroot.path()).or_else(host_rustc_version)
            } else {
                None
            }
        }
    }
}

/// `--extern` for each dependency: its file, exactly, and whether the crate being read can name
/// it.
///
/// A name with a prelude file keeps its `noprelude` files apart, in
/// `ExternEntry::transitive_files`: a lookup by name for the crate being read chooses among the
/// prelude files only, and the others are found only by the hash a loaded crate recorded for
/// them (see [`Dependency::prelude`]). A name with no prelude file keeps its files where a
/// lookup by name finds them, which is how an injected `std` or an `extern crate core` of a
/// `noprelude` crate is found, as in rustc.
fn externs(dependencies: &[Dependency]) -> crate::rustc_session::config::Externs {
    use crate::rustc_session::config::{ExternEntry, ExternLocation, Externs};
    use crate::rustc_session::utils::CanonicalizedPath;
    use alloc::collections::{BTreeMap, BTreeSet};
    // Per name: its prelude files, then its `noprelude` ones.
    let mut by_name: BTreeMap<String, (BTreeSet<CanonicalizedPath>, BTreeSet<CanonicalizedPath>)> =
        BTreeMap::new();
    for dependency in dependencies {
        let (prelude, noprelude) = by_name.entry(dependency.name.clone()).or_default();
        let file =
            CanonicalizedPath::new(eko::path::PathBuf::from(dependency.metadata.as_str()));
        let files = if dependency.prelude { prelude } else { noprelude };
        files.insert(file);
    }
    let map = by_name
        .into_iter()
        .map(|(name, (prelude, noprelude))| {
            let add_prelude = !prelude.is_empty();
            let (named, transitive_files) =
                if add_prelude { (prelude, noprelude) } else { (noprelude, BTreeSet::new()) };
            let entry = ExternEntry {
                location: ExternLocation::ExactPaths(named),
                is_private_dep: false,
                add_prelude,
                nounused_dep: true,
                force: false,
                transitive_files,
            };
            (name, entry)
        })
        .collect();
    Externs::new(map)
}

/// One analysis session over `input`, the one place every facts entry point builds it.
fn analyze_input(
    setup: &Setup<'_>,
    input: Input,
    items_only: bool,
    write_metadata: Option<&eko::path::Path>,
) -> Result<CrateFacts, Vec<String>> {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "analyze_source needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let mut opts = setup.options(&input)?;
    if let Some(path) = write_metadata {
        // What rustc's `--emit=metadata=PATH` records; it is also what makes lowering keep the
        // HIR hashes the metadata's crate hash is made of.
        opts.output_types = crate::rustc_session::config::OutputTypes::new(&[(
            crate::rustc_session::config::OutputType::Metadata,
            Some(crate::rustc_session::config::OutFileName::Real(path.to_path_buf())),
        )]);
    }
    let rustc_version = setup.rustc_version(&opts);
    let text = alloc::sync::Arc::new(eko::thread::Mutex::new(String::new()));
    // Shared, not leaked per call; see `USING_INTERNAL_FEATURES`.
    let using_internal_features = &USING_INTERNAL_FEATURES;
    let config = Config {
        opts,
        input,
        psess_created: Some(capture_diagnostics(&text)),
        using_internal_features,
        rustc_version,
        crate_cfg: setup.loaded.cfg.to_vec(),
    };
    // The facts are handed out through `extracted` rather than returned, because returning is
    // not the way out of a run that emitted an error: `run_compiler` ends every such run in
    // `abort_if_errors`, which unwinds past the return value. What `extract` finished before
    // that is kept here, and the unwind only tells us the run had errors.
    let mut extracted: Option<CrateFacts> = None;
    let library = setup.library;
    // Encoding began, and encoding returned.
    let mut written = false;
    let mut encoded = false;
    let finished = catch_fatal_errors(|| {
        run_compiler(config, |compiler| {
            let krate = parse(&compiler.sess);
            create_and_enter_global_ctxt(compiler, krate, |tcx| {
                extracted = Some(if items_only { extract_items(tcx) } else { extract(tcx) });
                // A crate with an error is no one's dependency, so its metadata is not written.
                // A library read is not judged: its metadata is written whatever it recorded.
                if let Some(path) = write_metadata
                    && (library || tcx.dcx().has_errors().is_none())
                {
                    written = true;
                    crate::rustc_metadata::encode_metadata(tcx, path, None);
                    encoded = true;
                }
            })
        })
    });
    // Only the errors that are kept become strings: warnings and the closing summary are read
    // in place and never copied.
    let mut errors = Vec::new();
    let captured = text.lock();
    for_each_diagnostic(&captured, |severity, entry| {
        if severity == Severity::Error && !entry.text.starts_with("error: aborting due to") {
            errors.push(entry.to_owned_string());
        }
    });
    drop(captured);
    // Metadata begun and then stopped by an error (encoding checks the bodies it carries) is
    // not a crate anyone may load. In a library read an error does not stop it; only an encoding
    // that did not return leaves metadata no one may load.
    let unfinished = if library { !encoded } else { finished.is_err() || !errors.is_empty() };
    if let Some(path) = write_metadata
        && written
        && unfinished
    {
        let _ = eko::file::remove_file(path);
    }
    // No facts means no HIR to read them from: the errors are the whole answer.
    let Some(mut facts) = extracted else {
        return Err(errors);
    };
    // A library read that was asked for metadata and has none has not done what it was asked:
    // the crates that depend on it cannot be read.
    if library && write_metadata.is_some() && !encoded {
        errors.push(format!(
            "error: the metadata of `{}` was not written: its encoding stopped on a fatal error",
            setup.crate_name
        ));
        return Err(errors);
    }
    facts.diagnostics = errors;
    // A library read's `complete` is `extract`'s, whole: its run ends in an error whenever it
    // recorded one, and nothing it records is a loss by itself (see `CrateFacts::complete`).
    if !library {
        facts.complete = facts.complete && finished.is_ok() && facts.diagnostics.is_empty();
    }
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
///
/// **The buffer is in serial order in parallel mode too, and nothing here has to do anything
/// for that.** This is the final sink: a diagnostic is rendered to text once, by this emitter,
/// when `DiagCtxt` prints it. A diagnostic emitted inside a par item does not get here when it
/// is emitted; it travels as the item's owned output and is printed when the item's turn comes
/// in item order (`rustc_errors::item_scope`, which every stage in `rustc_data_structures::sync`
/// runs every item of a parallel stage through). So the emitter is only ever called in the
/// order a serial run calls it, and always under the `DiagCtxt` lock, which is why `Sink`'s
/// own lock is never contended by two diagnostics at once.
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
/// dropped: it annotates a diagnostic rather than being one. An internal compiler error counts
/// as an error: input the compiler could not handle has not been shown to be clean.
///
/// The one splitter for every entry point here, `syntax` and `diagnostics` included, so what
/// counts as an error cannot differ between them. Each returned string is one allocation,
/// copied once out of `captured`.
pub(crate) fn split_diagnostics(captured: &str) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for_each_diagnostic(captured, |severity, entry| match severity {
        Severity::Error => errors.push(entry.to_owned_string()),
        Severity::Warning => warnings.push(entry.to_owned_string()),
    });
    (errors, warnings)
}

/// Whether a captured diagnostic is an error or a warning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Severity {
    Error,
    Warning,
}

/// One diagnostic in captured emitter output: its first line through its last location line,
/// borrowed from the capture.
#[derive(Clone, Copy)]
struct Entry<'a> {
    /// The lines exactly as captured, with the line breaks between them.
    text: &'a str,
    /// Some break inside `text` is `\r\n`, which the entry's string spells `\n`.
    crlf: bool,
}

impl Entry<'_> {
    /// The entry's lines joined by `\n`, in one allocation.
    fn to_owned_string(self) -> String {
        if !self.crlf {
            return String::from(self.text);
        }
        let mut out = String::with_capacity(self.text.len());
        for (index, line) in self.text.lines().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(line);
        }
        out
    }
}

/// Hand each error and warning in `captured` to `f`, in emission order, as a slice of it.
///
/// Lines are split the way `str::lines` splits them: at `\n`, with a `\r` right before it
/// dropped too. A line that starts with a space continues the current entry, and one before any
/// entry is dropped.
fn for_each_diagnostic<'a>(captured: &'a str, mut f: impl FnMut(Severity, Entry<'a>)) {
    let mut emit = |start: usize, end: usize, crlf: bool| {
        let text = &captured[start..end];
        let severity = if text.starts_with("error") || text.starts_with("internal compiler error")
        {
            Severity::Error
        } else if text.starts_with("warning") {
            Severity::Warning
        } else {
            return;
        };
        f(severity, Entry { text, crlf });
    };
    // The current entry: where it starts and ends, whether a break inside it is `\r\n`, and
    // whether its last line's own break is.
    let mut current: Option<(usize, usize, bool, bool)> = None;
    let mut pos = 0;
    for raw in captured.split_inclusive('\n') {
        let start = pos;
        pos += raw.len();
        let (line, line_crlf) = match raw.strip_suffix('\n') {
            Some(line) => match line.strip_suffix('\r') {
                Some(line) => (line, true),
                None => (line, false),
            },
            None => (raw, false),
        };
        let end = start + line.len();
        if line.starts_with(' ') {
            if let Some((_, entry_end, crlf, last_crlf)) = current.as_mut() {
                *crlf |= *last_crlf;
                *entry_end = end;
                *last_crlf = line_crlf;
            }
        } else {
            if let Some((entry_start, entry_end, crlf, _)) = current.take() {
                emit(entry_start, entry_end, crlf);
            }
            current = Some((start, end, false, line_crlf));
        }
    }
    if let Some((start, end, crlf, _)) = current {
        emit(start, end, crlf);
    }
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
    check_source_with_width(crate_name, source, 1)
}

/// [`check_source`], with up to `width` of this check's independent stage items running at
/// once on nagoya's pool. `1` (or `0`) is the serial compiler, and the answer does not depend
/// on it.
pub fn check_source_with_width(crate_name: &str, source: &str, width: usize) -> Checked {
    check_shared_source_with_width(crate_name, Arc::new(String::from(source)), width)
}

/// [`check_source_with_width`] over text the caller already shares. The session's source file
/// keeps `source` itself, so the text is never copied (unless it has a BOM or a `\r\n`, which
/// the source map normalizes away in its own copy).
pub fn check_shared_source_with_width(
    crate_name: &str,
    source: Arc<String>,
    width: usize,
) -> Checked {
    check_no_core_source(crate_name, source, width, false)
}

/// [`check_shared_source_with_width`], built with rustc's `--test` when `test` is set (see
/// [`check_source_against`]).
fn check_no_core_source(
    crate_name: &str,
    source: Arc<String>,
    width: usize,
    test: bool,
) -> Checked {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "check_source needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let mut opts = Options::default();
    opts.crate_name = Some(crate_name.to_string());
    opts.crate_types = alloc::vec![CrateType::Rlib];
    opts.test = test;
    opts.unstable_features = UnstableFeatures::Allow;
    opts.jobs.frontend = frontend_jobs(width);
    opts.unstable_opts.crate_attr.push("no_core".to_string());
    opts.unstable_opts.crate_attr.push("feature(no_core)".to_string());

    check_input(
        opts,
        Input::Str { name: FileName::anon_source_code(&source), input: source },
        Vec::new(),
    )
}

/// [`check_source`] against dependencies frontend read earlier: one source text, with its bodies
/// type checked, borrow checked and linted against the crates in `loaded`, which are loaded from
/// the metadata [`read_crate`] wrote for them. What rustc would say of the file compiled as its
/// crate's root with those crates as `--extern`s.
///
/// The file stands alone: it may be one module of a crate the caller does not have, so a path
/// into its own missing siblings (`crate::other`) fails as an unresolved import or path (E0432,
/// E0433), which the codes tell apart from a mistake about a dependency's API (E0599 for a
/// method no type in reach has, E0061 for a wrong argument count, E0425 for a missing item).
///
/// `edition` is its crate's; `None` is 2015. With no dependency this is [`check_source`].
///
/// **`test` is rustc's `--test`**, which `cargo check --all-targets` passes for a lib's test
/// target, so a file whose code sits in `#[cfg(test)]` modules and `#[test]` functions gets the
/// diagnostics cargo gives that target. As in rustc: `cfg(test)` is set, each `#[test]` function
/// is kept with the descriptor const its expansion writes beside it, the crate is a `bin`
/// whatever its crate type, and the harness adds a `main` that calls `test::test_main_static`.
/// Those descriptors and that `main` name the crate `test` (libtest) through `extern crate
/// test`, so **the caller supplies libtest**: the crate `test`, read from rust-src's
/// `library/test` like std (a library read, after std and libtest's other dependencies, with its
/// metadata written), in `loaded.dependencies` like any other dependency. Pass it as
/// [`Dependency::transitive`]: rustc finds libtest in the sysroot, not in the extern prelude, so
/// the file names `test::` only after its own `extern crate test`. Not supplied, the check says
/// what rustc says: E0463, can't find crate for `test`. It is a check still: nothing is emitted.
/// `false` is the check without `--test`.
pub fn check_source_against(
    crate_name: &str,
    source: Arc<String>,
    edition: Option<&str>,
    loaded: Loaded<'_>,
    width: usize,
    test: bool,
) -> Checked {
    if loaded.dependencies.is_empty() && edition.is_none() && loaded.cfg.is_empty() {
        return check_no_core_source(crate_name, source, width, test);
    }
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "check_source needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let setup = Setup { edition, width, loaded, test, ..Setup::plain(crate_name) };
    let input = Input::Str { name: FileName::anon_source_code(&source), input: source };
    let opts = match setup.options(&input) {
        Ok(opts) => opts,
        Err(errors) => return Checked { errors, warnings: Vec::new(), fatal: true },
    };
    check_input(opts, input, setup.loaded.cfg.to_vec())
}

/// What [`evaluate`] found when it ran a call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Evaluation {
    /// The call returned. `rendered` is the value as `{:?}` prints it, `ty` its type, and
    /// `steps` how many MIR statements and terminators the interpreter stepped to get it.
    Value { rendered: String, ty: String, steps: u64 },
    /// The call panicked: an overflow (overflow checks are on, as in a debug build), an index out
    /// of bounds, an `unwrap` of `None`, a `panic!`. `message` is what the panic says.
    Panicked { message: String },
    /// The call was not run to its end, and why: the source does not compile (its errors), it
    /// calls something this interpreter does not serve (a foreign function, named), it has
    /// undefined behavior, its value has no `Debug` (or its `Debug` failed), or the compiler
    /// hit a bug on the way (what it said).
    Refused { why: String },
    /// The caller's step budget ran out after `steps` steps.
    Exhausted { steps: u64 },
}

/// Run `call`, a Rust expression over the items `source` defines (`count("strawberry", 'r')`),
/// through rustc's MIR interpreter, and return its value.
///
/// **One semantics.** The source is compiled as rustc compiles it (type check, borrow check,
/// MIR building and optimization), with `call` as the body of a function appended to it, and
/// that function is run by `rustc_const_eval::interpret`, the interpreter under CTFE and Miri.
/// Every rule of what the program does is that interpreter's; the machine it runs on
/// (`interpreter.rs`) only decides what is served. Heap allocation is served from the
/// interpreter's own memory; so are writes to standard output and standard error (a program's
/// printing is its output), the one thread's thread-local statics, and the OS's random bytes, as
/// a fixed stream so a run reproduces. Any other foreign function (a syscall, a C library, file
/// or network I/O) is refused by name. Overflow checks are on, as in a debug build, and a panic
/// is returned as [`Evaluation::Panicked`] with its message, never a crash. Nothing unwinds out
/// of this function: a compiler bug on the way is a [`Evaluation::Refused`] that says what it
/// was.
///
/// **The value is rendered by its type's own `Debug`**, run on the same interpreter after the
/// call returns (`{:?}` of it, through `core::fmt`), so the rendering is the library's and not a
/// second implementation of it. A `no_core` source has no `Debug`; its primitive values
/// (integers, `bool`, `char`, floats, `str`, references, arrays, slices, tuples) are read by
/// layout instead.
///
/// **Library functions run from their MIR**, so every crate the call reaches has to have been
/// read with its MIR written ([`CrateRead::all_mir`]); a function whose MIR is missing is refused
/// by name. `loaded`, `edition` and the rest are what [`check_source_against`] takes: with no
/// dependency the source is read `no_core`.
///
/// `budget` bounds the steps; `None` runs until the call returns, however long that is.
pub fn evaluate(
    source: &str,
    edition: Option<&str>,
    loaded: Loaded<'_>,
    call: &str,
    budget: Option<u64>,
) -> Evaluation {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "evaluate needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    let setup = Setup { edition, loaded, ..Setup::plain("evaluated") };
    // A string input's text does not enter the options (only a file root is looked at, for a
    // `no_core` it declares), so they are taken first: whether a library is loaded decides what
    // is appended to the source.
    let probe =
        Input::Str { name: FileName::anon_source_code(""), input: Arc::new(String::new()) };
    let mut opts = match setup.options(&probe) {
        Ok(opts) => opts,
        Err(errors) => return Evaluation::Refused { why: errors.join("\n") },
    };
    opts.cg.overflow_checks = Some(true);
    opts.debug_assertions = true;
    let library = !opts.unstable_opts.crate_attr.iter().any(|attr| attr == "no_core");
    // Appended, so every span of the source is where it was. `impl Sized` lets the call's type
    // be whatever it is; the interpreter sees it revealed. With a library, the value is rendered
    // by its own `Debug`, which `interpreter::RENDER` runs.
    let entry = interpreter::ENTRY;
    let mut text =
        format!("{source}\n#[allow(warnings)]\nfn {entry}() -> impl Sized {{\n{call}\n}}\n");
    if library {
        text.push_str(interpreter::RENDER);
    }
    let source = Arc::new(text);
    let input = Input::Str { name: FileName::anon_source_code(&source), input: source };
    let text = alloc::sync::Arc::new(eko::thread::Mutex::new(String::new()));
    let captured = text.clone();
    let config = Config {
        opts,
        input,
        psess_created: Some(capture_diagnostics(&text)),
        using_internal_features: &USING_INTERNAL_FEATURES,
        rustc_version: None,
        crate_cfg: setup.loaded.cfg.to_vec(),
    };
    // Handed out rather than returned, as in `analyze_input`: a run with an error ends in an
    // unwind, not a return.
    let mut outcome: Option<Evaluation> = None;
    // Nothing unwinds out of `evaluate`: a fatal error is caught by `catch_fatal_errors`, and
    // any other panic (a compiler bug, a delayed bug flushed when the session ends) by this
    // outer catch, and each becomes a refusal that says what it was.
    let session = crate::unwind_janky::catch(|| {
        catch_fatal_errors(|| {
            run_compiler(config, |compiler| {
                let krate = parse(&compiler.sess);
                create_and_enter_global_ctxt(compiler, krate, |tcx| {
                    // Only a program rustc accepts is run.
                    tcx.analysis(());
                    if tcx.dcx().has_errors().is_some() {
                        return;
                    }
                    outcome = Some(evaluate_in(tcx, entry, library, budget, &captured, 0).0);
                })
            })
        })
    });
    let errors = || {
        let (mut errors, _) = split_diagnostics(&text.lock());
        errors.retain(|error| !error.starts_with("error: aborting due to"));
        errors
    };
    match (outcome, session) {
        (Some(outcome), Ok(_)) => outcome,
        (Some(outcome), Err(payload)) => Evaluation::Refused {
            why: with_errors(
                format!(
                    "the session panicked as it ended, after the call gave {outcome:?}: {}",
                    panic_text(&payload)
                ),
                &errors(),
            ),
        },
        (None, Ok(_)) => {
            let errors = errors();
            let why = if errors.is_empty() {
                "the source did not compile".to_string()
            } else {
                errors.join("\n")
            };
            Evaluation::Refused { why }
        }
        (None, Err(payload)) => Evaluation::Refused {
            why: with_errors(format!("the analyser panicked: {}", panic_text(&payload)), &errors()),
        },
    }
}

/// The part of [`evaluate`] inside the compiled session: find the appended functions and run the
/// call. A panic out of the interpreter is caught here, nearest to it, where its payload is still
/// its own; any error rustc reported while the call ran (a constant that failed, a compiler bug)
/// is added to a refusal, since that is what the refusal is about. Only what was reported from
/// byte `since` of `captured` on is the call's: a session shared by several calls
/// ([`evaluate_all`]) holds the others' too. The flag is whether the interpreter unwound, after
/// which the session is not trusted for another call.
fn evaluate_in(
    tcx: TyCtxt<'_>,
    entry: &str,
    library: bool,
    budget: Option<u64>,
    captured: &alloc::sync::Arc<eko::thread::Mutex<String>>,
    since: usize,
) -> (Evaluation, bool) {
    let function = |name: &str| {
        tcx.hir_crate_items(()).free_items().map(|item| item.owner_id.def_id).find(|&id| {
            tcx.def_kind(id) == DefKind::Fn && tcx.item_name(id.to_def_id()).as_str() == name
        })
    };
    let Some(id) = function(entry) else {
        return (
            Evaluation::Refused { why: "the call's function was not found".to_string() },
            false,
        );
    };
    let render = if library {
        match (function(interpreter::DEBUG), function(interpreter::SINK)) {
            (Some(debug), Some(sink)) => {
                Some(interpreter::Render { debug: debug.to_def_id(), sink: sink.to_def_id() })
            }
            _ => {
                return (
                    Evaluation::Refused {
                        why: "the functions that render a value were not found".to_string(),
                    },
                    false,
                );
            }
        }
    } else {
        None
    };
    let (evaluation, unwound) = match crate::unwind_janky::catch(|| {
        interpreter::run_entry(tcx, id, render, budget)
    }) {
        Ok(evaluation) => (evaluation, false),
        Err(payload) => (
            Evaluation::Refused {
                why: format!("the interpreter panicked: {}", panic_text(&payload)),
            },
            true,
        ),
    };
    let evaluation = match evaluation {
        Evaluation::Refused { why } => {
            let said = captured.lock();
            let (errors, _) = split_diagnostics(said.get(since..).unwrap_or(""));
            Evaluation::Refused { why: with_errors(why, &errors) }
        }
        other => other,
    };
    (evaluation, unwound)
}

/// What [`evaluate_all`] answered, and how many compiler sessions it took to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluatedAll {
    /// One per call, in the order given, each what [`evaluate`] answers for that call alone.
    pub evaluations: Vec<Evaluation>,
    /// The sessions run: one when every call built, one more for each round of calls whose
    /// errors were taken out, and one for each call asked alone.
    pub sessions: usize,
}

/// [`evaluate`] for every call of `calls` over the one `source`, sharing a compiler session: the
/// source is compiled once with one function per call appended, and each call is run by the
/// interpreter in that session, each on a machine of its own (its own memory, its own
/// statics' copies, its own budget), so no call sees another's values.
///
/// **Each answer is what the call's own session would give.** That is the contract, and every
/// way a shared session could blur it is taken out rather than approximated:
///
/// - An error is read by where rustc placed it. An error on a call's own lines is that call's:
///   it is refused with it, and the rest are compiled again without it, since `analysis` stops
///   at the first body with an error and the lints a clean call's own session would run have
///   not run yet. Rounds repeat until the calls left build, so a call refused by a lint is
///   refused by that lint, as alone.
/// - An error on the source's own lines refuses every call, with the source's errors and its
///   own, as each call's own session would.
/// - An error placed anywhere else (a library's file, `RENDER`'s lines, nowhere), a session
///   that stops before it has a context, a panic out of the interpreter, or a session that
///   panics as it ends: whatever that session had not settled is asked alone, with
///   [`evaluate`]. A shared session that cannot say whose a failure is, never guesses.
///
/// Nothing outlives the call: each session is dropped before the next.
pub fn evaluate_all(
    source: &str,
    edition: Option<&str>,
    loaded: Loaded<'_>,
    calls: &[&str],
    budget: Option<u64>,
) -> EvaluatedAll {
    let mut answers: Vec<Option<Evaluation>> = calls.iter().map(|_| None).collect();
    let mut sessions = 0;
    // The calls still to answer, by their place in `calls`.
    let mut open: Vec<usize> = (0..calls.len()).collect();
    while open.len() > 1 {
        let asked: Vec<&str> = open.iter().map(|&at| calls[at]).collect();
        sessions += 1;
        let Some(settled) = shared_session(source, edition, loaded, &asked, budget) else {
            break;
        };
        let mut left = Vec::new();
        for (&at, outcome) in open.iter().zip(settled) {
            match outcome {
                Some(outcome) => answers[at] = Some(outcome),
                None => left.push(at),
            }
        }
        if left.len() == open.len() {
            break;
        }
        open = left;
    }
    let evaluations = answers
        .into_iter()
        .zip(calls)
        .map(|(answer, call)| {
            answer.unwrap_or_else(|| {
                sessions += 1;
                evaluate(source, edition, loaded, call, budget)
            })
        })
        .collect();
    EvaluatedAll { evaluations, sessions }
}

/// One session over `source` with one function per call of `calls`: each call's answer when
/// the session could say it, `None` for a call to ask again, and `None` for the whole when the
/// session settled nothing it could vouch for. See [`evaluate_all`] for which is which.
fn shared_session(
    source: &str,
    edition: Option<&str>,
    loaded: Loaded<'_>,
    calls: &[&str],
    budget: Option<u64>,
) -> Option<Vec<Option<Evaluation>>> {
    let setup = Setup { edition, loaded, ..Setup::plain("evaluated") };
    let probe =
        Input::Str { name: FileName::anon_source_code(""), input: Arc::new(String::new()) };
    // Options that do not build refuse every call alike, as each call alone says.
    let mut opts = setup.options(&probe).ok()?;
    opts.cg.overflow_checks = Some(true);
    opts.debug_assertions = true;
    let library = !opts.unstable_opts.crate_attr.iter().any(|attr| attr == "no_core");
    let newlines = |text: &str| text.bytes().filter(|&byte| byte == b'\n').count();
    // The source, then each call's function on lines of its own, so a diagnostic is a call's
    // exactly when rustc places it on the call's lines. The source's spans are where
    // `evaluate` puts them.
    let mut text = format!("{source}\n");
    let source_lines = newlines(&text);
    let mut entries: Vec<(String, core::ops::RangeInclusive<usize>)> =
        Vec::with_capacity(calls.len());
    for (at, call) in calls.iter().enumerate() {
        let name = format!("{}_{at}", interpreter::ENTRY);
        let first = newlines(&text) + 1;
        text.push_str(&format!("#[allow(warnings)]\nfn {name}() -> impl Sized {{\n{call}\n}}\n"));
        entries.push((name, first..=newlines(&text)));
    }
    if library {
        text.push_str(interpreter::RENDER);
    }
    let source_text = Arc::new(text);
    let input = Input::Str { name: FileName::anon_source_code(&source_text), input: source_text };
    let said = alloc::sync::Arc::new(eko::thread::Mutex::new(String::new()));
    let captured = said.clone();
    let config = Config {
        opts,
        input,
        psess_created: Some(capture_diagnostics(&said)),
        using_internal_features: &USING_INTERNAL_FEATURES,
        rustc_version: None,
        crate_cfg: setup.loaded.cfg.to_vec(),
    };
    let mut settled: Option<Vec<Option<Evaluation>>> = None;
    let session = crate::unwind_janky::catch(|| {
        catch_fatal_errors(|| {
            run_compiler(config, |compiler| {
                let krate = parse(&compiler.sess);
                create_and_enter_global_ctxt(compiler, krate, |tcx| {
                    // `analysis` raises a fatal error once a body has one, after every body
                    // was type and borrow checked: caught here, so the errors can be read by
                    // where they are. Any other panic goes on out, and nothing is settled.
                    let analysed = catch_fatal_errors(|| tcx.analysis(())).is_ok();
                    if tcx.dcx().has_errors().is_none() {
                        // Stopped with nothing said: each call alone says what that was.
                        if !analysed {
                            return;
                        }
                        let mut each: Vec<Option<Evaluation>> =
                            entries.iter().map(|_| None).collect();
                        for ((name, _), slot) in entries.iter().zip(each.iter_mut()) {
                            let since = captured.lock().len();
                            let (evaluation, unwound) =
                                evaluate_in(tcx, name, library, budget, &captured, since);
                            // An interpreter that unwound leaves this session untrusted: that
                            // call and every one after it are asked again elsewhere.
                            if unwound {
                                break;
                            }
                            *slot = Some(evaluation);
                        }
                        settled = Some(each);
                        return;
                    }
                    let (errors, _) = split_diagnostics(&captured.lock());
                    let errors: Vec<String> = errors
                        .into_iter()
                        .filter(|error| !error.starts_with("error: aborting due to"))
                        .collect();
                    let mut own: Vec<Vec<usize>> = entries.iter().map(|_| Vec::new()).collect();
                    let mut in_source: Vec<usize> = Vec::new();
                    for (index, error) in errors.iter().enumerate() {
                        let Some(line) = placed_line(error) else {
                            return;
                        };
                        if line <= source_lines {
                            in_source.push(index);
                            continue;
                        }
                        let Some(at) = entries.iter().position(|(_, lines)| lines.contains(&line))
                        else {
                            return;
                        };
                        own[at].push(index);
                    }
                    if errors.is_empty() {
                        return;
                    }
                    let refused = |mine: &[usize]| Evaluation::Refused {
                        why: errors
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| in_source.contains(index) || mine.contains(index))
                            .map(|(_, error)| error.as_str())
                            .collect::<Vec<&str>>()
                            .join("\n"),
                    };
                    settled = Some(if in_source.is_empty() {
                        own.iter()
                            .map(|mine| (!mine.is_empty()).then(|| refused(mine.as_slice())))
                            .collect()
                    } else {
                        own.iter().map(|mine| Some(refused(mine.as_slice()))).collect()
                    });
                })
            })
        })
    });
    // A session that panicked as it ended would have refused each call alone, saying so; ask
    // them alone rather than say it for them.
    let _ended = session.ok()?;
    settled
}

/// The line of the text as compiled that rustc placed `error` on, from its first `-->` location,
/// when that is in the text (`<anon>`) and not in another file.
fn placed_line(error: &str) -> Option<usize> {
    let location = error.lines().map(str::trim).find_map(|line| line.strip_prefix("--> "))?;
    let (from, _) = location.rsplit_once(": ")?;
    let mut parts = from.rsplitn(3, ':');
    let _column = parts.next()?;
    let line = parts.next()?.parse().ok()?;
    (parts.next()? == "<anon>").then_some(line)
}

/// `why`, then what rustc reported, one diagnostic a line, when it reported anything.
fn with_errors(why: String, errors: &[String]) -> String {
    if errors.is_empty() { why } else { format!("{why}\n{}", errors.join("\n")) }
}

/// What a caught panic said: its payload when that is text, or else what the panic handler
/// recorded (`unwind_janky::record_panic`), which is also what survives `resume`'s re-raise. A
/// compiler bug's payload is not text; its message is among the session's diagnostics.
fn panic_text(payload: &crate::unwind_janky::Payload) -> String {
    let said = payload
        .downcast_ref::<&'static str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    let recorded = crate::unwind_janky::take_last_panic();
    match said {
        Some(said) if said != "resuming a caught panic" => said,
        said => recorded.or(said).unwrap_or_else(|| {
            "a panic whose payload is not text (a compiler bug; see the diagnostics)".to_string()
        }),
    }
}

/// `tcx.analysis(())` over one session, and what it said.
fn check_input(opts: Options, input: Input, crate_cfg: Vec<String>) -> Checked {
    let text = alloc::sync::Arc::new(eko::thread::Mutex::new(String::new()));
    // Shared, not leaked per call; see `USING_INTERNAL_FEATURES`.
    let using_internal_features = &USING_INTERNAL_FEATURES;
    let config = Config {
        opts,
        input,
        psess_created: Some(capture_diagnostics(&text)),
        using_internal_features,
        rustc_version: None,
        crate_cfg,
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

/// Collects one body's references as the `DefId` each resolved to, its kind and its span.
/// Each body gets its own visitor and its own `found`, so visitors on different workers share
/// nothing; [`body_fact`] turns the ids into paths once the walk is done.
struct RefVisitor<'a, 'tcx> {
    typeck: &'tcx crate::rustc_middle::ty::TypeckResults<'tcx>,
    found: &'a mut Vec<(DefId, RefKind, Span)>,
}

impl<'a, 'tcx> Visitor<'tcx> for RefVisitor<'a, 'tcx> {
    type NestedFilter = intravisit::IgnoreNested;
    type Result = ();
    fn visit_expr(&mut self, expr: &'tcx crate::rustc_hir::Expr<'tcx>) {
        match expr.kind {
            ExprKind::Path(ref qpath) => {
                if let Some(def_id) = self.typeck.qpath_res(qpath, expr.hir_id).opt_def_id() {
                    self.found.push((def_id, RefKind::Path, expr.span));
                }
            }
            ExprKind::MethodCall(_, _, _, _) => {
                if let Some(def_id) = self.typeck.type_dependent_def_id(expr.hir_id) {
                    self.found.push((def_id, RefKind::Method, expr.span));
                }
            }
            _ => {}
        }
        intravisit::walk_expr(self, expr);
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
                "error[E0425]: cannot find value `x` in this scope\n  --> src/lib.rs:1:14"
                    .to_string(),
                "error: aborting due to 1 previous error".to_string(),
            ]
        );
        assert_eq!(warnings, vec!["warning: unused variable: `y`".to_string()]);
    }

    // The split hands out slices of the capture, so a `\r\n` break inside an entry has to come
    // out as `\n`, which is what splitting with `str::lines` and joining did. A bare `\r` is not
    // a break and stays, and a location line before any entry is dropped.
    #[test]
    fn crlf_breaks_inside_an_entry_read_as_newlines() {
        let captured =
            "  --> orphan.rs:1:1\r\nerror: a\r\n  --> a.rs:1:1\r\n  --> a.rs:2:2\nwarning: w\r\nerror: b\r";
        let (errors, warnings) = split_diagnostics(captured);
        assert_eq!(
            errors,
            vec!["error: a\n  --> a.rs:1:1\n  --> a.rs:2:2".to_string(), "error: b\r".to_string()]
        );
        assert_eq!(warnings, vec!["warning: w".to_string()]);
    }
}
