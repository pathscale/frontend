//! Attest the parser-visible source bytes for a compiler-selected callable definition.
//!
//! This module deliberately reads source through rustc's `SourceMap`. For imported files,
//! `span_to_source` first loads the external source only when it can verify the bytes against
//! the source hash carried by compiler metadata. A metadata hash is never returned as the
//! attestation: both hashes below are computed from the actual source text supplied to the
//! closure.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::visit::{self, AssocCtxt, Visitor};
use crate::rustc_ast::{AssocItem, AssocItemKind, Impl, Item, ItemKind, Variant};
use crate::rustc_errors::DiagCtxt;
use crate::rustc_errors::emitter::SilentEmitter;
use crate::rustc_hir::def::DefKind;
use crate::rustc_hir::def_id::DefId;
use crate::rustc_middle::ty::{AssocContainer, TyCtxt};
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::new_parser_from_source_str;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{
    BytePos, FileName, SourceFileHash, SourceFileHashAlgorithm, Span, SpanSnippetError,
};
use serde::{Deserialize, Serialize};

use super::effects::StaticFact;
use super::{ByteSpan, StableDefPathIdentity};

/// Source evidence for one compiler-selected callable definition.
///
/// `definition` must equal the resolved callable's `StableDefPathIdentity`. The file name in
/// `item_span` is only a source locator, not a semantic identity. Both the whole-file digest
/// and item digest cover rustc's normalized UTF-8 source text, after BOM removal and CRLF
/// normalization. `item_span` uses offsets in that normalized text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CallableDefinitionSourceAttestation {
    pub definition: StableDefPathIdentity,
    /// Versioned digest definition, fixed to `blake3=` followed by lowercase hex.
    pub source_digest_scheme: String,
    pub source_content_digest: String,
    /// The compiler's definition span, relative to the normalized source file.
    pub item_span: ByteSpan,
    /// BLAKE3 of exactly the bytes selected by `item_span`.
    pub item_digest: String,
    /// Compiler and target context. Absent in legacy wire data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context: Option<ResolvedCallableSourceContext>,
}

/// Exact compiler and crate context that produced a source-derived identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedCallableSourceContext {
    pub compiler_cfg_version: String,
    pub target_triple: String,
    pub crate_name: String,
    pub stable_crate_id: String,
    pub crate_metadata_hash: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionSourceKind {
    Callable,
    Macro,
    Trait,
    TraitImplementation,
    AssociatedType,
    TraitMethod,
    ImplementationMethod,
    NominalType,
}

/// Verifiable source identity for a callable, trait-related, or macro definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DefinitionSourceAttestation {
    pub definition: StableDefPathIdentity,
    pub definition_kind: DefinitionSourceKind,
    pub source_context: ResolvedCallableSourceContext,
    pub source_digest_scheme: String,
    pub source_content_digest: String,
    pub item_span: ByteSpan,
    pub item_digest: String,
}

/// Hash a selected definition's exact source file and item, or report why the source evidence
/// is unavailable. This function does not infer source from a path, name, or metadata hash.
pub(super) fn attest_callable_definition(
    tcx: TyCtxt<'_>,
    def_id: DefId,
) -> StaticFact<CallableDefinitionSourceAttestation> {
    match attest_definition(tcx, def_id, DefinitionSourceKind::Callable) {
        StaticFact::Unknown { why } => StaticFact::Unknown { why },
        StaticFact::Known { value } => StaticFact::Known {
            value: CallableDefinitionSourceAttestation {
                definition: value.definition,
                source_digest_scheme: value.source_digest_scheme,
                source_content_digest: value.source_content_digest,
                item_span: value.item_span,
                item_digest: value.item_digest,
                source_context: Some(value.source_context),
            },
        },
    }
}

