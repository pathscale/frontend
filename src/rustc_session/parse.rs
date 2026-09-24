//! Contains `ParseSess` which holds state living beyond what one `Parser` might.
//! It also serves as an input to the parser itself.

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::sync::Arc;

use crate::rustc_ast::attr::AttrIdGenerator;
use crate::rustc_ast::node_id::NodeId;
use crate::rustc_data_structures::fx::{FxHashMap, FxIndexMap};
use crate::rustc_data_structures::sync::{AppendOnlyVec, DynSend, DynSync, Lock};
use crate::rustc_errors::plain_emitter::PlainEmitter;
use crate::rustc_errors::emitter::EmitterWithNote;
use crate::rustc_errors::{
    BufferedEarlyLint, ColorConfig, DecorateDiagCompat, Diag, DiagCtxt, DiagCtxtHandle, Level,
    MultiSpan,
};
use crate::rustc_span::edition::Edition;
use crate::rustc_span::hygiene::ExpnId;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{Span, Symbol};

use crate::rustc_session::Session;
use crate::rustc_session::lint::{Lint, LintId};

/// Collected spans during parsing for places where a certain feature was
/// used and should be feature gated accordingly in `check_crate` in `rustc_ast_passes`.
#[derive(Default)]
pub struct GatedSpans {
    pub spans: Lock<FxHashMap<Symbol, Vec<Span>>>,
}

impl GatedSpans {
    /// Feature gate the given `span` under the given `feature`
    /// which is same `Symbol` used in `unstable.rs`.
    pub fn gate(&self, feature: Symbol, span: Span) {
        self.spans.borrow_mut().entry(feature).or_default().push(span);
    }

    /// Ungate the last span under the given `feature`.
    /// Panics if the given `span` wasn't the last one.
    ///
    /// Using this is discouraged unless you have a really good reason to.
    pub fn ungate_last(&self, feature: Symbol, span: Span) {
        let removed_span = self.spans.borrow_mut().entry(feature).or_default().pop().unwrap();
        debug_assert_eq!(span, removed_span);
    }

    /// Prepend the given set of `spans` onto the set in `self`.
    pub fn merge(&self, mut spans: FxHashMap<Symbol, Vec<Span>>) {
        let mut inner = self.spans.borrow_mut();
        // The entries will be moved to another map so the drain order does not
        // matter.
        for (gate, mut gate_spans) in inner.drain() {
            spans.entry(gate).or_default().append(&mut gate_spans);
        }
        *inner = spans;
    }
}

#[derive(Default)]
pub struct SymbolGallery {
    /// All symbols occurred and their first occurrence span.
    pub symbols: Lock<FxIndexMap<Symbol, Span>>,
}

impl SymbolGallery {
    /// Insert a symbol and its span into symbol gallery.
    /// If the symbol has occurred before, ignore the new occurrence.
    pub fn insert(&self, symbol: Symbol, span: Span) {
        self.symbols.lock().entry(symbol).or_insert(span);
    }
}

/// Info about a parsing session.
pub struct ParseSess {
    dcx: DiagCtxt,
    pub edition: Edition,
    /// Places where raw identifiers were used. This is used to avoid complaining about idents
    /// clashing with keywords in new editions.
    pub raw_identifier_spans: AppendOnlyVec<Span>,
    /// Places where identifiers that contain invalid Unicode codepoints but that look like they
    /// should be. Useful to avoid bad tokenization when encountering emoji. We group them to
    /// provide a single error per unique incorrect identifier.
    pub bad_unicode_identifiers: Lock<FxIndexMap<Symbol, Vec<Span>>>,
    source_map: Arc<SourceMap>,
    pub buffered_lints: Lock<Vec<BufferedEarlyLint>>,
    /// Contains the spans of block expressions that could have been incomplete based on the
    /// operation token that followed it, but that the parser cannot identify without further
    /// analysis.
    pub ambiguous_block_expr_parse: Lock<FxIndexMap<Span, Span>>,
    pub gated_spans: GatedSpans,
    pub symbol_gallery: SymbolGallery,
    /// Used to generate new `AttrId`s. Every `AttrId` is unique.
    pub attr_id_generator: AttrIdGenerator,
}

