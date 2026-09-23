//! Late resolution's unresolved-path errors, recorded where the walk finds them and finished
//! after it, with their suggestion search run as one stage over the frozen resolver.
//!
//! # What used to happen in place
//!
//! `smart_resolve_path_fragment` (`late.rs`) reports a path that does not resolve through
//! `smart_resolve_report_errors`. After building the base error, that ran the "relaxed lookup"
//! (`try_lookup_name_relaxed`) and its follow-ups, and those are the searches that dominate
//! late resolution on a corpus full of unresolved names:
//!
//! - **A**, `lookup_import_candidates` for the name: a walk of every module of the crate;
//! - **B**, the same walk again for enum variants of that name, when A found nothing;
//! - **C**, `lookup_typo_candidate`: every name in scope (the ribs, the enclosing modules, the
//!   preludes) against the unresolved one, by edit distance; run up to three times per error,
//!   with the same inputs each time;
//! - **D**, `smart_resolve_partial_mod_path_errors`: a third walk, for modules of that name,
//!   when nothing else was found.
//!
//! # What runs where now
//!
//! Everything in that function that reads the *walk's* state still runs in place, because the
//! walk's state (its ribs, the item and function it is in, its diagnostic metadata, its unused
//! labels) is gone once the walk moves on. That is the base error, every help that returns
//! early, and every help after the searches whose *decision* reads the walk (the `self.`
//! suggestions, the `where` clause restriction, the associated type from bounds, the `let`
//! suggestions, the similarly named labels, the hygiene hint). Each of those is computed in
//! place as data ([`FailureFacts`]), not written into the error, because in the error they sit
//! between things the searches add, and the error's children and suggestions are ordered.
//!
//! What the searches need is recorded as [`FailureQuery`]: the path, the scope, the source's
//! expectation, and, for the typo search, the names the walk's ribs contribute and where the
//! rib walk stops ([`TypoSource`]). The ribs are the only walk state the typo search reads, and
//! they are read as the list of names they held at the failure, which is exactly the data the
//! search consumes. Nothing else is copied: the module graph, the resolutions, the AST are all
//! read in place by the stage, through `&Resolver`.
//!
//! The stage ([`Resolver::flush_late_failures`], through `frozen.rs`) runs A, B, C once and D
//! per failure and returns [`LateSearch`], owned. Then, serially and in failure order,
//! [`Resolver::finish_late_failure`] writes into each error exactly what the old code wrote,
//! in the order it wrote it, from the facts and the answers, and
//! [`Resolver::settle_late_failure`] does with the error what the call site did: a `use`
//! injection, a stash, or an emission.
//!
//! Only errors with *no* resolution (`res == None`, the unresolved names) are split this way;
//! a path that resolved to the wrong kind of thing (`res == Some(..)`) goes through the
//! original code in place, because its context-dependent help writes into the error between the
//! searches' writes and reads the walk throughout. So does a path whose source borrows another
//! source (`PathSource::TraitItem`), which cannot be recorded. Both are rare next to the plain
//! unresolved name.
//!
//! # Order
//!
//! The output is what the in-place code produced, in the same order:
//!
//! - **Within an error**: [`Resolver::finish_late_failure`] follows the old control flow step
//!   by step, taking each walk-dependent decision from the facts and each search result from
//!   the answer. The decisions never depended on the searches (the early returns of the relaxed
//!   lookup read only the walk), so computing them first changes nothing, and the one side
//!   effect among them (a label suggested for a `break` is taken out of the unused labels) is
//!   still done in place, under the same condition.
//! - **Between errors that become `use` injections**: each failure takes a slot in
//!   [`LateFailureLog`]'s injections when it is recorded, so the injections come out in the
//!   order the walk found them, however late their search finishes.
//! - **Against everything else late resolution emits**: most of these errors were never emitted
//!   in place (they were `use` injections, emitted by `report_errors` after resolution), and
//!   the ones that stash do not print until the stash is flushed. The one kind that did print
//!   in place is a failed call path of three or more segments with no import candidate
//!   (`report_errors_for_call`'s `err.emit()`). While one of those waits, every other emission
//!   late resolution can make first calls [`Resolver::before_emit`], which runs the search for
//!   everything waiting and emits the waiting ones in order, before the new one. So the stage
//!   is one stage per stretch of the walk between two such emissions: one, for a file whose
//!   only errors are unresolved names. [`Resolver::flush_late_failures`] checks, in debug
//!   builds, that no error was counted while one was waiting, which is what a missed emission
//!   site would look like.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use core::mem::{swap, take};

use crate::rustc_ast::{Expr, ExprKind, NodeId};
use crate::rustc_data_structures::fx::FxHashSet;
use crate::rustc_errors::{Applicability, Diag, StashKey, SuggestionStyle, Suggestions};
use crate::rustc_hir::def::DefKind;
use crate::rustc_hir::def::Namespace::{self, *};
use crate::rustc_hir::def_id::CRATE_DEF_ID;
use crate::rustc_span::hygiene::SyntaxContext;
use crate::rustc_span::{DesugaringKind, Span, Spanned, Symbol};

use super::{AssocSuggestion, BaseError, TypoCandidate, import_candidate_to_enum_paths};
use crate::rustc_resolve::diagnostics::impls::{ImportSuggestion, TypoSuggestion};
use crate::rustc_resolve::late::{LateResolutionVisitor, RibKind};
use crate::rustc_resolve::{
    Module, ModuleOrUniformRoot, ParentScope, PathResult, PathSource, Res, ResolutionError,
    Resolver, ScopeSet, Segment, UseError, path_names_to_string,
};