/// Attest a compiler-selected generic definition through rustc's metadata-verified SourceMap.
/// The caller chooses a semantic source kind; this function validates its DefKind and does not
/// inspect or special-case the definition's name.
pub(super) fn attest_definition(
    tcx: TyCtxt<'_>,
    def_id: DefId,
    definition_kind: DefinitionSourceKind,
) -> StaticFact<DefinitionSourceAttestation> {
    let kind_matches = match definition_kind {
        DefinitionSourceKind::Callable => {
            matches!(tcx.def_kind(def_id), DefKind::Fn | DefKind::AssocFn | DefKind::Ctor(..))
        }
        DefinitionSourceKind::Macro => matches!(tcx.def_kind(def_id), DefKind::Macro(_)),
        DefinitionSourceKind::Trait => matches!(tcx.def_kind(def_id), DefKind::Trait),
        DefinitionSourceKind::TraitImplementation => {
            matches!(tcx.def_kind(def_id), DefKind::Impl { of_trait: true })
        }
        DefinitionSourceKind::AssociatedType => matches!(tcx.def_kind(def_id), DefKind::AssocTy),
        DefinitionSourceKind::TraitMethod => tcx
            .opt_associated_item(def_id)
            .is_some_and(|item| matches!(item.container, AssocContainer::Trait)),
        DefinitionSourceKind::ImplementationMethod => tcx
            .opt_associated_item(def_id)
            .is_some_and(|item| matches!(item.container, AssocContainer::TraitImpl(Ok(_)))),
        DefinitionSourceKind::NominalType => {
            matches!(tcx.def_kind(def_id), DefKind::Struct | DefKind::Enum | DefKind::Union)
        }
    };
    if !kind_matches {
        return unknown("definition_kind_mismatch");
    }
    let compiler_anchor = tcx.def_span(def_id);
    let span = if def_id.is_local() {
        if matches!(definition_kind, DefinitionSourceKind::Macro) {
            return unknown("local_macro_definition_source_not_attested");
        }
        tcx.hir_span_with_body(tcx.local_def_id_to_hir_id(def_id.expect_local()))
    } else {
        compiler_anchor
    };
    if span.is_dummy() {
        return unknown("definition_span_dummy");
    }
    if span.from_expansion() {
        return unknown("definition_from_expansion");
    }
    if span.lo() >= span.hi() {
        return unknown("definition_span_empty");
    }

    let source_map = tcx.sess.source_map();
    let (start_file, _) = source_map.lookup_byte_offset_in(span.lo());
    let (end_file, _) = source_map.lookup_byte_offset_in(span.hi());
    if !core::ptr::eq(start_file, end_file) {
        return unknown("definition_span_crosses_files");
    }

    // `span_to_source` may load external source. SourceMap verifies imported source against
    // the metadata hash before making its normalized bytes available here.
    let digests = source_map.span_to_source(span, |source, start_index, end_index| {
        let (item_start, item_end) = if def_id.is_local() {
            (start_index, end_index)
        } else {
            let file_base = start_file.start_pos;
            let (item_start, item_end) = external_item_range(
                tcx,
                def_id,
                definition_kind,
                source,
                file_base,
                start_file.end_position(),
            )
            .ok_or(SpanSnippetError::IllFormedSpan(span))?;
            (item_start, item_end)
        };
        let item = source.get(item_start..item_end).ok_or(SpanSnippetError::IllFormedSpan(span))?;
        let whole_file =
            SourceFileHash::new_in_memory(SourceFileHashAlgorithm::Blake3, source.as_bytes())
                .to_string();
        let item = SourceFileHash::new_in_memory(SourceFileHashAlgorithm::Blake3, item.as_bytes())
            .to_string();
        Ok((whole_file, item, item_start as u32, item_end as u32))
    });

    match digests {
        Ok((source_content_digest, item_digest, item_start, item_end)) => StaticFact::Known {
            value: DefinitionSourceAttestation {
                definition: super::stable_def_path_identity(tcx, def_id),
                definition_kind,
                source_context: source_context(tcx, def_id),
                source_digest_scheme: "rustc_normalized_source_blake3_v1".to_string(),
                source_content_digest,
                item_span: ByteSpan {
                    file: super::file_name(start_file),
                    start: item_start,
                    end: item_end,
                },
                item_digest,
            },
        },
        Err(SpanSnippetError::SourceNotAvailable { .. }) => {
            unknown("definition_source_unavailable_or_unverified")
        }
        Err(SpanSnippetError::DistinctSources(_)) => unknown("definition_span_crosses_files"),
        Err(SpanSnippetError::IllFormedSpan(_) | SpanSnippetError::MalformedForSourcemap(_)) => {
            unknown("definition_span_invalid")
        }
    }
}