impl ParseSess {
    /// Used for testing.
    pub fn new() -> Self {
        let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
        let emitter = Box::new(PlainEmitter::new().sm(Some(Arc::clone(&sm))));
        let dcx = DiagCtxt::new(emitter);
        ParseSess::with_dcx(dcx, sm)
    }

    pub fn with_dcx(dcx: DiagCtxt, source_map: Arc<SourceMap>) -> Self {
        Self {
            dcx,
            edition: ExpnId::root().expn_data().edition,
            raw_identifier_spans: Default::default(),
            bad_unicode_identifiers: Lock::new(Default::default()),
            source_map,
            buffered_lints: Lock::new(vec![]),
            ambiguous_block_expr_parse: Lock::new(Default::default()),
            gated_spans: GatedSpans::default(),
            symbol_gallery: SymbolGallery::default(),
            attr_id_generator: AttrIdGenerator::new(),
        }
    }

    /// A session for parsing one part of a file on its own, beside this one: this session's
    /// source map and edition, `dcx` for its diagnostics, its own `AttrId` counter starting at
    /// `first_attr_id`, and empty state everywhere else. Built field by field rather than through
    /// `with_dcx`, whose edition default reads the hygiene tables under their global lock, once
    /// per part, on every worker at once.
    ///
    /// What the part's parse leaves in it is that part's owned output, handed back with
    /// [`ParseSess::absorb_part`] in the order the parts come in the file. The parallel parse of
    /// a file's items (`rustc_parse::parser::item_chunks`) is the user;
    /// `research/parallel-parse.md` lists every field and why its merge reproduces the serial
    /// parse.
    pub fn part(&self, dcx: DiagCtxt, first_attr_id: u32) -> ParseSess {
        ParseSess {
            dcx,
            edition: self.edition,
            raw_identifier_spans: Default::default(),
            bad_unicode_identifiers: Lock::new(Default::default()),
            source_map: Arc::clone(&self.source_map),
            buffered_lints: Lock::new(vec![]),
            ambiguous_block_expr_parse: Lock::new(Default::default()),
            gated_spans: GatedSpans::default(),
            symbol_gallery: SymbolGallery::default(),
            attr_id_generator: AttrIdGenerator::starting_at(first_attr_id),
        }
    }

    /// Append what a part's parse left in `part` to this session, exactly as if this session's
    /// own parse had written it at the point where the part begins. Parts are absorbed in file
    /// order, each after everything this session already holds.
    ///
    /// `part` must have told its `DiagCtxt` nothing: a part whose parse reported anything is
    /// discarded, not absorbed, and the caller parses serially instead. Its `AttrId` counter is
    /// the caller's business (the caller advances this session's counter by the ids the part
    /// took, and corrects the part's ids in the AST if its start was mispredicted), so it is
    /// dropped here.
    pub fn absorb_part(&self, part: ParseSess) {
        // Destructured, so that a field added to `ParseSess` fails to compile here until its
        // merge rule is written down.
        let ParseSess {
            dcx,
            edition: _,
            raw_identifier_spans,
            bad_unicode_identifiers,
            source_map: _,
            buffered_lints,
            ambiguous_block_expr_parse,
            gated_spans,
            symbol_gallery,
            attr_id_generator: _,
        } = part;
        debug_assert!(dcx.handle().has_errors_or_delayed_bugs().is_none());
        drop(dcx);

        // Appended in order: the serial parse pushes each span as it reads it.
        for span in raw_identifier_spans.iter() {
            self.raw_identifier_spans.push(span);
        }
        // Keyed by first occurrence: an entry this session already has keeps its place and its
        // earlier spans, and the part's spans follow them.
        {
            let mut mine = self.bad_unicode_identifiers.lock();
            for (symbol, spans) in bad_unicode_identifiers.into_inner() {
                mine.entry(symbol).or_default().extend(spans);
            }
        }
        // Appended in order.
        self.buffered_lints.lock().extend(buffered_lints.into_inner());
        // Insertion order is the map's order; `insert` on a key already present replaces its
        // value in place, as the serial parse's `insert` would have.
        {
            let mut mine = self.ambiguous_block_expr_parse.lock();
            for (block, lhs) in ambiguous_block_expr_parse.into_inner() {
                mine.insert(block, lhs);
            }
        }
        // Per feature, in order. The map is only ever looked up by feature, so the order its
        // keys were first inserted in is not observable.
        {
            let mut mine = self.gated_spans.spans.lock();
            for (feature, spans) in gated_spans.spans.into_inner() {
                mine.entry(feature).or_default().extend(spans);
            }
        }
        // First occurrence wins, as in `SymbolGallery::insert`.
        {
            let mut mine = self.symbol_gallery.symbols.lock();
            for (symbol, span) in symbol_gallery.symbols.into_inner() {
                mine.entry(symbol).or_insert(span);
            }
        }
    }