// ---- the log -------------------------------------------------------------------------------

/// Late resolution's unresolved-path errors, in the order the walk found them.
///
/// Lives on the resolver, because emission sites inside name lookup (`ident.rs`) have to be
/// able to flush it (`Resolver::before_emit`), and they see only the resolver. Filled and
/// emptied during late resolution only: `late_resolve_crate` takes the injections, after a last
/// flush, when the walk is over.
#[derive(Default)]
pub(crate) struct LateFailureLog<'ra, 'tcx> {
    /// One slot per recorded failure, in failure order: the `use` injection it ends as, if it
    /// ends as one. A failure that is still waiting has `None` here until it is settled; one
    /// that ends as a stash or an emission keeps `None`.
    injections: Vec<Option<UseError<'tcx>>>,
    /// Failures recorded and not yet searched, in failure order.
    waiting: Vec<LateFailure<'ra, 'tcx>>,
    /// How many of `waiting` emit in place once settled (see [`Shell::emits_in_place`]).
    waiting_emitters: usize,
    /// `err_count` when the first of those started waiting: a debug check that nothing was
    /// emitted past a waiting one (`Resolver::flush_late_failures`).
    errors_before_waiting: usize,
}

impl LateFailureLog<'_, '_> {
    /// Whether a failure that emits in place is waiting, so a new emission has to flush first.
    pub(crate) fn has_waiting_emitters(&self) -> bool {
        self.waiting_emitters != 0
    }
}

/// One recorded failure that waits for its search.
struct LateFailure<'ra, 'tcx> {
    planned: PlannedFailure<'ra, 'tcx>,
    shell: Shell<'ra, 'tcx>,
    /// Its slot in [`LateFailureLog::injections`].
    slot: usize,
}

/// What `prepare_smart_resolve_report` hands back for one failing path.
pub(crate) enum Prepared<'ra, 'tcx> {
    /// The error is complete, suggestions and import candidates included: it returned before
    /// the searches, or it went through the original code in place (a resolution of the wrong
    /// kind, or a source that cannot be recorded).
    Done(Diag<'tcx>, Vec<ImportSuggestion>),
    /// The error as far as the walk takes it, and what its search needs.
    Planned(PlannedFailure<'ra, 'tcx>),
}

/// An error waiting for its search: the error itself, what the stage reads, and what the walk
/// decided for the rest of it.
pub(crate) struct PlannedFailure<'ra, 'tcx> {
    /// The base error with everything the code before the relaxed lookup added to it: message,
    /// code, primary span, labels, notes, suggestions. Built once, in place, and moved along.
    err: Diag<'tcx>,
    query: FailureQuery<'ra>,
    facts: FailureFacts,
}

/// What the stage reads for one failure. Owned, and nothing in it is a copy of resolver
/// state: the path's segments (which become the `use` injection's path, as they always did),
/// the scope (a handful of arena references), and the names the walk's ribs held.
struct FailureQuery<'ra> {
    path: Vec<Segment>,
    following_seg: Option<Segment>,
    /// The path's source with its borrowed expression dropped: the search needs its namespace
    /// and what it expects (`PathSource::is_expected`), and neither reads the expression.
    source: PathSource<'ra, 'ra, 'ra>,
    parent_scope: ParentScope<'ra>,
    /// Where the typo search looks, in the order it looks, as the walk's ribs decide it.
    typo_sources: Vec<TypoSource<'ra>>,
    /// The relaxed lookup ended at a help the walk found ([`FailureFacts::found`]): the search
    /// stops after the typo candidate, as the lookup returned there.
    found: bool,
}