pub(super) fn source_context(tcx: TyCtxt<'_>, def_id: DefId) -> ResolvedCallableSourceContext {
    ResolvedCallableSourceContext {
        compiler_cfg_version: tcx.sess.cfg_version.to_string(),
        target_triple: tcx.sess.opts.target_triple.tuple().to_string(),
        crate_name: tcx.crate_name(def_id.krate).to_string(),
        stable_crate_id: format!("{:016x}", tcx.stable_crate_id(def_id.krate).as_u64()),
        crate_metadata_hash: tcx.crate_hash(def_id.krate).to_hex(),
    }
}

#[derive(Clone, Copy)]
struct SourceRange {
    start: u32,
    end: u32,
}

#[derive(Clone, Copy)]
enum OwnerAnchor {
    Ident(SourceRange),
    ImplHeader(SourceRange),
}

/// Select one external definition only when its parser-visible AST is anchored by the exact
/// compiler identifier span and, for associated items, its exact parent anchor.
fn external_item_range(
    tcx: TyCtxt<'_>,
    def_id: DefId,
    definition_kind: DefinitionSourceKind,
    source: &str,
    file_base: BytePos,
    file_end: BytePos,
) -> Option<(usize, usize)> {
    let own_anchor_span = tcx.def_span(def_id);
    if own_anchor_span.is_dummy()
        || own_anchor_span.from_expansion()
        || own_anchor_span.lo() >= own_anchor_span.hi()
    {
        return None;
    }
    let own_anchor = source_range(own_anchor_span, file_base, file_end)?;
    let own_ident = if matches!(definition_kind, DefinitionSourceKind::TraitImplementation) {
        None
    } else {
        let ident_span = tcx.def_ident_span(def_id)?;
        if ident_span.is_dummy()
            || ident_span.from_expansion()
            || ident_span.lo() >= ident_span.hi()
        {
            return None;
        }
        Some(source_range(ident_span, file_base, file_end)?)
    };
    let associated = tcx.opt_associated_item(def_id).is_some();
    let parent = if associated {
        let parent = tcx.parent(def_id);
        let parent_kind = tcx.def_kind(parent);
        if !matches!(parent_kind, DefKind::Trait | DefKind::Impl { .. }) {
            return None;
        }
        if matches!(definition_kind, DefinitionSourceKind::TraitMethod)
            && !matches!(parent_kind, DefKind::Trait)
        {
            return None;
        }
        if matches!(definition_kind, DefinitionSourceKind::ImplementationMethod)
            && !matches!(parent_kind, DefKind::Impl { .. })
        {
            return None;
        }
        let parent_span = if matches!(parent_kind, DefKind::Impl { .. }) {
            tcx.def_span(parent)
        } else {
            tcx.def_ident_span(parent)?
        };
        if parent_span.is_dummy()
            || parent_span.from_expansion()
            || parent_span.lo() >= parent_span.hi()
        {
            return None;
        }
        let range = source_range(parent_span, file_base, file_end)?;
        Some(if matches!(parent_kind, DefKind::Impl { .. }) {
            OwnerAnchor::ImplHeader(range)
        } else {
            OwnerAnchor::Ident(range)
        })
    } else {
        None
    };
    let expected_name = if own_ident.is_some() {
        Some(tcx.opt_item_name(def_id)?.as_str().to_string())
    } else {
        None
    };

    // Source-token keyword classification reads the root edition from session globals.
    // Keep the parsed AST and its symbols inside this temporary source-only session.
    let edition = tcx.cstore_untracked().crate_edition(def_id.krate);
    crate::rustc_span::with_source_edition_globals(edition, || {
        let expected_name = expected_name
            .as_deref()
            .map(crate::rustc_span::Symbol::intern);
        let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
        let mut psess =
            ParseSess::with_dcx(DiagCtxt::new(Box::new(SilentEmitter)), Arc::clone(&sm));
        psess.edition = edition;
        let krate = match crate::rustc_span::fatal_error::catch_fatal_errors(|| {
            let mut parser = match new_parser_from_source_str(
                &psess,
                FileName::anon_source_code(source),
                source.to_string(),
                StripTokens::ShebangAndFrontmatter,
            ) {
                Ok(parser) => parser,
                Err(diags) => {
                    for diag in diags {
                        diag.cancel();
                    }
                    return None;
                }
            };
            match parser.parse_crate_mod() {
                Ok(krate) => Some(krate),
                Err(diag) => {
                    diag.cancel();
                    None
                }
            }
        }) {
            Ok(Some(krate)) => krate,
            _ => return None,
        };
        if psess.dcx().err_count() != 0 {
            return None;
        }
        let files = sm.files();
        let parsed_file = files.first()?;
        let mut finder = DefinitionFinder {
            kind: definition_kind,
            name: expected_name,
            own_ident,
            own_anchor,
            owner: parent,
            owner_active: parent.is_none(),
            owner_matches: 0,
            candidates: Vec::new(),
            file_base: parsed_file.start_pos,
        };
        visit::walk_crate(&mut finder, &krate);
        if psess.dcx().err_count() != 0 || finder.owner_matches != usize::from(parent.is_some()) {
            return None;
        }
        if finder.candidates.len() != 1 {
            return None;
        }
        finder.candidates.first().copied()
    })
}