    pub fn emitter_with_note(note: String) -> Self {
        let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
        let emitter = Box::new(PlainEmitter::new());
        let dcx = DiagCtxt::new(Box::new(EmitterWithNote { emitter, note }));
        ParseSess::with_dcx(dcx, sm)
    }

    #[inline]
    pub fn source_map(&self) -> &SourceMap {
        &self.source_map
    }

    pub fn clone_source_map(&self) -> Arc<SourceMap> {
        Arc::clone(&self.source_map)
    }

    pub fn buffer_lint(
        &self,
        lint: &'static Lint,
        span: impl Into<MultiSpan>,
        node_id: NodeId,
        diagnostic: impl Into<DecorateDiagCompat>,
    ) {
        self.opt_span_buffer_lint(lint, Some(span.into()), node_id, diagnostic.into())
    }

    pub fn dyn_buffer_lint<
        F: for<'a> FnOnce(DiagCtxtHandle<'a>, Level) -> Diag<'a, ()> + DynSync + DynSend + 'static,
    >(
        &self,
        lint: &'static Lint,
        span: impl Into<MultiSpan>,
        node_id: NodeId,
        callback: F,
    ) {
        self.opt_span_buffer_lint(
            lint,
            Some(span.into()),
            node_id,
            DecorateDiagCompat(Box::new(|dcx, level, _| callback(dcx, level))),
        )
    }

    pub fn dyn_buffer_lint_sess<
        F: for<'a> FnOnce(DiagCtxtHandle<'a>, Level, &Session) -> Diag<'a, ()>
            + DynSync
            + DynSend
            + 'static,
    >(
        &self,
        lint: &'static Lint,
        span: impl Into<MultiSpan>,
        node_id: NodeId,
        callback: F,
    ) {
        self.opt_span_buffer_lint(
            lint,
            Some(span.into()),
            node_id,
            DecorateDiagCompat(Box::new(|dcx, level, sess| {
                let sess = sess.downcast_ref::<Session>().expect("expected a `Session`");
                callback(dcx, level, sess)
            })),
        )
    }

    pub(crate) fn opt_span_buffer_lint(
        &self,
        lint: &'static Lint,
        span: Option<MultiSpan>,
        node_id: NodeId,
        diagnostic: DecorateDiagCompat,
    ) {
        self.buffered_lints.with_lock(|buffered_lints| {
            buffered_lints.push(BufferedEarlyLint {
                span,
                node_id,
                lint_id: LintId::of(lint),
                diagnostic,
            });
        });
    }

    pub fn dcx(&self) -> DiagCtxtHandle<'_> {
        self.dcx.handle()
    }

    /// Replace the emitter, before any diagnostic has been emitted.
    ///
    /// This is what `interface::Config::psess_created` exists for from a server's point of view:
    /// a diagnostic is a `DiagInner` until an emitter renders it, and a server wants the record
    /// rather than the rendering. See
    // `+ DynSend` dropped: no longer an auto trait (see `rustc_data_structures/marker.rs`).
    pub fn set_emitter(&self, emitter: Box<dyn crate::rustc_errors::emitter::Emitter>) {
        self.dcx.set_emitter(emitter);
    }
}