/// One step of the typo search, in the order `lookup_typo_candidate` took them.
#[derive(Clone, Copy)]
pub(super) enum TypoSource<'ra> {
    /// A name one of the walk's ribs bound, which passed the filter and the rib's hygiene.
    Rib(TypoSuggestion),
    /// A block's anonymous module, read with the syntax context the rib walk had reached.
    Block(Module<'ra>, SyntaxContext),
    /// The module the rib walk stopped at, and every scope outward from it: `ScopeSet::All`.
    Scope(ParentScope<'ra>, Span),
    /// The module a multi-segment path's prefix resolved to.
    Module(Module<'ra>),
}

/// What the walk decided for the part of the error after the searches. See the module header
/// for why these are data and not writes.
struct FailureFacts {
    /// `smart_resolve_report_errors`' span.
    span: Span,
    base_error: BaseError,
    /// A help the relaxed lookup returned with, found in the walk's state; `None` if it went on.
    found: Option<FoundHelp>,
    /// The rest of the error, when the relaxed lookup went on.
    tail: Option<TailFacts>,
}

/// The helps the relaxed lookup returns early with, for an unresolved name.
enum FoundHelp {
    /// A field or associated item of `Self` has the name (`lookup_assoc_candidate`).
    Assoc {
        candidate: AssocSuggestion,
        self_is_available: bool,
        /// `name: ` for a shorthand field in a struct expression.
        pre: String,
        /// The name is a named argument of a format string.
        format_named_arg: bool,
    },
    /// The call's first argument is `self`: suggest calling it as a method.
    CallAsMethod { call_span: Span, message: String, suggestion: String },
    /// A local of that name was bound in a block the walk has left.
    OtherScope(Span),
}

/// What the walk decided for the error after the relaxed lookup, when that went on.
struct TailFacts {
    /// `restrict_assoc_type_in_where_clause`: whether it applies, and its suggestion.
    where_restriction: (bool, Option<(Span, String, String)>),
    typo: TypoFacts,
    /// `err_code_special_cases`, in the order it writes.
    special_cases: Vec<SpecialCase>,
}

/// What `suggest_typo` does, as far as the walk decides it.
enum TypoFacts {
    /// `suggest_assoc_type_from_bounds` found these, and `suggest_typo` returns right after.
    AssocFromBounds(Vec<String>),
    Typo {
        /// `let x: T = ..` where `=` was probably meant: the span between the pattern and type.
        assign: Option<Span>,
        /// `let_binding_suggestion`.
        let_binding: Option<(Span, &'static str, String)>,
    },
}

/// One write of `err_code_special_cases`.
pub(super) enum SpecialCase {
    /// A label with a similar name exists.
    SimilarLabel(Span),
    /// ... and this is a `break` with a value: use that label.
    UseLabel(Symbol),
    /// An identifier of that name exists but hygiene hides it.
    HiddenByHygiene(Span, &'static str),
    /// The name of a type in another language, and the Rust type it probably means.
    LikelyType(Symbol),
}

/// What the stage finds for one failure.
pub(crate) struct LateSearch {
    /// A, with intrinsics dropped when there are others, or D when the lookup went on and A
    /// found nothing.
    candidates: Vec<ImportSuggestion>,
    /// B, sorted: `(variant path, enum path)`.
    enum_candidates: Vec<(String, String)>,
    /// C.
    typo: TypoCandidate,
}

/// What the call site does with a finished error.
pub(crate) enum Shell<'ra, 'tcx> {
    /// `smart_resolve_path_fragment`'s `report_errors`: a `use` injection.
    Report(ReportShell<'tcx>),
    /// `smart_resolve_path_fragment`'s `report_errors_for_call`: merged with the path's own
    /// resolution error, then a `use` injection, a stash, or an emission.
    Call(CallShell<'ra>),
}

pub(crate) struct ReportShell<'tcx> {
    pub(crate) node_id: NodeId,
    pub(crate) instead: bool,
    pub(crate) suggestion: Option<(Span, &'static str, String, Applicability)>,
    /// `suggest_adding_generic_parameter`'s error, which replaces this one.
    pub(crate) replacement: Option<Diag<'tcx>>,
    pub(crate) is_call: bool,
}

pub(crate) struct CallShell<'ra> {
    /// The resolution error of the whole path, whose message and code the error takes.
    pub(crate) parent: Spanned<ResolutionError<'ra>>,
    pub(crate) node_id: NodeId,
    /// `should_report_errs` at the failure.
    pub(crate) report: bool,
    /// For `a::b` with `a` single: stash here instead of emitting, for typeck to improve.
    pub(crate) stash_at: Option<Span>,
    pub(crate) is_call: bool,
}

impl Shell<'_, '_> {
    /// Whether the error is emitted as soon as it is settled, if it ends with no import
    /// candidate: the one kind of these errors that prints in place.
    fn emits_in_place(&self) -> bool {
        matches!(self, Shell::Call(CallShell { report: true, stash_at: None, .. }))
    }
}

// ---- in place: planning ---------------------------------------------------------------------

/// The path's source in a form a record can keep, or `None` for a source that borrows another
/// (`TraitItem`). The expressions of `Expr` and `Struct` are dropped: see [`FailureQuery`].
pub(super) fn recordable_source<'ra>(
    source: PathSource<'_, '_, 'ra>,
) -> Option<PathSource<'ra, 'ra, 'ra>> {
    Some(match source {
        PathSource::Type => PathSource::Type,
        PathSource::Trait(alias) => PathSource::Trait(alias),
        PathSource::Expr(_) => PathSource::Expr(None),
        PathSource::Pat => PathSource::Pat,
        PathSource::Struct(_) => PathSource::Struct(None),
        PathSource::TupleStruct(span, spans) => PathSource::TupleStruct(span, spans),
        PathSource::TraitItem(..) => return None,
        PathSource::Delegation => PathSource::Delegation,
        PathSource::ExternItemImpl => PathSource::ExternItemImpl,
        PathSource::PreciseCapturingArg(ns) => PathSource::PreciseCapturingArg(ns),
        PathSource::ReturnTypeNotation => PathSource::ReturnTypeNotation,
        PathSource::DefineOpaques => PathSource::DefineOpaques,
        PathSource::Macro => PathSource::Macro,
        PathSource::Module => PathSource::Module,
    })
}

/// The span the relaxed lookup's suggestions replace: with a following segment, only the path
/// itself, not what follows it. `try_lookup_name_relaxed` computes it on entry.
fn relaxed_span(path: &[Segment], following_seg: Option<&Segment>, span: Span) -> Span {
    match following_seg {
        Some(_) if path[0].ident.span.eq_ctxt(path[path.len() - 1].ident.span) => {
            path[0].ident.span.to(path[path.len() - 1].ident.span)
        }
        _ => span,
    }
}

impl<'ast, 'ra, 'tcx> LateResolutionVisitor<'_, 'ast, 'ra, 'tcx> {
    /// Record everything the rest of `smart_resolve_report_errors` needs from the walk, for an
    /// unresolved name (`res == None`) whose source can be recorded, at the point where that
    /// function started its relaxed lookup. `err` is the error as far as it got.
    pub(super) fn plan_failure(
        &mut self,
        err: Diag<'tcx>,
        path: &[Segment],
        following_seg: Option<&Segment>,
        span: Span,
        source: PathSource<'_, 'ast, 'ra>,
        recorded_source: PathSource<'ra, 'ra, 'ra>,
        base_error: BaseError,
    ) -> PlannedFailure<'ra, 'tcx> {
        let is_expected = &|res| source.is_expected(res);
        // In the order the old code read the walk: the typo search's ribs (`try_lookup_name_
        // relaxed`, first thing after the import candidates), the early returns, then the
        // rest. Only the last has a side effect, and only when the lookup went on.
        let typo_sources = self.typo_sources(path, source.namespace(), is_expected);
        let found = self.found_help(path, following_seg, span, source);
        let tail = match found {
            Some(_) => None,
            None => Some(self.tail_facts(&err, path, span, source, &base_error)),
        };
        let query = FailureQuery {
            path: path.to_vec(),
            following_seg: following_seg.copied(),
            source: recorded_source,
            parent_scope: self.parent_scope,
            typo_sources,
            found: found.is_some(),
        };
        PlannedFailure { err, query, facts: FailureFacts { span, base_error, found, tail } }
    }

    /// The helps `try_lookup_name_relaxed` returns early with for an unresolved name, in its
    /// order: an associated item or field of `Self`, a call with `self` as its first argument,
    /// a local bound in a block the walk has left. Reads only the walk and the frozen tables.
    fn found_help(
        &mut self,
        path: &[Segment],
        following_seg: Option<&Segment>,
        span: Span,
        source: PathSource<'_, 'ast, 'ra>,
    ) -> Option<FoundHelp> {
        let span = relaxed_span(path, following_seg, span);
        let ident = path.last().unwrap().ident;
        let is_expected = &|res| source.is_expected(res);
        let ns = source.namespace();
        let path_str = Segment::names_to_string(path);
        if let [segment] = path
            && !matches!(source, PathSource::Delegation)
            && self.self_type_is_available()
        {
            if let Some(candidate) =
                self.lookup_assoc_candidate(ident, ns, is_expected, source.is_call())
            {
                let self_is_available = self.self_value_is_available(segment.ident.span);
                // Account for `Foo { field }` when suggesting `self.field` so we result on
                // `Foo { field: self.field }`.
                let pre = match source {
                    PathSource::Expr(Some(Expr { kind: ExprKind::Struct(expr), .. }))
                        if expr
                            .fields
                            .iter()
                            .any(|f| f.ident == segment.ident && f.is_shorthand) =>
                    {
                        format!("{path_str}: ")
                    }
                    _ => String::new(),
                };
                let format_named_arg = matches!(
                    span.desugaring_kind(),
                    Some(DesugaringKind::FormatLiteral { .. })
                ) && self
                    .r
                    .tcx
                    .sess
                    .source_map()
                    .span_to_source(span, |s, start, _| {
                        Ok(s.get(start.saturating_sub(1)..start) == Some("{"))
                    })
                    .unwrap_or(false);
                return Some(FoundHelp::Assoc { candidate, self_is_available, pre, format_named_arg });
            }

            // If the first argument in call is `self` suggest calling a method.
            if let Some((call_span, args_span)) = self.call_has_self_arg(source) {
                let mut args_snippet = String::new();
                if let Some(args_span) = args_span
                    && let Ok(snippet) = self.r.tcx.sess.source_map().span_to_snippet(args_span)
                {
                    args_snippet = snippet;
                }
                // With no resolution, the lookup always takes this branch's `else`: suggest
                // `self.name(..)`.
                return Some(FoundHelp::CallAsMethod {
                    call_span,
                    message: format!("try calling `{ident}` as a method"),
                    suggestion: format!("self.{path_str}({args_snippet})"),
                });
            }
        }

        // Try to find in last block rib
        if let Some(rib) = &self.last_block_rib {
            for (ident, &res) in &rib.bindings {
                if let Res::Local(_) = res
                    && path.len() == 1
                    && ident.span.eq_ctxt(path[0].ident.span)
                    && ident.name == path[0].ident.name
                {
                    return Some(FoundHelp::OtherScope(ident.span));
                }
            }
        }
        None
    }

    /// What `smart_resolve_report_errors` decides from the walk after the relaxed lookup, for
    /// an unresolved name: `suggest_trait_and_bounds` (which with no resolution is only the
    /// `where` clause restriction), `suggest_typo`'s walk-dependent parts, and
    /// `err_code_special_cases`, whose unused-label side effect happens here, as it did.
    fn tail_facts(
        &mut self,
        err: &Diag<'tcx>,
        path: &[Segment],
        span: Span,
        source: PathSource<'_, 'ast, 'ra>,
        base_error: &BaseError,
    ) -> TailFacts {
        let where_restriction = self.where_restriction_plan(span);
        let ident_span = path.last().map_or(span, |ident| ident.ident.span);
        let typo = match self.assoc_type_from_bounds_plan(source, path, ident_span) {
            Some(suggestions) => TypoFacts::AssocFromBounds(suggestions),
            None => TypoFacts::Typo {
                assign: match self.diag_metadata.current_let_binding {
                    Some((pat_sp, Some(ty_sp), None))
                        if ty_sp.contains(base_error.span) && base_error.could_be_expr =>
                    {
                        Some(pat_sp.between(ty_sp))
                    }
                    _ => None,
                },
                // The code has not changed since the base error: nothing between here and the
                // prefix sets one for an unresolved name.
                let_binding: self.let_binding_plan(err.code, ident_span),
            },
        };
        let special_cases = self.special_cases_plan(err.code, source, path, span);
        TailFacts { where_restriction, typo, special_cases }
    }

    /// Where `lookup_typo_candidate` looks for `path`, as far as the walk decides it: for a
    /// single segment, the names of every rib up to the first module rib (with each block's
    /// module and the module the walk stops at, in order); for a longer path, the module its
    /// prefix resolves to.
    pub(super) fn typo_sources(
        &mut self,
        path: &[Segment],
        ns: Namespace,
        filter_fn: &impl Fn(Res) -> bool,
    ) -> Vec<TypoSource<'ra>> {
        let mut sources = Vec::new();
        if let [segment] = path {
            let mut ctxt = segment.ident.span.ctxt();

            // Search in lexical scope.
            // Walk backwards up the ribs in scope and collect candidates.
            for rib in self.ribs[ns].iter().rev() {
                let rib_ctxt = if rib.kind.contains_params() {
                    ctxt.normalize_to_macros_2_0()
                } else {
                    ctxt.normalize_to_macro_rules()
                };

                // Locals and type parameters
                for (ident, &res) in &rib.bindings {
                    if filter_fn(res) && ident.span.ctxt() == rib_ctxt {
                        sources.push(TypoSource::Rib(TypoSuggestion::new(
                            ident.name, ident.span, res,
                        )));
                    }
                }

                if let RibKind::Block(Some(module)) = rib.kind {
                    sources.push(TypoSource::Block(module.to_module(), ctxt));
                } else if let RibKind::Module(module) = rib.kind {
                    // Encountered a module item, abandon ribs and look into that module and
                    // preludes.
                    let parent_scope = ParentScope { module: module.to_module(), ..self.parent_scope };
                    sources.push(TypoSource::Scope(parent_scope, segment.ident.span.with_ctxt(ctxt)));
                    break;
                }

                if let RibKind::MacroDefinition(def) = rib.kind
                    && def == self.r.macro_def(ctxt)
                {
                    // If an invocation of this macro created `ident`, give up on `ident`
                    // and switch to `ident`'s source from the macro definition.
                    ctxt.remove_mark();
                }
            }
        } else {
            // Search in module.
            let mod_path = &path[..path.len() - 1];
            if let PathResult::Module(ModuleOrUniformRoot::Module(module)) =
                self.resolve_path(mod_path, Some(TypeNS), None, PathSource::Type)
            {
                sources.push(TypoSource::Module(module));
            }
        }
        sources
    }
}

// ---- the stage: searching -------------------------------------------------------------------

impl<'ra, 'tcx> Resolver<'ra, 'tcx> {
    /// The typo candidate for `path` from the places `sources` names, in order. What
    /// `lookup_typo_candidate` did after its rib walk: every step reads only the frozen
    /// resolver, and the edit distance, the expensive part, is here.
    pub(super) fn typo_candidate_from(
        &self,
        sources: &[TypoSource<'ra>],
        path: &[Segment],
        following_seg: Option<&Segment>,
        ns: Namespace,
        filter_fn: &impl Fn(Res) -> bool,
    ) -> TypoCandidate {
        let mut names = Vec::new();
        for &source in sources {
            match source {
                TypoSource::Rib(suggestion) => names.push(suggestion),
                TypoSource::Block(module, ctxt) => {
                    self.add_module_candidates(module, &mut names, &filter_fn, Some(ctxt))
                }
                TypoSource::Scope(parent_scope, span) => self.add_scope_set_candidates(
                    &mut names,
                    ScopeSet::All(ns),
                    &parent_scope,
                    span,
                    filter_fn,
                ),
                TypoSource::Module(module) => {
                    self.add_module_candidates(module, &mut names, &filter_fn, None)
                }
            }
        }

        // if next_seg is present, let's filter everything that does not continue the path
        if let Some(following_seg) = following_seg {
            names.retain(|suggestion| match suggestion.res {
                Res::Def(DefKind::Struct | DefKind::Enum | DefKind::Union, _) => {
                    // FIXME: this is not totally accurate, but mostly works
                    suggestion.candidate != following_seg.ident.name
                }
                Res::Def(DefKind::Mod, def_id) => {
                    let module = self.expect_module(def_id);
                    self.resolutions(module)
                        .iter()
                        .any(|(key, _)| key.ident.name == following_seg.ident.name)
                }
                _ => true,
            });
        }
        let name = path[path.len() - 1].ident.name;
        // Make sure error reporting is deterministic.
        names.sort_by(|a, b| a.candidate.as_str().cmp(b.candidate.as_str()));

        match crate::rustc_span::edit_distance::find_best_match_for_name(
            &names.iter().map(|suggestion| suggestion.candidate).collect::<Vec<Symbol>>(),
            name,
            None,
        ) {
            Some(found) => {
                let Some(sugg) = names.into_iter().find(|suggestion| suggestion.candidate == found)
                else {
                    return TypoCandidate::None;
                };
                if found == name {
                    TypoCandidate::Shadowed(sugg.res, sugg.span)
                } else {
                    TypoCandidate::Typo(sugg)
                }
            }
            _ => TypoCandidate::None,
        }
    }

    /// Modules named like the last segment of `prefix_path` that have a child named like
    /// `following_seg`: `smart_resolve_partial_mod_path_errors`, which reads only the frozen
    /// resolver and the scope.
    pub(super) fn partial_mod_path_candidates(
        &self,
        parent_scope: &ParentScope<'ra>,
        prefix_path: &[Segment],
        following_seg: Option<&Segment>,
    ) -> Vec<ImportSuggestion> {
        if let Some(segment) = prefix_path.last()
            && let Some(following_seg) = following_seg
        {
            let candidates = self.lookup_import_candidates(
                segment.ident,
                Namespace::TypeNS,
                parent_scope,
                &|res: Res| matches!(res, Res::Def(DefKind::Mod, _)),
            );
            // double check next seg is valid
            candidates
                .into_iter()
                .filter(|candidate| {
                    if let Some(def_id) = candidate.did
                        && let Some(module) = self.get_module(def_id)
                    {
                        Some(def_id) != parent_scope.module.opt_def_id()
                            && self
                                .resolutions(module)
                                .iter()
                                .any(|(key, _r)| key.ident.name == following_seg.ident.name)
                    } else {
                        false
                    }
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    }

    /// One stage item: the searches of one failure, over the frozen resolver. A, then B if A
    /// found nothing and an enum is expected, then C, then D if the relaxed lookup went on and
    /// A found nothing, exactly as `try_lookup_name_relaxed` ran them for an unresolved name.
    fn search_late_failure(&self, query: &FailureQuery<'ra>) -> LateSearch {
        let source = query.source;
        let is_expected = &|res| source.is_expected(res);
        let ns = source.namespace();
        let ident = query.path.last().unwrap().ident;

        // A. With no resolution there is no definition to leave out of the candidates.
        let mut candidates =
            self.lookup_import_candidates(ident, ns, &query.parent_scope, is_expected);
        // Try to filter out intrinsics candidates, as long as we have
        // some other candidates to suggest.
        let intrinsic_candidates: Vec<_> = candidates
            .extract_if(.., |sugg| {
                let path = path_names_to_string(&sugg.path);
                path.starts_with("core::intrinsics::") || path.starts_with("core::intrinsics::")
            })
            .collect();
        if candidates.is_empty() {
            // Put them back if we have no more candidates to suggest...
            candidates = intrinsic_candidates;
        }

        // B.
        let mut enum_candidates = Vec::new();
        let crate_def_id = CRATE_DEF_ID.to_def_id();
        if candidates.is_empty() && is_expected(Res::Def(DefKind::Enum, crate_def_id)) {
            let is_enum_variant = &|res| matches!(res, Res::Def(DefKind::Variant, _));
            enum_candidates = self
                .lookup_import_candidates(ident, ns, &query.parent_scope, is_enum_variant)
                .into_iter()
                .map(|suggestion| import_candidate_to_enum_paths(&suggestion))
                .filter(|(_, enum_ty_path)| !enum_ty_path.starts_with("std::prelude::"))
                .collect();
            enum_candidates.sort();
        }

        // C, once: the old code ran it up to three times on the same inputs.
        let typo = self.typo_candidate_from(
            &query.typo_sources,
            &query.path,
            query.following_seg.as_ref(),
            ns,
            is_expected,
        );

        // D.
        if !query.found && candidates.is_empty() {
            candidates = self.partial_mod_path_candidates(
                &query.parent_scope,
                &query.path,
                query.following_seg.as_ref(),
            );
        }

        LateSearch { candidates, enum_candidates, typo }
    }
}

// ---- after the stage: finishing, in failure order -------------------------------------------

impl<'ra, 'tcx> Resolver<'ra, 'tcx> {
    /// Record one failing path, with what its call site does with the finished error. Waits
    /// for the stage when the view is complete; otherwise finishes in place, as before (see
    /// `frozen.rs`, "When the view is not complete").
    pub(crate) fn record_late_failure(
        &mut self,
        prepared: Prepared<'ra, 'tcx>,
        shell: Shell<'ra, 'tcx>,
        path: &[Segment],
    ) {
        let slot = self.late_failures.injections.len();
        self.late_failures.injections.push(None);
        match prepared {
            Prepared::Planned(planned) if self.frozen_view_is_complete() => {
                if shell.emits_in_place() {
                    if !self.late_failures.has_waiting_emitters() {
                        self.late_failures.errors_before_waiting = self.dcx().err_count();
                    }
                    self.late_failures.waiting_emitters += 1;
                }
                self.late_failures.waiting.push(LateFailure { planned, shell, slot });
            }
            Prepared::Planned(planned) => {
                let answer = self.search_late_failure(&planned.query);
                let (err, candidates, path) = self.finish_late_failure(planned, answer);
                self.settle_late_failure(err, candidates, path, shell, slot);
            }
            Prepared::Done(err, candidates) => {
                // A finished call-path error may stash or emit right now, after everything
                // waiting.
                if let Shell::Call(_) = shell {
                    self.before_emit();
                }
                self.settle_late_failure(err, candidates, path.to_vec(), shell, slot);
            }
        }
    }

    /// Search every waiting failure in one stage, then finish and settle each, in failure
    /// order. Called before any other emission while an in-place emitter waits
    /// (`before_emit`), and once when the walk is over.
    pub(crate) fn flush_late_failures(&mut self) {
        let waiting = take(&mut self.late_failures.waiting);
        if self.late_failures.has_waiting_emitters() {
            // A waiting error that emits in place must print before anything emitted after
            // it was found. An error counted since it started waiting was emitted by a site
            // that did not call `before_emit` first, and printed ahead of it.
            debug_assert_eq!(
                self.dcx().err_count(),
                self.late_failures.errors_before_waiting,
                "an error was emitted during late resolution without `before_emit` while an \
                 unresolved path's error was waiting to be emitted ahead of it",
            );
        }
        self.late_failures.waiting_emitters = 0;
        if waiting.is_empty() {
            return;
        }
        let answers =
            self.run_frozen(&waiting, |r, failure| r.search_late_failure(&failure.planned.query));
        for (LateFailure { planned, shell, slot }, answer) in waiting.into_iter().zip(answers) {
            let (err, candidates, path) = self.finish_late_failure(planned, answer);
            self.settle_late_failure(err, candidates, path, shell, slot);
        }
    }

    /// Every `use` injection of late resolution, in the order the walk found the errors, once
    /// the last waiting ones are settled.
    pub(crate) fn take_late_use_injections(&mut self) -> Vec<UseError<'tcx>> {
        self.flush_late_failures();
        take(&mut self.late_failures.injections).into_iter().flatten().collect()
    }

    /// The rest of `smart_resolve_report_errors` for one failure: every write it made after
    /// the base error, in its order, from the walk's facts and the stage's answers. Returns
    /// the finished error, its import candidates, and the path.
    fn finish_late_failure(
        &self,
        planned: PlannedFailure<'ra, 'tcx>,
        answer: LateSearch,
    ) -> (Diag<'tcx>, Vec<ImportSuggestion>, Vec<Segment>) {
        let PlannedFailure { mut err, query, facts } = planned;
        let FailureQuery { path, following_seg, .. } = query;
        let FailureFacts { span, base_error, found, tail } = facts;
        let LateSearch { mut candidates, enum_candidates, typo } = answer;
        let ident = path.last().unwrap().ident;
        let ident_span = ident.span;
        let path_str = Segment::names_to_string(&path);

        // `try_lookup_name_relaxed`, from the enum candidates on.
        let relaxed = relaxed_span(&path, following_seg.as_ref(), span);
        let mut suggested_candidates = FxHashSet::default();
        if !enum_candidates.is_empty() {
            // Contextualize for E0425 "cannot find type", but don't belabor the point
            // (that it's a variant) for E0573 "expected type, found variant". With no
            // resolution this is always the former.
            let others = match enum_candidates.len() {
                1 => String::new(),
                2 => " and 1 other".to_owned(),
                n => format!(" and {n} others"),
            };
            let preamble =
                format!("there is an enum variant `{}`{}; ", enum_candidates[0].0, others);
            let msg = format!("{preamble}try using the variant's enum");

            suggested_candidates.extend(
                enum_candidates.iter().map(|(_variant_path, enum_ty_path)| enum_ty_path.clone()),
            );
            err.span_suggestions(
                relaxed,
                msg,
                enum_candidates.into_iter().map(|(_variant_path, enum_ty_path)| enum_ty_path),
                Applicability::MachineApplicable,
            );
        }
        let typo_sugg = typo
            .to_opt_suggestion()
            .filter(|sugg| !suggested_candidates.contains(sugg.candidate.as_str()));

        if let Some(found) = found {
            match found {
                FoundHelp::Assoc { candidate, self_is_available, pre, format_named_arg } => {
                    match candidate {
                        AssocSuggestion::Field(field_span) => {
                            if self_is_available {
                                if format_named_arg {
                                    err.help(
                                        format!("you might have meant to use the available field in a format string: `\"{{}}\", self.{}`", ident.name),
                                    );
                                } else {
                                    err.span_suggestion_verbose(
                                        relaxed.shrink_to_lo(),
                                        "you might have meant to use the available field",
                                        format!("{pre}self."),
                                        Applicability::MaybeIncorrect,
                                    );
                                }
                            } else {
                                err.span_label(field_span, "a field by that name exists in `Self`");
                            }
                        }
                        AssocSuggestion::MethodWithSelf { called } if self_is_available => {
                            let msg = if called {
                                "you might have meant to call the method"
                            } else {
                                "you might have meant to refer to the method"
                            };
                            err.span_suggestion_verbose(
                                relaxed.shrink_to_lo(),
                                msg,
                                "self.",
                                Applicability::MachineApplicable,
                            );
                        }
                        AssocSuggestion::MethodWithSelf { .. }
                        | AssocSuggestion::AssocFn { .. }
                        | AssocSuggestion::AssocConst
                        | AssocSuggestion::AssocType => {
                            err.span_suggestion_verbose(
                                relaxed.shrink_to_lo(),
                                format!("you might have meant to {}", candidate.action()),
                                "Self::",
                                Applicability::MachineApplicable,
                            );
                        }
                    }
                    self.add_typo_suggestion(&mut err, typo_sugg, ident_span);
                }
                FoundHelp::CallAsMethod { call_span, message, suggestion } => {
                    err.span_suggestion(
                        call_span,
                        message,
                        suggestion,
                        Applicability::MachineApplicable,
                    );
                }
                FoundHelp::OtherScope(binding_span) => {
                    err.span_help(
                        binding_span,
                        format!("the binding `{path_str}` is available in a different scope in the same function"),
                    );
                }
            }
            return (err, candidates, path);
        }
        let TailFacts { where_restriction, typo: typo_facts, special_cases } =
            tail.expect("a relaxed lookup that went on has its tail planned");

        // `suggest_shadowed`.
        if let TypoCandidate::Shadowed(res, Some(sugg_span)) = typo
            && res.opt_def_id().is_some_and(|id| {
                let source_map = self.tcx.sess.source_map();
                id.is_local() || source_map.span_to_filename(span) == source_map.span_to_filename(sugg_span)
            })
        {
            err.span_label(
                sugg_span,
                format!("you might have meant to refer to this {}", res.descr()),
            );
            // if there is already a shadowed name, don'suggest candidates for importing
            candidates.clear();
        }

        // `suggest_trait_and_bounds`: with no resolution, only the `where` restriction.
        let (mut fallback, where_suggestion) = where_restriction;
        if let Some((where_span, msg, sugg)) = where_suggestion {
            err.span_suggestion_verbose(where_span, msg, sugg, Applicability::MaybeIncorrect);
        }

        // `suggest_typo`.
        fallback |= match typo_facts {
            TypoFacts::AssocFromBounds(suggestions) => {
                err.span_suggestions_with_style(
                    ident_span,
                    "you might have meant to use an associated type of the same name",
                    suggestions,
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::ShowAlways,
                );
                false
            }
            TypoFacts::Typo { assign, let_binding } => {
                let mut typo_fallback = true;
                let typo_sugg = typo
                    .to_opt_suggestion()
                    .filter(|sugg| !suggested_candidates.contains(sugg.candidate.as_str()));
                self.add_typo_suggestion(&mut err, typo_sugg, ident_span);
                if let Some(assign_span) = assign {
                    err.span_suggestion_verbose(
                        assign_span,
                        "use `=` if you meant to assign",
                        " = ",
                        Applicability::MaybeIncorrect,
                    );
                }
                // The single associated item suggestion is for trait items only, which are
                // never recorded: nothing to add.
                if let Some((let_span, msg, text)) = let_binding {
                    err.span_suggestion_verbose(let_span, msg, text, Applicability::MaybeIncorrect);
                    typo_fallback = false;
                }
                typo_fallback
            }
        };

        if fallback {
            // Fallback label.
            err.span_label(base_error.span, base_error.fallback_label);
        }
        super::apply_special_cases(&mut err, span, special_cases);

        let module = base_error.module.unwrap_or_else(|| CRATE_DEF_ID.to_def_id());
        self.find_cfg_stripped(&mut err, &ident.name, module);

        (err, candidates, path)
    }

    /// Do with a finished error what its call site did: see [`Shell`].
    fn settle_late_failure(
        &mut self,
        err: Diag<'tcx>,
        candidates: Vec<ImportSuggestion>,
        path: Vec<Segment>,
        shell: Shell<'ra, 'tcx>,
        slot: usize,
    ) {
        match shell {
            Shell::Report(ReportShell { node_id, instead, suggestion, replacement, is_call }) => {
                let mut err = err;
                if let Some(const_err) = replacement {
                    err.cancel();
                    err = const_err;
                }
                self.late_failures.injections[slot] =
                    Some(UseError { err, candidates, node_id, instead, suggestion, path, is_call });
            }
            Shell::Call(CallShell { parent, node_id, report, stash_at, is_call }) => {
                let err = self.merge_call_error(err, parent);
                if report {
                    if candidates.is_empty() {
                        if let Some(stash_span) = stash_at {
                            // Delay to check whether method name is an associated function or not
                            // ```
                            // let foo = Foo {};
                            // foo::bar(); // possibly suggest to foo.bar();
                            //```
                            err.stash(stash_span, StashKey::CallAssocMethod);
                        } else {
                            // When there is no suggested imports, we can just emit the error
                            // and suggestions immediately. Note that we bypass the usually error
                            // reporting routine (ie via `self.r.report_error`) because we need
                            // to post-process the `ResolutionError` above.
                            err.emit();
                        }
                    } else {
                        // If there are suggested imports, the error reporting is delayed
                        self.late_failures.injections[slot] = Some(UseError {
                            err,
                            candidates,
                            node_id,
                            instead: false,
                            suggestion: None,
                            path,
                            is_call,
                        });
                    }
                } else {
                    err.cancel();
                }
            }
        }
    }

    /// `report_errors_for_call`'s merge: keep the *message* of the whole path's resolution
    /// error (E0433 "failed to resolve"), and the *hints* of the prefix's error.
    fn merge_call_error(
        &self,
        mut err: Diag<'tcx>,
        parent: Spanned<ResolutionError<'ra>>,
    ) -> Diag<'tcx> {
        // There are two different error messages user might receive at
        // this point:
        // - E0425 cannot find type `{}` in this scope
        // - E0433 failed to resolve: use of undeclared type or module `{}`
        //
        // The first one is emitted for paths in type-position, and the
        // latter one - for paths in expression-position.
        //
        // Thus (since we're in expression-position at this point), not to
        // confuse the user, we want to keep the *message* from E0433 (so
        // `parent_err`), but we want *hints* from E0425 (so `err`).
        //
        // And that's what happens below - we're just mixing both messages
        // into a single one.
        let failed_to_resolve = match parent.node {
            ResolutionError::FailedToResolve { .. } => true,
            _ => false,
        };
        let mut parent_err = self.into_struct_error(parent.span, parent.node);

        // overwrite all properties with the parent's error message
        err.messages = take(&mut parent_err.messages);
        err.code = take(&mut parent_err.code);
        swap(&mut err.span, &mut parent_err.span);
        if failed_to_resolve {
            err.children = take(&mut parent_err.children);
        } else {
            err.children.append(&mut parent_err.children);
        }
        err.sort_span = parent_err.sort_span;
        err.is_lint = parent_err.is_lint.clone();

        // merge the parent_err's suggestions with the typo (err's) suggestions
        match &mut err.suggestions {
            Suggestions::Enabled(typo_suggestions) => match &mut parent_err.suggestions {
                Suggestions::Enabled(parent_suggestions) => {
                    // If both suggestions are enabled, append parent_err's suggestions to err's suggestions.
                    typo_suggestions.append(parent_suggestions)
                }
                Suggestions::Sealed(_) | Suggestions::Disabled => {
                    // If the parent's suggestions are either sealed or disabled, it signifies that
                    // new suggestions cannot be added or removed from the diagnostic. Therefore,
                    // we assign both types of suggestions to err's suggestions and discard the
                    // existing suggestions in err.
                    err.suggestions = core::mem::take(&mut parent_err.suggestions);
                }
            },
            Suggestions::Sealed(_) | Suggestions::Disabled => (),
        }

        parent_err.cancel();
        err
    }
}