fn source_range(span: Span, file_base: BytePos, file_end: BytePos) -> Option<SourceRange> {
    if span.lo() < file_base || span.hi() > file_end {
        return None;
    }
    Some(SourceRange {
        start: span.lo().0.checked_sub(file_base.0)?,
        end: span.hi().0.checked_sub(file_base.0)?,
    })
}

struct DefinitionFinder {
    kind: DefinitionSourceKind,
    name: Option<crate::rustc_span::Symbol>,
    own_ident: Option<SourceRange>,
    own_anchor: SourceRange,
    owner: Option<OwnerAnchor>,
    owner_active: bool,
    owner_matches: usize,
    candidates: Vec<(usize, usize)>,
    file_base: BytePos,
}

impl DefinitionFinder {
    fn anchor(&self, span: Span, expected: SourceRange) -> bool {
        source_range(span, self.file_base, BytePos(self.file_base.0.saturating_add(u32::MAX)))
            .is_some_and(|actual| actual.start == expected.start && actual.end == expected.end)
    }

    fn item_matches(&mut self, ident: Option<crate::rustc_span::Ident>, span: Span) {
        if !self.owner_active {
            return;
        }
        let ident_matches = match (self.own_ident, ident) {
            (Some(expected), Some(ident)) => {
                self.name == Some(ident.name) && self.anchor(ident.span, expected)
            }
            (None, None) => true,
            _ => false,
        };
        if !span_contains(span, self.own_anchor, self.file_base) || !ident_matches {
            return;
        }
        let Some(range) =
            source_range(span, self.file_base, BytePos(self.file_base.0.saturating_add(u32::MAX)))
        else {
            return;
        };
        self.candidates.push((range.start as usize, range.end as usize));
    }

    fn parent_item_matches(&self, item: &Item) -> bool {
        match (self.owner, &item.kind) {
            (Some(OwnerAnchor::Ident(expected)), ItemKind::Trait(trait_)) => {
                self.anchor(trait_.ident.span, expected)
            }
            (Some(OwnerAnchor::ImplHeader(expected)), ItemKind::Impl(imp)) => {
                self.anchor(ast_impl_header_span(item, imp), expected)
            }
            _ => false,
        }
    }

    fn push_item(&mut self, item: &Item) {
        let ident = match &item.kind {
            ItemKind::Fn(fun) => Some(fun.ident),
            ItemKind::Struct(ident, ..)
            | ItemKind::Enum(ident, ..)
            | ItemKind::Union(ident, ..) => Some(*ident),
            ItemKind::Trait(trait_) => Some(trait_.ident),
            ItemKind::MacroDef(ident, _) => Some(*ident),
            _ => None,
        };
        let kind_matches = match self.kind {
            DefinitionSourceKind::Callable => {
                ident.is_some() && !matches!(item.kind, ItemKind::MacroDef(..))
            }
            DefinitionSourceKind::Macro => matches!(item.kind, ItemKind::MacroDef(..)),
            DefinitionSourceKind::Trait => matches!(item.kind, ItemKind::Trait(_)),
            DefinitionSourceKind::TraitImplementation => {
                matches!(&item.kind, ItemKind::Impl(imp) if imp.of_trait.is_some())
            }
            DefinitionSourceKind::NominalType => {
                matches!(item.kind, ItemKind::Struct(..) | ItemKind::Enum(..) | ItemKind::Union(..))
            }
            DefinitionSourceKind::AssociatedType
            | DefinitionSourceKind::TraitMethod
            | DefinitionSourceKind::ImplementationMethod => false,
        };
        if kind_matches {
            if matches!(self.kind, DefinitionSourceKind::TraitImplementation) {
                if let ItemKind::Impl(imp) = &item.kind {
                    if self.anchor(ast_impl_header_span(item, imp), self.own_anchor) {
                        self.item_matches(None, item.span);
                    }
                }
            } else {
                self.item_matches(ident, item.span);
            }
        }
    }
}

fn ast_impl_header_span(item: &Item, imp: &Impl) -> Span {
    let where_clause_span = imp.generics.where_clause.span;
    if where_clause_span.is_dummy() {
        return item.span;
    }
    where_clause_span
        .find_ancestor_inside(item.span)
        .map_or(item.span, |where_span| item.span.with_hi(where_span.hi()))
}

fn span_contains(span: Span, expected: SourceRange, file_base: BytePos) -> bool {
    let Some(actual) = source_range(span, file_base, BytePos(file_base.0.saturating_add(u32::MAX)))
    else {
        return false;
    };
    actual.start <= expected.start && expected.end <= actual.end
}

impl<'ast> Visitor<'ast> for DefinitionFinder {
    type Result = ();

    fn visit_item(&mut self, item: &'ast Item) {
        let previous = self.owner_active;
        if self.owner.is_some() && self.parent_item_matches(item) {
            self.owner_matches += 1;
            self.owner_active = true;
        } else if self.owner.is_some() {
            self.owner_active = false;
        }
        self.push_item(item);
        visit::walk_item(self, item);
        self.owner_active = previous;
    }

    fn visit_assoc_item(&mut self, item: &'ast AssocItem, ctxt: AssocCtxt) {
        let kind_matches = match (self.kind, &item.kind) {
            (DefinitionSourceKind::Callable, AssocItemKind::Fn(_))
            | (DefinitionSourceKind::TraitMethod, AssocItemKind::Fn(_))
            | (DefinitionSourceKind::ImplementationMethod, AssocItemKind::Fn(_)) => true,
            (DefinitionSourceKind::AssociatedType, AssocItemKind::Type(_)) => true,
            _ => false,
        };
        if kind_matches {
            self.item_matches(item.kind.ident(), item.span);
        }
        visit::walk_assoc_item(self, item, ctxt);
    }

    fn visit_variant(&mut self, variant: &'ast Variant) {
        if matches!(self.kind, DefinitionSourceKind::Callable) {
            self.item_matches(Some(variant.ident), variant.span);
        }
        visit::walk_variant(self, variant);
    }
}

fn unknown<T>(why: &str) -> StaticFact<T> {
    StaticFact::Unknown { why: why.to_string() }
}
