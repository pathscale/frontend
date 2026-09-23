//! Diagnostics creation and emission for `rustc`.
//!
//! This module contains the code for creating and emitting diagnostics.

// tidy-alphabetical-start
// tidy-alphabetical-end

// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.

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
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

extern crate self as rustc_errors;

pub use alloc::borrow::Cow;
use core::cell::Cell;
use core::hash::Hash;
use core::num::NonZero;
use core::ops::DerefMut;
use eko::path::{Path, PathBuf};
use eko::thread::ThreadId;
use core::fmt::Write as _;
use core::{fmt, mem};
use crate::assert_matches;

use Level::*;
// The `anstyle` re-export went with the colouring. It was here for external consumers such as
// `rust-gpu` (rust-lang/rust#115393), which this fork does not serve; nothing in the tree
// referenced it once the snippet renderer was removed.
pub use codes::*;
pub use decorate_diag::{BufferedEarlyLint, DecorateDiagCompat, LintBuffer};
// Re-exported for `#[derive(Diagnostic)]`. The expansion lands in whichever crate wrote the
// attribute, and that crate can name `alloc::borrow::Cow` only if it declared `extern crate
// alloc`, or `alloc::borrow::Cow` only if it links std - neither holds for every crate that
// derives. Naming it through `rustc_errors` costs the deriving crate nothing.
pub use diagnostic::{
    BugAbort, Diag, DiagDecorator, DiagInner, DiagLocation, DiagStyledString, Diagnostic,
    EmissionGuarantee, FatalAbort, StringPart, Subdiag, Subdiagnostic,
};
pub use diagnostic_impls::{
    DiagSymbolList, ElidedLifetimeInPathSubdiag, ExpectedLifetimeParameter,
    IndicateAnonymousLifetime, SingleLabelManySpans,
};
pub use emitter::ColorConfig;
use emitter::{DynEmitter, Emitter};
use crate::rustc_ast::attr::version::RustcVersion;
use crate::rustc_data_structures::AtomicRef;
use crate::rustc_data_structures::fx::{FxHashSet, FxIndexMap, FxIndexSet};
use crate::rustc_data_structures::stable_hash::StableHasher;
use crate::rustc_data_structures::sync::Lock;
pub use crate::rustc_error_messages::{
    DiagArg, DiagArgFromDisplay, DiagArgMap, DiagArgName, DiagArgValue, DiagMessage, IntoDiagArg,
    LongTyPath, MultiSpan, SpanLabel, into_diag_arg_using_display,
};
use crate::rustc_hashes::Hash128;
use crate::rustc_lint_defs::LintExpectationId;
pub use crate::rustc_lint_defs::{Applicability, listify, pluralize};
pub use rustc_macros::msg;
use rustc_macros::{Decodable, Encodable};
pub use crate::rustc_span::ErrorGuaranteed;
pub use crate::rustc_span::fatal_error::{FatalError, FatalErrorMarker, catch_fatal_errors};
use crate::rustc_span::source_map::SourceMap;
use crate::rustc_span::{DUMMY_SP, Span};
use tracing::debug;

use crate::rustc_errors::emitter::TimingEvent;
use crate::rustc_errors::formatting::DiagMessageAddArg;
pub use crate::rustc_errors::formatting::{diag_message_source, format_diag_message, try_format_diag_message};
use crate::rustc_errors::timings::TimingRecord;

// The snippet emitter is gone. This compiler is built to be driven by a program rather than read
// under source lines in colour had no consumer, and it was what pulled in `annotate-snippets`,
// `anstream`, `anstyle` and `termize`. `plain_emitter` is what remains.
pub mod plain_emitter;
pub mod codes;
mod decorate_diag;
mod diagnostic;
mod diagnostic_impls;
pub mod emitter;
pub mod formatting;
pub mod json;
mod lock;
// A par item's diagnostics as its owned output, and a query's as its own: collected while they
// run, forwarded with their results, replayed in serial order. See its module docs for the
// whole design.
mod item_scope;
pub use item_scope::{
    ItemDiagnostics, ItemRun, OrderedReplay, QueryDiagnostics, QueryFrame,
    consume_query_diagnostics,
};
use item_scope::{DcxRef, EventKind, StashOp, StashReplay};
// `markdown` rendered the long error explanations for `--explain`, a command-line feature this
// compiler does not have - the note further down already records that its 518 markdown files
// went. The renderer outlived them by an oversight.
pub mod timings;

pub type PResult<'a, T> = Result<T, Diag<'a>>;

// `PResult` is used a lot. Make sure it doesn't unintentionally get bigger.
#[cfg(target_pointer_width = "64")]
crate::static_assert_size!(PResult<'_, ()>, 24);
#[cfg(target_pointer_width = "64")]
crate::static_assert_size!(PResult<'_, bool>, 24);

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, Encodable, Decodable)]
pub enum SuggestionStyle {
    /// Hide the suggested code when displaying this suggestion inline.
    HideCodeInline,
    /// Always hide the suggested code but display the message.
    HideCodeAlways,
    /// Do not display this suggestion in the cli output, it is only meant for tools.
    CompletelyHidden,
    /// Always show the suggested code.
    /// This will *not* show the code if the suggestion is inline *and* the suggested code is
    /// empty.
    ShowCode,
    /// Always show the suggested code independently.
    ShowAlways,
}

impl SuggestionStyle {
    fn hide_inline(&self) -> bool {
        !matches!(*self, SuggestionStyle::ShowCode)
    }
}

/// Represents the help messages seen on a diagnostic.
#[derive(Clone, Debug, PartialEq, Hash, Encodable, Decodable)]
pub enum Suggestions {
    /// Indicates that new suggestions can be added or removed from this diagnostic.
    ///
    /// `DiagInner`'s new_* methods initialize the `suggestions` field with
    /// this variant. Also, this is the default variant for `Suggestions`.
    Enabled(Vec<CodeSuggestion>),
    /// Indicates that suggestions cannot be added or removed from this diagnostic.
    ///
    /// Gets toggled when `.seal_suggestions()` is called on the `DiagInner`.
    Sealed(Box<[CodeSuggestion]>),
    /// Indicates that no suggestion is available for this diagnostic.
    ///
    /// Gets toggled when `.disable_suggestions()` is called on the `DiagInner`.
    Disabled,
}

impl Suggestions {
    /// Returns the underlying list of suggestions.
    pub fn unwrap_tag(self) -> Vec<CodeSuggestion> {
        match self {
            Suggestions::Enabled(suggestions) => suggestions,
            Suggestions::Sealed(suggestions) => suggestions.into_vec(),
            Suggestions::Disabled => Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Suggestions::Enabled(suggestions) => suggestions.len(),
            Suggestions::Sealed(suggestions) => suggestions.len(),
            Suggestions::Disabled => 0,
        }
    }
}

impl Default for Suggestions {
    fn default() -> Self {
        Self::Enabled(vec![])
    }
}

#[derive(Clone, Debug, PartialEq, Hash, Encodable, Decodable)]
pub struct CodeSuggestion {
    /// Each substitute can have multiple variants due to multiple
    /// applicable suggestions
    ///
    /// `foo.bar` might be replaced with `a.b` or `x.y` by replacing
    /// `foo` and `bar` on their own:
    ///
    /// ```ignore (illustrative)
    /// vec![
    ///     Substitution { parts: vec![(0..3, "a"), (4..7, "b")] },
    ///     Substitution { parts: vec![(0..3, "x"), (4..7, "y")] },
    /// ]
    /// ```
    ///
    /// or by replacing the entire span:
    ///
    /// ```ignore (illustrative)
    /// vec![
    ///     Substitution { parts: vec![(0..7, "a.b")] },
    ///     Substitution { parts: vec![(0..7, "x.y")] },
    /// ]
    /// ```
    pub substitutions: Vec<Substitution>,
    pub msg: DiagMessage,
    /// Visual representation of this suggestion.
    pub style: SuggestionStyle,
    /// Whether or not the suggestion is approximate
    ///
    /// Sometimes we may show suggestions with placeholders,
    /// which are useful for users but not useful for
    /// tools like rustfix
    pub applicability: Applicability,
}

#[derive(Clone, Debug, PartialEq, Hash, Encodable, Decodable)]
/// See the docs on `CodeSuggestion::substitutions`
pub struct Substitution {
    pub parts: Vec<SubstitutionPart>,
}

#[derive(Clone, Debug, PartialEq, Hash, Encodable, Decodable)]
pub struct SubstitutionPart {
    pub span: Span,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq, Hash, Encodable, Decodable)]
pub struct TrimmedSubstitutionPart {
    pub original_span: Span,
    pub span: Span,
    pub snippet: String,
}

impl TrimmedSubstitutionPart {
    pub fn is_addition(&self, sm: &SourceMap) -> bool {
        !self.snippet.is_empty() && !self.replaces_meaningful_content(sm)
    }

    pub fn is_deletion(&self, sm: &SourceMap) -> bool {
        self.snippet.trim().is_empty() && self.replaces_meaningful_content(sm)
    }

    pub fn is_replacement(&self, sm: &SourceMap) -> bool {
        !self.snippet.is_empty() && self.replaces_meaningful_content(sm)
    }

    /// Whether this is a replacement that overwrites source with a snippet
    /// in a way that isn't a superset of the original string. For example,
    /// replacing "abc" with "abcde" is not destructive, but replacing it
    /// it with "abx" is, since the "c" character is lost.
    pub fn is_destructive_replacement(&self, sm: &SourceMap) -> bool {
        self.is_replacement(sm)
            && !sm
                .span_to_snippet(self.span)
                .is_ok_and(|snippet| as_substr(snippet.trim(), self.snippet.trim()).is_some())
    }

    fn replaces_meaningful_content(&self, sm: &SourceMap) -> bool {
        sm.span_to_snippet(self.span)
            .map_or(!self.span.is_empty(), |snippet| !snippet.trim().is_empty())
    }
}

/// Given an original string like `AACC`, and a suggestion like `AABBCC`, try to detect
/// the case where a substring of the suggestion is "sandwiched" in the original, like
/// `BB` is. Return the length of the prefix, the "trimmed" suggestion, and the length
/// of the suffix.
fn as_substr<'a>(original: &'a str, suggestion: &'a str) -> Option<(usize, &'a str, usize)> {
    let common_prefix = original
        .chars()
        .zip(suggestion.chars())
        .take_while(|(c1, c2)| c1 == c2)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let original = &original[common_prefix..];
    let suggestion = &suggestion[common_prefix..];
    if suggestion.ends_with(original) {
        let common_suffix = original.len();
        Some((common_prefix, &suggestion[..suggestion.len() - original.len()], common_suffix))
    } else {
        None
    }
}

/// Signifies that the compiler died with an explicit call to `.bug`
/// or `.span_bug` rather than a failed assertion, etc.
pub struct ExplicitBug;

/// Signifies that the compiler died due to a delayed bug rather than a failed
/// assertion, etc.
pub struct DelayedBugPanic;

/// A `DiagCtxt` deals with errors and other compiler output.
/// Certain errors (fatal, bug, unimpl) may cause immediate exit,
/// others log errors for later reporting.
pub struct DiagCtxt {
    inner: Lock<DiagCtxtInner>,
}

#[derive(Copy, Clone)]
pub struct DiagCtxtHandle<'a> {
    dcx: &'a DiagCtxt,
    /// Some contexts create `DiagCtxtHandle` with this field set, and thus all
    /// errors emitted with it will automatically taint when emitting errors.
    tainted_with_errors: Option<&'a Cell<Option<ErrorGuaranteed>>>,
}

impl<'a> core::ops::Deref for DiagCtxtHandle<'a> {
    type Target = &'a DiagCtxt;

    fn deref(&self) -> &Self::Target {
        &self.dcx
    }
}

/// This inner struct exists to keep it all behind a single lock;
/// this is done to prevent possible deadlocks in a multi-threaded compiler,
/// as well as inconsistent state observation.
struct DiagCtxtInner {
    flags: DiagCtxtFlags,

    /// The error guarantees from all emitted errors, each paired with the
    /// thread that emitted it. The length gives the error count.
    err_guars: Vec<(ErrorGuaranteed, ThreadId)>,
    /// The error guarantee from all emitted lint errors, each paired with the
    /// thread that emitted it. The length gives the lint error count.
    lint_err_guars: Vec<(ErrorGuaranteed, ThreadId)>,
    /// The delayed bugs and their error guarantees. One recorded inside a par item is carried
    /// in the item's `ItemDiagnostics` and lands here at replay, so this is in serial order.
    delayed_bugs: Vec<(DelayedDiagInner, ErrorGuaranteed)>,

    /// The error count shown to the user at the end.
    deduplicated_err_count: usize,
    /// The warning count shown to the user at the end.
    deduplicated_warn_count: usize,

    emitter: Box<DynEmitter>,

    /// Must we produce a diagnostic to justify the use of the expensive
    /// `trimmed_def_paths` function? This used to hold a `Backtrace` naming the location of
    /// the call; without std there is no backtrace, so only the fact of the call remains.
    must_produce_diag: Option<()>,

    /// Has this diagnostic context printed any diagnostics? (I.e. has
    /// `self.emitter.emit_diagnostic()` been called?
    has_printed: bool,

    /// This flag indicates that an expected diagnostic was emitted and suppressed.
    /// This is used for the `must_produce_diag` check.
    suppressed_expected_diag: bool,

    /// This set contains the code of all emitted diagnostics to avoid
    /// emitting the same diagnostic with extended help (`--teach`) twice, which
    /// would be unnecessary repetition.
    taught_diagnostics: FxHashSet<ErrCode>,

    /// Used to suggest rustc --explain `<error code>`
    emitted_diagnostic_codes: FxIndexSet<ErrCode>,

    /// This set contains a hash of every diagnostic that has been emitted by
    /// this `DiagCtxt`. These hashes is used to avoid emitting the same error
    /// twice.
    emitted_diagnostics: FxHashSet<Hash128>,

    /// We only want to emit `recursion_depth_exceeding_limit` once per
    /// crate. Otherwise crates like `calimero-store` emit more than
    /// a thousand warnings.
    ///
    /// We only check this in `TRACK_DIAGNOSTIC` meaning that the diagnostics
    /// still get tracked by the query system, even if they don't get emitted
    /// to users.
    emitted_recursion_depth_exceeding_limit: bool,

    /// Stashed diagnostics emitted in one stage of the compiler that may be
    /// stolen and emitted/cancelled by other stages (e.g. to improve them and
    /// add more information). All stashed diagnostics must be emitted with
    /// `emit_stashed_diagnostics` by the time the `DiagCtxtInner` is dropped,
    /// otherwise an assertion failure will occur.
    stashed_diagnostics: FxIndexMap<StashKey, FxIndexMap<Span, Stashed>>,

    /// The log from which `emit_stashed_diagnostics` rebuilds the order a serial run would have
    /// left the stash in, once a par item has stashed or stolen. `None` in a serial run and
    /// until then. See `item_scope::StashReplay`.
    stash_replay: Option<StashReplay>,

    /// This context's value on the `item_scope` clock. A par item scope opened later captures
    /// what this context is told inside the item; one opened earlier (this context was made
    /// inside that item) leaves it alone. See `item_scope`.
    created: u64,

    future_breakage_diagnostics: Vec<DiagInner>,

    /// expected diagnostic will have the level `Expect` which additionally
    /// carries the [`LintExpectationId`] of the expectation that can be
    /// marked as fulfilled. This is a collection of all [`LintExpectationId`]s
    /// that have been marked as fulfilled this way.
    ///
    /// Emitting expectations after having stolen this field can happen. In particular, an
    /// `#[expect(warnings)]` can easily make the `UNFULFILLED_LINT_EXPECTATIONS` lint expect
    /// itself. To avoid needless complexity in this corner case, we tolerate failing to track
    /// those expectations.
    ///
    /// [RFC-2383]: https://rust-lang.github.io/rfcs/2383-lint-reasons.html
    fulfilled_expectations: FxIndexSet<LintExpectationId>,

    /// The file where the ICE information is stored. This allows delayed_span_bug backtraces to be
    /// stored along side the main panic backtrace.
    ice_file: Option<PathBuf>,

    /// Controlled by `-Z hint-msrv`; this allows avoiding emitting lints which would raise MSRV.
    msrv: Option<RustcVersion>,
}

/// One stashed diagnostic: the diagnostic, the guarantee handed out for it if it is an error,
/// and the thread that stashed it (for `err_count_on_current_thread`).
type Stashed = (DiagInner, Option<ErrorGuaranteed>, ThreadId);

/// A key denoting where from a diagnostic was stashed.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum StashKey {
    ItemNoType,
    UnderscoreForArrayLengths,
    EarlySyntaxWarning,
    CallIntoMethod,
    /// When an invalid lifetime e.g. `'2` should be reinterpreted
    /// as a char literal in the parser
    LifetimeIsChar,
    /// Maybe there was a typo where a comma was forgotten before
    /// FRU syntax
    MaybeFruTypo,
    CallAssocMethod,
    AssociatedTypeSuggestion,
    UndeterminedMacroResolution,
    /// Used by `Parser::maybe_recover_trailing_expr`
    ExprInPat,
    /// If in the parser we detect a field expr with turbofish generic params it's possible that
    /// it's a method call without parens. If later on in `hir_typeck` we find out that this is
    /// the case we suppress this message and we give a better suggestion.
    GenericInFieldExpr,
    ReturnTypeNotation,
}

fn default_track_diagnostic<R>(diag: DiagInner, f: &mut dyn FnMut(DiagInner) -> R) -> R {
    (*f)(diag)
}

/// Diagnostics emitted by `DiagCtxtInner::emit_diagnostic` are passed through this function. Used
/// for tracking by incremental, to replay diagnostics as necessary.
pub static TRACK_DIAGNOSTIC: AtomicRef<
    fn(DiagInner, &mut dyn FnMut(DiagInner) -> Option<ErrorGuaranteed>) -> Option<ErrorGuaranteed>,
> = AtomicRef::new(&(default_track_diagnostic as _));

#[derive(Copy, Clone, Default)]
pub struct DiagCtxtFlags {
    /// If false, warning-level lints are suppressed.
    /// (rustc: see `--allow warnings` and `--cap-lints`)
    pub can_emit_warnings: bool,
    /// If Some, the Nth error-level diagnostic is upgraded to bug-level.
    /// (rustc: see `-Z treat-err-as-bug`)
    pub treat_err_as_bug: Option<NonZero<usize>>,
    /// Eagerly emit delayed bugs as errors, so that the compiler debugger may
    /// see all of the errors being emitted at once.
    pub eagerly_emit_delayed_bugs: bool,
    /// Show macro backtraces.
    /// (rustc: see `-Z macro-backtrace`)
    pub macro_backtrace: bool,
    /// If true, identical diagnostics are reported only once.
    pub deduplicate_diagnostics: bool,
    /// Track where errors are created. Enabled with `-Ztrack-diagnostics`.
    pub track_diagnostics: bool,
}

impl Drop for DiagCtxtInner {
    fn drop(&mut self) {
        // For tools using `interface::run_compiler` (e.g. rustc, rustdoc)
        // stashed diagnostics will have already been emitted. But for others
        // that don't use `interface::run_compiler` (e.g. rustfmt, some clippy
        // lints) this fallback is necessary.
        //
        // Important: it is sound to produce an `ErrorGuaranteed` when stashing
        // errors because they are guaranteed to be emitted here or earlier.
        self.emit_stashed_diagnostics();

        // Important: it is sound to produce an `ErrorGuaranteed` when emitting
        // delayed bugs because they are guaranteed to be emitted here if
        // necessary.
        self.flush_delayed();

        // Sanity check: did we use some of the expensive `trimmed_def_paths` functions
        // unexpectedly, that is, without producing diagnostics? If so, for debugging purposes, we
        // suggest where this happened and how to avoid it.
        // `std::thread::panicking()` was `false` here: with `panic = "abort"` there is no
        // unwinding, so a drop glue can never be running because of a panic in the first place.
        if !self.has_printed && !self.suppressed_expected_diag {
            if self.must_produce_diag.is_some() {
                // The captured `Backtrace` that used to name the `set_must_produce_diag` call site
                // is gone with std; only the fact that the call happened survives.
                panic!(
                    "`trimmed_def_paths` called, diagnostics were expected but none were emitted. \
                    Use `with_no_trimmed_paths` for debugging. (no backtrace: the compiler no \
                    longer links std, so the call site cannot be captured.)"
                );
            }
        }
    }
}

impl DiagCtxt {
    pub fn disable_warnings(mut self) -> Self {
        self.inner.get_mut().flags.can_emit_warnings = false;
        self
    }

    pub fn with_flags(mut self, flags: DiagCtxtFlags) -> Self {
        self.inner.get_mut().flags = flags;
        self
    }

    pub fn with_ice_file(mut self, ice_file: PathBuf) -> Self {
        self.inner.get_mut().ice_file = Some(ice_file);
        self
    }

    pub fn with_msrv(mut self, msrv: RustcVersion) -> Self {
        self.inner.get_mut().msrv = Some(msrv);
        self
    }

    pub fn new(emitter: Box<DynEmitter>) -> Self {
        Self { inner: Lock::new(DiagCtxtInner::new(emitter)) }
    }

    pub fn make_silent(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.emitter = Box::new(emitter::SilentEmitter {});
    }

    // `+ DynSend` dropped: no longer an auto trait (see `rustc_data_structures/marker.rs`).
    pub fn set_emitter(&self, emitter: Box<dyn Emitter>) {
        self.inner.borrow_mut().emitter = emitter;
    }

    // This is here to not allow mutation of flags;
    // as of this writing it's used in Session::consider_optimizing and
    // in tests in rustc_interface.
    pub fn can_emit_warnings(&self) -> bool {
        self.inner.borrow_mut().flags.can_emit_warnings
    }

    /// Resets the diagnostic error count as well as the cached emitted diagnostics.
    ///
    /// NOTE: *do not* call this function from rustc. It is only meant to be called from external
    /// tools that want to reuse a `Parser` cleaning the previously emitted diagnostics as well as
    /// the overall count of emitted error diagnostics.
    pub fn reset_err_count(&self) {
        // Use destructuring so that if a field gets added to `DiagCtxtInner`, it's impossible to
        // fail to update this method as well.
        let mut inner = self.inner.borrow_mut();
        let DiagCtxtInner {
            flags: _,
            err_guars,
            lint_err_guars,
            delayed_bugs,
            deduplicated_err_count,
            deduplicated_warn_count,
            emitter: _,
            must_produce_diag,
            has_printed,
            suppressed_expected_diag,
            taught_diagnostics,
            emitted_diagnostic_codes,
            emitted_diagnostics,
            emitted_recursion_depth_exceeding_limit,
            stashed_diagnostics,
            stash_replay,
            created: _,
            future_breakage_diagnostics,
            fulfilled_expectations,
            ice_file: _,
            msrv: _,
        } = inner.deref_mut();

        // For the `Vec`s and `HashMap`s, we overwrite with an empty container to free the
        // underlying memory (which `clear` would not do).
        *err_guars = Default::default();
        *lint_err_guars = Default::default();
        *delayed_bugs = Default::default();
        *deduplicated_err_count = 0;
        *deduplicated_warn_count = 0;
        *must_produce_diag = None;
        *has_printed = false;
        *suppressed_expected_diag = false;
        *taught_diagnostics = Default::default();
        *emitted_diagnostic_codes = Default::default();
        *emitted_diagnostics = Default::default();
        *emitted_recursion_depth_exceeding_limit = false;
        *stashed_diagnostics = Default::default();
        // The stash log goes with the stash it described.
        *stash_replay = None;
        *future_breakage_diagnostics = Default::default();
        *fulfilled_expectations = Default::default();
    }

    pub fn handle<'a>(&'a self) -> DiagCtxtHandle<'a> {
        DiagCtxtHandle { dcx: self, tainted_with_errors: None }
    }

    /// Link this to a taintable context so that emitting errors will automatically set
    /// the `Option<ErrorGuaranteed>` instead of having to do that manually at every error
    /// emission site.
    pub fn taintable_handle<'a>(
        &'a self,
        tainted_with_errors: &'a Cell<Option<ErrorGuaranteed>>,
    ) -> DiagCtxtHandle<'a> {
        DiagCtxtHandle { dcx: self, tainted_with_errors: Some(tainted_with_errors) }
    }
}

impl<'a> DiagCtxtHandle<'a> {
    /// Stashes a diagnostic for possible later improvement in a different,
    /// later stage of the compiler. Possible actions depend on the diagnostic
    /// level:
    /// - Level::Bug, Level:Fatal: not allowed, will trigger a panic.
    /// - Level::Error: immediately counted as an error that has occurred, because it
    ///   is guaranteed to be emitted eventually. Can be later accessed with the
    ///   provided `span` and `key` through
    ///   [`DiagCtxtHandle::try_steal_modify_and_emit_err`] or
    ///   [`DiagCtxtHandle::try_steal_replace_and_emit_err`]. These do not allow
    ///   cancellation or downgrading of the error. Returns
    ///   `Some(ErrorGuaranteed)`.
    /// - Level::DelayedBug: this does happen occasionally with errors that are
    ///   downgraded to delayed bugs. It is not stashed, but immediately
    ///   emitted as a delayed bug. This is because stashing it would cause it
    ///   to be counted by `err_count` which we don't want. It doesn't matter
    ///   that we cannot steal and improve it later, because it's not a
    ///   user-facing error. Returns `Some(ErrorGuaranteed)` as is normal for
    ///   delayed bugs.
    /// - Level::Warning and lower (i.e. !is_error()): can be accessed with the
    ///   provided `span` and `key` through [`DiagCtxtHandle::steal_non_err()`]. This
    ///   allows cancelling and downgrading of the diagnostic. Returns `None`.
    pub fn stash_diagnostic(
        &self,
        span: Span,
        key: StashKey,
        diag: DiagInner,
    ) -> Option<ErrorGuaranteed> {
        let guar = match diag.level {
            Bug | Fatal => {
                self.span_bug(
                    span,
                    format!("invalid level in `stash_diagnostic`: {:?}", diag.level),
                );
            }
            // We delay a bug here so that `-Ztreat-err-as-bug -Zeagerly-emit-delayed-bugs`
            // can be used to create a backtrace at the stashing site instead of whenever the
            // diagnostic context is dropped and thus delayed bugs are emitted.
            Error => Some(self.span_delayed_bug(span, format!("stashing {key:?}"))),
            DelayedBug => {
                return self.inner.borrow_mut().emit_diagnostic(
                    diag,
                    self.tainted_with_errors,
                    Some(DcxRef::of(self.dcx)),
                );
            }
            ForceWarning | Warning | Note | OnceNote | Help | OnceHelp | FailureNote | Allow
            | Expect => None,
        };

        // FIXME(Centril, #69537): Consider reintroducing panic on overwriting a stashed diagnostic
        // if/when we have a more robust macro-friendly replacement for `(span, key)` as a key.
        // See the PR for a discussion.
        let mut inner = self.inner.borrow_mut();
        let span = span.with_parent(None);
        // Logged before it is applied, so a log that begins here snapshots the stash without it.
        inner.record_stash_op(StashOp::Insert(key, span), DcxRef::of(self.dcx));
        inner
            .stashed_diagnostics
            .entry(key)
            .or_default()
            .insert(span, (diag, guar, eko::thread::current_id()));

        guar
    }

    /// Steal a previously stashed non-error diagnostic with the given `Span`
    /// and [`StashKey`] as the key. Panics if the found diagnostic is an
    /// error.
    pub fn steal_non_err(self, span: Span, key: StashKey) -> Option<Diag<'a, ()>> {
        let (diag, guar, _) = self.inner.borrow_mut().steal(key, span, DcxRef::of(self.dcx))?;
        assert!(!diag.is_error());
        assert!(guar.is_none());
        Some(Diag::new_diagnostic(self, diag))
    }

    /// Steals a previously stashed error with the given `Span` and
    /// [`StashKey`] as the key, modifies it, and emits it. Returns `None` if
    /// no matching diagnostic is found. Panics if the found diagnostic's level
    /// isn't `Level::Error`.
    pub fn try_steal_modify_and_emit_err<F>(
        self,
        span: Span,
        key: StashKey,
        mut modify_err: F,
    ) -> Option<ErrorGuaranteed>
    where
        F: FnMut(&mut Diag<'_>),
    {
        let err = self.inner.borrow_mut().steal(key, span, DcxRef::of(self.dcx));
        err.map(|(err, guar, _)| {
            // The use of `::<ErrorGuaranteed>` is safe because level is `Level::Error`.
            assert_eq!(err.level, Error);
            assert!(guar.is_some());
            let mut err = Diag::<ErrorGuaranteed>::new_diagnostic(self, err);
            modify_err(&mut err);
            assert_eq!(err.level, Error);
            err.emit()
        })
    }

    /// Steals a previously stashed error with the given `Span` and
    /// [`StashKey`] as the key, cancels it if found, and emits `new_err`.
    /// Panics if the found diagnostic's level isn't `Level::Error`.
    pub fn try_steal_replace_and_emit_err(
        self,
        span: Span,
        key: StashKey,
        new_err: Diag<'_>,
    ) -> ErrorGuaranteed {
        let old_err = self.inner.borrow_mut().steal(key, span, DcxRef::of(self.dcx));
        match old_err {
            Some((old_err, guar, _)) => {
                assert_eq!(old_err.level, Error);
                assert!(guar.is_some());
                // Because `old_err` has already been counted, it can only be
                // safely cancelled because the `new_err` supplants it.
                Diag::<ErrorGuaranteed>::new_diagnostic(self, old_err).cancel();
            }
            None => {}
        };
        new_err.emit()
    }

    pub fn has_stashed_diagnostic(&self, span: Span, key: StashKey) -> bool {
        let inner = self.inner.borrow();
        if let Some(stashed_diagnostics) = inner.stashed_diagnostics.get(&key)
            && !stashed_diagnostics.is_empty()
        {
            stashed_diagnostics.contains_key(&span.with_parent(None))
        } else {
            false
        }
    }

    /// Emit all stashed diagnostics.
    pub fn emit_stashed_diagnostics(&self) -> Option<ErrorGuaranteed> {
        self.inner.borrow_mut().emit_stashed_diagnostics()
    }

    /// This excludes delayed bugs.
    #[inline]
    pub fn err_count(&self) -> usize {
        let inner = self.inner.borrow();
        inner.err_guars.len()
            + inner.lint_err_guars.len()
            + inner
                .stashed_diagnostics
                .values()
                .map(|a| a.values().filter(|(_, guar, _)| guar.is_some()).count())
                .sum::<usize>()
    }

    /// The number of errors that have been emitted on the *current thread*.
    ///
    /// Like [`DiagCtxtHandle::err_count`], but only counts errors whose recorded
    /// emitting thread is the calling thread.
    pub fn err_count_on_current_thread(&self) -> usize {
        let inner = self.inner.borrow();
        let current = eko::thread::current_id();
        inner.err_guars.iter().filter(|(_, thread)| *thread == current).count()
            + inner.lint_err_guars.iter().filter(|(_, thread)| *thread == current).count()
            + inner
                .stashed_diagnostics
                .values()
                .map(|a| {
                    a.values()
                        .filter(|(_, guar, thread)| guar.is_some() && *thread == current)
                        .count()
                })
                .sum::<usize>()
    }

    /// This excludes lint errors and delayed bugs. Unless absolutely
    /// necessary, prefer `has_errors` to this method.
    pub fn has_errors_excluding_lint_errors(&self) -> Option<ErrorGuaranteed> {
        self.inner.borrow().has_errors_excluding_lint_errors()
    }

    /// This excludes delayed bugs.
    pub fn has_errors(&self) -> Option<ErrorGuaranteed> {
        self.inner.borrow().has_errors()
    }

    /// This excludes nothing. Unless absolutely necessary, prefer `has_errors`
    /// to this method.
    pub fn has_errors_or_delayed_bugs(&self) -> Option<ErrorGuaranteed> {
        self.inner.borrow().has_errors_or_delayed_bugs()
    }

    pub fn print_error_count(&self) {
        let mut inner = self.inner.borrow_mut();

        // Any stashed diagnostics should have been handled by
        // `emit_stashed_diagnostics` by now.
        assert!(inner.stashed_diagnostics.is_empty());

        if inner.treat_err_as_bug() {
            return;
        }

        let warnings = match inner.deduplicated_warn_count {
            0 => Cow::from(""),
            1 => Cow::from("1 warning emitted"),
            count => Cow::from(format!("{count} warnings emitted")),
        };
        let errors = match inner.deduplicated_err_count {
            0 => Cow::from(""),
            1 => Cow::from("aborting due to 1 previous error"),
            count => Cow::from(format!("aborting due to {count} previous errors")),
        };

        match (errors.len(), warnings.len()) {
            (0, 0) => return,
            (0, _) => {
                // Use `ForceWarning` rather than `Warning` to guarantee emission, e.g. with a
                // configuration like `--cap-lints allow --force-warn bare_trait_objects`.
                // The count is a whole-run summary, printed once everything else has been; it is
                // never captured into an item (`None`).
                inner.emit_diagnostic(
                    DiagInner::new(ForceWarning, DiagMessage::Str(warnings)),
                    None,
                    None,
                );
            }
            (_, 0) => {
                inner.emit_diagnostic(
                    DiagInner::new(Error, errors),
                    self.tainted_with_errors,
                    None,
                );
            }
            (_, _) => {
                inner.emit_diagnostic(
                    DiagInner::new(Error, format!("{errors}; {warnings}")),
                    self.tainted_with_errors,
                    None,
                );
            }
        }

        // The "try `rustc --explain E0308`" footer is gone with the explanations it pointed
        // at: 518 markdown files whose only reader was `--explain`, itself a command-line
        // feature this compiler does not have. The error code still travels on
        // `DiagInner::code`, which is the part a client can act on.
    }

    /// This excludes delayed bugs. Used for early aborts after errors occurred
    /// -- e.g. because continuing in the face of errors is likely to lead to
    /// bad results, such as spurious/uninteresting additional errors -- when
    /// returning an error `Result` is difficult.
    pub fn abort_if_errors(&self) {
        if let Some(guar) = self.has_errors() {
            guar.raise_fatal();
        }
    }

    /// `true` if we haven't taught a diagnostic with this code already.
    /// The caller must then teach the user about such a diagnostic.
    ///
    /// Used to suppress emitting the same error multiple times with extended explanation when
    /// calling `-Zteach`.
    pub fn must_teach(&self, code: ErrCode) -> bool {
        self.inner.borrow_mut().taught_diagnostics.insert(code)
    }

    pub fn emit_diagnostic(&self, diagnostic: DiagInner) -> Option<ErrorGuaranteed> {
        self.inner.borrow_mut().emit_diagnostic(
            diagnostic,
            self.tainted_with_errors,
            Some(DcxRef::of(self.dcx)),
        )
    }

    pub fn emit_artifact_notification(&self, path: &Path, artifact_type: &str) {
        self.inner.borrow_mut().emitter.emit_artifact_notification(path, artifact_type);
    }

    pub fn emit_timing_section_start(&self, record: TimingRecord) {
        self.inner.borrow_mut().emitter.emit_timing_section(record, TimingEvent::Start);
    }

    pub fn emit_timing_section_end(&self, record: TimingRecord) {
        self.inner.borrow_mut().emitter.emit_timing_section(record, TimingEvent::End);
    }

    pub fn emit_future_breakage_report(&self) {
        let inner = &mut *self.inner.borrow_mut();
        let diags = mem::take(&mut inner.future_breakage_diagnostics);
        if !diags.is_empty() {
            inner.emitter.emit_future_breakage_report(diags);
        }
    }

    pub fn emit_unused_externs(
        &self,
        lint_level: crate::rustc_lint_defs::Level,
        loud: bool,
        unused_externs: &[&str],
    ) {
        let mut inner = self.inner.borrow_mut();

        // This "error" is an odd duck.
        // - It's only produce with JSON output.
        // - It's not emitted the usual way, via `emit_diagnostic`.
        // - The `$message_type` field is "unused_externs" rather than the usual
        //   "diagnostic".
        //
        // We count it as a lint error because it has a lint level. The value
        // of `loud` (which comes from "unused-externs" or
        // "unused-externs-silent"), also affects whether it's treated like a
        // hard error or not.
        if loud && lint_level.is_error() {
            // This `unchecked_error_guaranteed` is valid. It is where the
            // `ErrorGuaranteed` for unused_extern errors originates.
            #[allow(deprecated)]
            let guar = ErrorGuaranteed::unchecked_error_guaranteed();
            inner.lint_err_guars.push((guar, eko::thread::current_id()));
            inner.panic_if_treat_err_as_bug();
        }

        inner.emitter.emit_unused_externs(lint_level, unused_externs)
    }

    /// This methods steals all [`LintExpectationId`]s that are stored inside
    /// [`DiagCtxtInner`] and indicate that the linked expectation has been fulfilled.
    #[must_use]
    pub fn steal_fulfilled_expectation_ids(&self) -> FxIndexSet<LintExpectationId> {
        mem::take(&mut self.inner.borrow_mut().fulfilled_expectations)
    }

    /// Trigger an ICE if there are any delayed bugs and no hard errors.
    ///
    /// This will panic if there are any stashed diagnostics. You can call
    /// `emit_stashed_diagnostics` to emit those before calling `flush_delayed`.
    pub fn flush_delayed(&self) {
        self.inner.borrow_mut().flush_delayed();
    }

    /// Used when trimmed_def_paths is called and we must produce a diagnostic
    /// to justify its cost.
    #[track_caller]
    pub fn set_must_produce_diag(&self) {
        assert!(
            self.inner.borrow().must_produce_diag.is_none(),
            "should only need to collect a backtrace once"
        );
        // Was `Some(Backtrace::capture())`; there is no backtrace without std.
        self.inner.borrow_mut().must_produce_diag = Some(());
    }
}

// This `impl` block contains only the public diagnostic creation/emission API.
//
// Functions beginning with `struct_`/`create_` create a diagnostic. Other
// functions create and emit a diagnostic all in one go.
impl<'a> DiagCtxtHandle<'a> {
    #[track_caller]
    pub fn struct_bug(self, msg: impl Into<Cow<'static, str>>) -> Diag<'a, BugAbort> {
        Diag::new(self, Bug, msg.into())
    }

    #[track_caller]
    pub fn bug(self, msg: impl Into<Cow<'static, str>>) -> ! {
        self.struct_bug(msg).emit()
    }

    #[track_caller]
    pub fn struct_span_bug(
        self,
        span: impl Into<MultiSpan>,
        msg: impl Into<Cow<'static, str>>,
    ) -> Diag<'a, BugAbort> {
        self.struct_bug(msg).with_span(span)
    }

    #[track_caller]
    pub fn span_bug(self, span: impl Into<MultiSpan>, msg: impl Into<Cow<'static, str>>) -> ! {
        self.struct_span_bug(span, msg.into()).emit()
    }

    #[track_caller]
    pub fn create_bug(self, bug: impl Diagnostic<'a, BugAbort>) -> Diag<'a, BugAbort> {
        bug.into_diag(self, Bug)
    }

    #[track_caller]
    pub fn emit_bug(self, bug: impl Diagnostic<'a, BugAbort>) -> ! {
        self.create_bug(bug).emit()
    }

    #[track_caller]
    pub fn struct_fatal(self, msg: impl Into<DiagMessage>) -> Diag<'a, FatalAbort> {
        Diag::new(self, Fatal, msg)
    }

    #[track_caller]
    pub fn fatal(self, msg: impl Into<DiagMessage>) -> ! {
        self.struct_fatal(msg).emit()
    }

    #[track_caller]
    pub fn struct_span_fatal(
        self,
        span: impl Into<MultiSpan>,
        msg: impl Into<DiagMessage>,
    ) -> Diag<'a, FatalAbort> {
        self.struct_fatal(msg).with_span(span)
    }

    #[track_caller]
    pub fn span_fatal(self, span: impl Into<MultiSpan>, msg: impl Into<DiagMessage>) -> ! {
        self.struct_span_fatal(span, msg).emit()
    }

    #[track_caller]
    pub fn create_fatal(self, fatal: impl Diagnostic<'a, FatalAbort>) -> Diag<'a, FatalAbort> {
        fatal.into_diag(self, Fatal)
    }

    #[track_caller]
    pub fn emit_fatal(self, fatal: impl Diagnostic<'a, FatalAbort>) -> ! {
        self.create_fatal(fatal).emit()
    }

    #[track_caller]
    pub fn create_almost_fatal(
        self,
        fatal: impl Diagnostic<'a, FatalError>,
    ) -> Diag<'a, FatalError> {
        fatal.into_diag(self, Fatal)
    }

    #[track_caller]
    pub fn emit_almost_fatal(self, fatal: impl Diagnostic<'a, FatalError>) -> FatalError {
        self.create_almost_fatal(fatal).emit()
    }

    // FIXME: This method should be removed (every error should have an associated error code).
    #[track_caller]
    pub fn struct_err(self, msg: impl Into<DiagMessage>) -> Diag<'a> {
        Diag::new(self, Error, msg)
    }

    #[track_caller]
    pub fn err(self, msg: impl Into<DiagMessage>) -> ErrorGuaranteed {
        self.struct_err(msg).emit()
    }

    #[track_caller]
    pub fn struct_span_err(
        self,
        span: impl Into<MultiSpan>,
        msg: impl Into<DiagMessage>,
    ) -> Diag<'a> {
        self.struct_err(msg).with_span(span)
    }

    #[track_caller]
    pub fn span_err(
        self,
        span: impl Into<MultiSpan>,
        msg: impl Into<DiagMessage>,
    ) -> ErrorGuaranteed {
        self.struct_span_err(span, msg).emit()
    }

    #[track_caller]
    pub fn create_err(self, err: impl Diagnostic<'a>) -> Diag<'a> {
        err.into_diag(self, Error)
    }

    #[track_caller]
    pub fn emit_err(self, err: impl Diagnostic<'a>) -> ErrorGuaranteed {
        self.create_err(err).emit()
    }

    /// Ensures that an error is printed. See [`Level::DelayedBug`].
    #[track_caller]
    pub fn delayed_bug(self, msg: impl Into<Cow<'static, str>>) -> ErrorGuaranteed {
        Diag::<ErrorGuaranteed>::new(self, DelayedBug, msg.into()).emit()
    }

    /// Ensures that an error is printed. See [`Level::DelayedBug`].
    ///
    /// Note: this function used to be called `delay_span_bug`. It was renamed
    /// to match similar functions like `span_err`, `span_warn`, etc.
    #[track_caller]
    pub fn span_delayed_bug(
        self,
        sp: impl Into<MultiSpan>,
        msg: impl Into<Cow<'static, str>>,
    ) -> ErrorGuaranteed {
        Diag::<ErrorGuaranteed>::new(self, DelayedBug, msg.into()).with_span(sp).emit()
    }

    #[track_caller]
    pub fn struct_warn(self, msg: impl Into<DiagMessage>) -> Diag<'a, ()> {
        Diag::new(self, Warning, msg)
    }

    #[track_caller]
    pub fn warn(self, msg: impl Into<DiagMessage>) {
        self.struct_warn(msg).emit()
    }

    #[track_caller]
    pub fn struct_span_warn(
        self,
        span: impl Into<MultiSpan>,
        msg: impl Into<DiagMessage>,
    ) -> Diag<'a, ()> {
        self.struct_warn(msg).with_span(span)
    }

    #[track_caller]
    pub fn span_warn(self, span: impl Into<MultiSpan>, msg: impl Into<DiagMessage>) {
        self.struct_span_warn(span, msg).emit()
    }

    #[track_caller]
    pub fn create_warn(self, warning: impl Diagnostic<'a, ()>) -> Diag<'a, ()> {
        warning.into_diag(self, Warning)
    }

    #[track_caller]
    pub fn emit_warn(self, warning: impl Diagnostic<'a, ()>) {
        self.create_warn(warning).emit()
    }

    #[track_caller]
    pub fn struct_note(self, msg: impl Into<DiagMessage>) -> Diag<'a, ()> {
        Diag::new(self, Note, msg)
    }

    #[track_caller]
    pub fn note(&self, msg: impl Into<DiagMessage>) {
        self.struct_note(msg).emit()
    }

    #[track_caller]
    pub fn struct_span_note(
        self,
        span: impl Into<MultiSpan>,
        msg: impl Into<DiagMessage>,
    ) -> Diag<'a, ()> {
        self.struct_note(msg).with_span(span)
    }

    #[track_caller]
    pub fn span_note(self, span: impl Into<MultiSpan>, msg: impl Into<DiagMessage>) {
        self.struct_span_note(span, msg).emit()
    }

    #[track_caller]
    pub fn create_note(self, note: impl Diagnostic<'a, ()>) -> Diag<'a, ()> {
        note.into_diag(self, Note)
    }

    #[track_caller]
    pub fn emit_note(self, note: impl Diagnostic<'a, ()>) {
        self.create_note(note).emit()
    }

    #[track_caller]
    pub fn struct_help(self, msg: impl Into<DiagMessage>) -> Diag<'a, ()> {
        Diag::new(self, Help, msg)
    }

    #[track_caller]
    pub fn struct_failure_note(self, msg: impl Into<DiagMessage>) -> Diag<'a, ()> {
        Diag::new(self, FailureNote, msg)
    }

    #[track_caller]
    pub fn struct_allow(self, msg: impl Into<DiagMessage>) -> Diag<'a, ()> {
        Diag::new(self, Allow, msg)
    }

    #[track_caller]
    pub fn struct_expect(self, msg: impl Into<DiagMessage>, id: LintExpectationId) -> Diag<'a, ()> {
        Diag::new(self, Expect, msg).with_lint_id(id)
    }
}

// Note: we prefer implementing operations on `DiagCtxt`, rather than
// `DiagCtxtInner`, whenever possible. This minimizes functions where
// `DiagCtxt::foo()` just borrows `inner` and forwards a call to
// `DiagCtxtInner::foo`.
impl DiagCtxtInner {
    fn new(emitter: Box<DynEmitter>) -> Self {
        Self {
            flags: DiagCtxtFlags { can_emit_warnings: true, ..Default::default() },
            err_guars: Vec::new(),
            lint_err_guars: Vec::new(),
            delayed_bugs: Vec::new(),
            deduplicated_err_count: 0,
            deduplicated_warn_count: 0,
            emitter,
            must_produce_diag: None,
            has_printed: false,
            suppressed_expected_diag: false,
            taught_diagnostics: Default::default(),
            emitted_diagnostic_codes: Default::default(),
            emitted_diagnostics: Default::default(),
            emitted_recursion_depth_exceeding_limit: false,
            stashed_diagnostics: Default::default(),
            stash_replay: None,
            created: item_scope::tick(),
            future_breakage_diagnostics: Vec::new(),
            fulfilled_expectations: Default::default(),
            ice_file: None,
            msrv: None,
        }
    }

    /// Emit all stashed diagnostics.
    fn emit_stashed_diagnostics(&mut self) -> Option<ErrorGuaranteed> {
        let mut guar = None;
        let has_errors = !self.err_guars.is_empty();
        let stashed = mem::take(&mut self.stashed_diagnostics);
        // The stash's own order is the serial one unless a par item stashed or stole while
        // items ran in parallel; then the log, filled in item order by the replay, says what
        // the serial order would have been. A serial run never has a log.
        let in_order: Vec<Stashed> = match self.stash_replay.take() {
            None => stashed.into_iter().flat_map(|(_, spans)| spans.into_values()).collect(),
            Some(replay) => replay.in_serial_order(stashed),
        };
        for (diag, _guar, _thread) in in_order {
            if !diag.is_error() {
                // Unless they're forced, don't flush stashed warnings when
                // there are errors, to avoid causing warning overload. The
                // stash would've been stolen already if it were important.
                if !diag.is_force_warn() && has_errors {
                    continue;
                }
            }
            guar = guar.or(self.emit_diagnostic(diag, None, None));
        }
        guar
    }

    /// Take a stashed diagnostic out, logging the steal when its order has to be replayed.
    fn steal(&mut self, key: StashKey, span: Span, origin: DcxRef) -> Option<Stashed> {
        let span = span.with_parent(None);
        // Logged only when something is actually taken, and before it is taken, so the log
        // removes exactly what the live stash lost.
        if self.stashed_diagnostics.get(&key).is_some_and(|spans| spans.contains_key(&span)) {
            self.record_stash_op(StashOp::Remove(key, span), origin);
        }
        // FIXME(#120456) - is `swap_remove` correct?
        self.stashed_diagnostics.get_mut(&key).and_then(|spans| spans.swap_remove(&span))
    }

    /// Put a stash or steal in serial order.
    ///
    /// The operation itself is applied live by the caller, because the item needs its effect
    /// at once. Inside a par item only its order is captured, and reaches the log when the
    /// item is replayed; the log's base is snapshotted here, on the first captured operation,
    /// before it is applied, so the base holds only what was stashed before any item ran.
    /// Outside every item the operation goes into the log directly once one exists; with no
    /// log (a serial run, or no item has touched the stash) there is nothing to do.
    fn record_stash_op(&mut self, op: StashOp, origin: DcxRef) {
        if let Some(capture) = item_scope::capture(origin, self.created) {
            if self.stash_replay.is_none() {
                self.stash_replay = Some(StashReplay::begin(&self.stashed_diagnostics));
            }
            capture.push(EventKind::Stash(op));
        } else if let Some(replay) = &mut self.stash_replay {
            replay.record(op);
        }
    }

    /// Hand a diagnostic that passed every check made at emission to the emitter, or, inside a
    /// par item, to the item's owned output. See `item_scope`.
    fn print_or_capture(&mut self, diagnostic: DiagInner, origin: Option<DcxRef>) {
        match origin.and_then(|origin| item_scope::capture(origin, self.created)) {
            Some(capture) => capture.push(EventKind::Print(diagnostic)),
            None => self.print(diagnostic),
        }
    }

    /// Apply an event an item captured, now that its turn in item order has come. Called by
    /// `item_scope` with this context's lock held, outside every item scope that captures it.
    ///
    /// Nothing is counted here: every count a running item can observe was made when the item
    /// emitted. See `item_scope` for why.
    fn replay(&mut self, event: EventKind) {
        match event {
            EventKind::Print(diagnostic) => self.print(diagnostic),
            EventKind::DelayedBug(bug, guar) => {
                // `emit_diagnostic` stops recording delayed bugs once an error exists, and the
                // first error clears the ones already recorded. Both come to "keep it only if
                // there is still no error", which is what a serial run ends up with.
                if self.has_errors().is_none() {
                    self.delayed_bugs.push((bug, guar));
                }
            }
            EventKind::Stash(op) => {
                // The log exists: it was begun when this operation was captured, and only
                // `reset_err_count` removes it, taking the stash it described with it.
                if let Some(replay) = &mut self.stash_replay {
                    replay.record(op);
                }
            }
        }
    }

    /// The printing half of emission: the decisions that depend on what was printed before,
    /// then the emitter, which renders the diagnostic to text here and only here. Runs at
    /// emission in a serial run and at replay for a par item's output, and in both it runs in
    /// serial order, which is what keeps the first occurrence of a duplicate the serial one.
    ///
    /// This is the code that stood inside the `TRACK_DIAGNOSTIC` closure in `emit_diagnostic`,
    /// statement for statement, moved so the replay can reach it.
    fn print(&mut self, mut diagnostic: DiagInner) {
        let already_emitted = {
            let mut hasher = StableHasher::new();
            diagnostic.hash(&mut hasher);
            let diagnostic_hash = hasher.finish();
            !self.emitted_diagnostics.insert(diagnostic_hash)
        };

        let is_error = diagnostic.is_error();
        // We only emit the first occurrence of `recursion_depth_exceeding_limit`.
        let silence_recursion_depth_exceeded_limit =
            diagnostic.is_lint.as_ref().is_some_and(|lint| {
                lint.name.eq_ignore_ascii_case(
                    crate::rustc_lint_defs::builtin::RECURSION_DEPTH_EXCEEDING_LIMIT.name,
                ) && mem::replace(&mut self.emitted_recursion_depth_exceeding_limit, true)
            });

        // Only emit the diagnostic if we've been asked to deduplicate or
        // haven't already emitted an equivalent diagnostic.
        if !silence_recursion_depth_exceeded_limit
            && !(self.flags.deduplicate_diagnostics && already_emitted)
        {
            debug!(?diagnostic);
            debug!(?self.emitted_diagnostics);

            let not_yet_emitted = |sub: &mut Subdiag| {
                debug!(?sub);
                if sub.level != OnceNote && sub.level != OnceHelp {
                    return true;
                }
                let mut hasher = StableHasher::new();
                sub.hash(&mut hasher);
                let diagnostic_hash = hasher.finish();
                debug!(?diagnostic_hash);
                self.emitted_diagnostics.insert(diagnostic_hash)
            };
            diagnostic.children.retain_mut(not_yet_emitted);
            if already_emitted {
                let msg = "duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`";
                diagnostic.sub(Note, msg, MultiSpan::new());
            }

            if is_error {
                self.deduplicated_err_count += 1;
            } else if matches!(diagnostic.level, ForceWarning | Warning) {
                self.deduplicated_warn_count += 1;
            }
            self.has_printed = true;

            self.emitter.emit_diagnostic(diagnostic);
        }
    }

    // Return value is only `Some` if the level is `Error` or `DelayedBug`.
    //
    // `origin` is the `DiagCtxt` this inner state belongs to, passed by the public entry points
    // so that a par item can capture what it emits and have it replayed into the right context
    // later (see `item_scope`). `None` from the paths that only ever run outside every item:
    // stashed-diagnostic emission, the error count, delayed-bug flushing.
    fn emit_diagnostic(
        &mut self,
        mut diagnostic: DiagInner,
        taint: Option<&Cell<Option<ErrorGuaranteed>>>,
        origin: Option<DcxRef>,
    ) -> Option<ErrorGuaranteed> {
        if diagnostic.has_future_breakage() {
            // Future breakages aren't emitted if they're `Level::Allow` or
            // `Level::Expect`, but they still need to be constructed and
            // stashed below, so they'll trigger the must_produce_diag check.
            assert_matches!(diagnostic.level, Error | ForceWarning | Warning | Allow | Expect);
            self.future_breakage_diagnostics.push(diagnostic.clone());
        }

        // We call TRACK_DIAGNOSTIC with an empty closure for the cases that
        // return early *and* have some kind of side-effect, except where
        // noted.
        match diagnostic.level {
            Bug => {}
            Fatal | Error => {
                if self.treat_next_err_as_bug() {
                    // `Fatal` and `Error` can be promoted to `Bug`.
                    diagnostic.level = Bug;
                }
            }
            DelayedBug => {
                // Note that because we check these conditions first,
                // `-Zeagerly-emit-delayed-bugs` and `-Ztreat-err-as-bug`
                // continue to work even after we've issued an error and
                // stopped recording new delayed bugs.
                if self.flags.eagerly_emit_delayed_bugs {
                    // `DelayedBug` can be promoted to `Error` or `Bug`.
                    if self.treat_next_err_as_bug() {
                        diagnostic.level = Bug;
                    } else {
                        diagnostic.level = Error;
                    }
                } else {
                    // If we have already emitted at least one error, we don't need
                    // to record the delayed bug, because it'll never be used.
                    return if let Some(guar) = self.has_errors() {
                        Some(guar)
                    } else {
                        // No `TRACK_DIAGNOSTIC` call is needed, because the
                        // incremental session is deleted if there is a delayed
                        // bug. This also saves us from cloning the diagnostic.
                        // A `Backtrace::capture()` used to be taken here and carried on the
                        // delayed bug so `flush_delayed` could print where it was created.
                        // There is no backtrace without std, so nothing is captured.
                        // This `unchecked_error_guaranteed` is valid. It is where the
                        // `ErrorGuaranteed` for delayed bugs originates. See
                        // `DiagCtxtInner::drop`.
                        #[allow(deprecated)]
                        let guar = ErrorGuaranteed::unchecked_error_guaranteed();
                        let bug = DelayedDiagInner::new(diagnostic);
                        // Inside a par item the bug is the item's output and reaches
                        // `delayed_bugs` at replay, so `flush_delayed` reports bugs in the
                        // order a serial run would. Otherwise it is recorded now, as always.
                        match origin.and_then(|o| item_scope::capture(o, self.created)) {
                            Some(capture) => capture.push(EventKind::DelayedBug(bug, guar)),
                            None => self.delayed_bugs.push((bug, guar)),
                        }
                        Some(guar)
                    };
                }
            }
            ForceWarning if diagnostic.lint_id.is_none() => {} // `ForceWarning(Some(...))` is below, with `Expect`
            Warning => {
                if !self.flags.can_emit_warnings {
                    // We are not emitting warnings.
                    if diagnostic.has_future_breakage() {
                        // The side-effect is at the top of this method.
                        TRACK_DIAGNOSTIC(diagnostic, &mut |_| None);
                    }
                    return None;
                }
            }
            Note | Help | FailureNote => {}
            OnceNote | OnceHelp => panic!("bad level: {:?}", diagnostic.level),
            Allow => {
                // Nothing emitted for allowed lints.
                if diagnostic.has_future_breakage() {
                    // The side-effect is at the top of this method.
                    TRACK_DIAGNOSTIC(diagnostic, &mut |_| None);
                    self.suppressed_expected_diag = true;
                }
                return None;
            }
            Expect | ForceWarning => {
                self.fulfilled_expectations.insert(diagnostic.lint_id.unwrap());
                if let Expect = diagnostic.level {
                    // Nothing emitted here for expected lints.
                    TRACK_DIAGNOSTIC(diagnostic, &mut |_| None);
                    self.suppressed_expected_diag = true;
                    return None;
                }
            }
        }

        if let (Some(msrv), Some(diag_msrv)) = (self.msrv, diagnostic.rust_version())
            && diag_msrv > msrv
        {
            return None;
        }

        TRACK_DIAGNOSTIC(diagnostic, &mut |diagnostic| {
            if let Some(code) = diagnostic.code {
                self.emitted_diagnostic_codes.insert(code);
            }

            let is_error = diagnostic.is_error();
            let is_lint = diagnostic.is_lint.is_some();

            // **Emission is split in two here.** What follows the call is the half a running
            // item can observe (the error guarantee, the counts behind `has_errors`, taint,
            // `treat_err_as_bug`), and it happens now, in every mode: counted once, at
            // collection. The half that decides what is printed (duplicates, once-only notes,
            // the printed counts, the emitter itself) is `print`, and inside a par item it
            // becomes the item's owned output and runs when the item is replayed in order.
            //
            // In a serial run no item scope is ever open, `print_or_capture` calls `print` at
            // once, and `print` is the code that stood here, statement for statement, so the
            // output is byte-identical. The one move is that `is_error`/`is_lint` are read
            // before the duplicate hash rather than after; neither the hash nor anything else
            // between them touches the level or `is_lint`.
            self.print_or_capture(diagnostic, origin);

            if is_error {
                // If we have any delayed bugs recorded, we can discard them
                // because they won't be used. (This should only occur if there
                // have been no errors previously emitted, because we don't add
                // new delayed bugs once the first error is emitted.)
                if !self.delayed_bugs.is_empty() {
                    assert_eq!(self.lint_err_guars.len() + self.err_guars.len(), 0);
                    self.delayed_bugs.clear();
                    self.delayed_bugs.shrink_to_fit();
                }

                // This `unchecked_error_guaranteed` is valid. It is where the
                // `ErrorGuaranteed` for errors and lint errors originates.
                #[allow(deprecated)]
                let guar = ErrorGuaranteed::unchecked_error_guaranteed();
                let thread = eko::thread::current_id();
                if is_lint {
                    self.lint_err_guars.push((guar, thread));
                } else {
                    if let Some(taint) = taint {
                        taint.set(Some(guar));
                    }
                    self.err_guars.push((guar, thread));
                }
                self.panic_if_treat_err_as_bug();
                Some(guar)
            } else {
                None
            }
        })
    }

    fn treat_err_as_bug(&self) -> bool {
        self.flags
            .treat_err_as_bug
            .is_some_and(|c| self.err_guars.len() + self.lint_err_guars.len() >= c.get())
    }

    // Use this one before incrementing `err_count`.
    fn treat_next_err_as_bug(&self) -> bool {
        self.flags
            .treat_err_as_bug
            .is_some_and(|c| self.err_guars.len() + self.lint_err_guars.len() + 1 >= c.get())
    }

    fn has_errors_excluding_lint_errors(&self) -> Option<ErrorGuaranteed> {
        self.err_guars.get(0).map(|(guar, _)| *guar).or_else(|| {
            if let Some((_diag, guar, _)) = self
                .stashed_diagnostics
                .values()
                .flat_map(|stashed_diagnostics| stashed_diagnostics.values())
                .find(|(diag, guar, _)| guar.is_some() && diag.is_lint.is_none())
            {
                *guar
            } else {
                None
            }
        })
    }

    fn has_errors(&self) -> Option<ErrorGuaranteed> {
        self.err_guars
            .get(0)
            .map(|(guar, _)| *guar)
            .or_else(|| self.lint_err_guars.get(0).map(|(guar, _)| *guar))
            .or_else(|| {
                self.stashed_diagnostics.values().find_map(|stashed_diagnostics| {
                    stashed_diagnostics.values().find_map(|(_, guar, _)| *guar)
                })
            })
    }

    fn has_errors_or_delayed_bugs(&self) -> Option<ErrorGuaranteed> {
        self.has_errors().or_else(|| self.delayed_bugs.get(0).map(|(_, guar)| guar).copied())
    }

    fn flush_delayed(&mut self) {
        // Stashed diagnostics must be emitted before delayed bugs are flushed.
        // Otherwise, we might ICE prematurely when errors would have
        // eventually happened.
        assert!(self.stashed_diagnostics.is_empty());

        if !self.err_guars.is_empty() {
            // If an error happened already. We shouldn't expose delayed bugs.
            return;
        }

        if self.delayed_bugs.is_empty() {
            // Nothing to do.
            return;
        }

        let bugs: Vec<_> = mem::take(&mut self.delayed_bugs).into_iter().map(|(b, _)| b).collect();

        let backtrace = eko::env::var_os("RUST_BACKTRACE").as_deref() != Some(b"0".as_slice());
        let decorate = backtrace || self.ice_file.is_none();
        let mut out = self
            .ice_file
            .as_ref()
            .and_then(|file| eko::file::File::append(file).ok());

        // Put the overall explanation before the `DelayedBug`s, to frame them
        // better (e.g. separate warnings from them). Also, use notes, which
        // don't count as errors, to avoid possibly triggering
        // `-Ztreat-err-as-bug`, which we don't want.
        let note1 = "no errors encountered even though delayed bugs were created";
        let note2 = "those delayed bugs will now be shown as internal compiler errors";
        self.emit_diagnostic(DiagInner::new(Note, note1), None, None);
        self.emit_diagnostic(DiagInner::new(Note, note2), None, None);

        for bug in bugs {
            if let Some(out) = &mut out {
                // The second line was the delayed bug's captured `Backtrace`; there is no
                // backtrace without std, so only the message is written to the ICE file.
                //
                // **Substituted, and never dropped.** This was
                // `filter_map(|(msg, _)| msg.as_str())`, and `as_str` answers `None` for a
                // template - the same hole that made `PlainEmitter` print `error[E0463]:` with
                // nothing after the colon. Here it was quieter and worse: a delayed bug built
                // from a `#[diag("...")]` attribute or a `msg!` is a `DiagMessage::Inline`, so
                // the ICE file recorded the empty string and the one artefact left to read said
                // nothing at all. `try_format_diag_message` does the substitution and
                // `diag_message_source` hands back the raw template when it cannot, which is
                // exactly what the two emitters do.
                let message: String = bug
                    .inner
                    .messages
                    .iter()
                    .map(|(msg, _)| {
                        try_format_diag_message(msg, &bug.inner.args)
                            .unwrap_or_else(|| Cow::Borrowed(diag_message_source(msg)))
                    })
                    .collect();
                _ = write!(out, "delayed bug: {message}\n");
            }

            let mut bug = if decorate { bug.decorate() } else { bug.inner };

            // "Undelay" the delayed bugs into plain bugs.
            if bug.level != DelayedBug {
                // NOTE(eddyb) not panicking here because we're already producing
                // an ICE, and the more information the merrier.
                //
                // We are at the `DiagInner`/`DiagCtxtInner` level rather than
                // the usual `Diag`/`DiagCtxt` level, so we must augment `bug`
                // in a lower-level fashion.
                let msg = msg!(
                    "`flushed_delayed` got diagnostic with level {$level}, instead of the expected `DelayedBug`"
                ).arg("level", bug.level).format();
                bug.sub(Note, msg, bug.span.primary_span().unwrap().into());
            }
            bug.level = Bug;

            self.emit_diagnostic(bug, None, None);
        }

        // Panic with `DelayedBugPanic` to avoid "unexpected panic" messages.
        // Was `panic::panic_any(DelayedBugPanic)`. With `panic = "abort"` there is no unwinding,
        // so nothing can catch the payload and the marker type cannot be recovered; the
        // containment that let a caller downcast to `DelayedBugPanic` was dropped with the panic
        // runtime. Same shape as `BugAbort` in `diagnostic.rs`.
        panic!("{}", stringify!(DelayedBugPanic));
    }

    fn panic_if_treat_err_as_bug(&self) {
        if self.treat_err_as_bug() {
            let n = self.flags.treat_err_as_bug.map(|c| c.get()).unwrap();
            assert_eq!(n, self.err_guars.len() + self.lint_err_guars.len());
            if n == 1 {
                panic!("aborting due to `-Z treat-err-as-bug=1`");
            } else {
                panic!("aborting after {n} errors due to `-Z treat-err-as-bug={n}`");
            }
        }
    }
}

struct DelayedDiagInner {
    inner: DiagInner,
    // The `note: Backtrace` field is gone. It held the `Backtrace` captured where the delayed
    // bug was created, and there is no backtrace without std, so the note it rendered into is
    // gone with it and only `emitted_at` remains to locate the bug.
}

impl DelayedDiagInner {
    fn new(diagnostic: DiagInner) -> Self {
        DelayedDiagInner { inner: diagnostic }
    }

    fn decorate(self) -> DiagInner {
        // We are at the `DiagInner`/`DiagCtxtInner` level rather than the
        // usual `Diag`/`DiagCtxt` level, so we must construct `diag` in a
        // lower-level fashion.
        let mut diag = self.inner;
        // This used to branch on whether the backtrace was captured, and interpolate it as
        // `{$note}`. With no backtrace there is one message and no note argument.
        let msg = msg!("delayed at {$emitted_at}")
            .arg("emitted_at", diag.emitted_at.clone())
            .format();
        diag.sub(Note, msg, diag.span.primary_span().unwrap_or(DUMMY_SP).into());
        diag
    }
}

/// | Level        | is_error | EmissionGuarantee            | Top-level | Sub | Used in lints?
/// | -----        | -------- | -----------------            | --------- | --- | --------------
/// | Bug          | yes      | BugAbort                     | yes       | -   | -
/// | Fatal        | yes      | FatalAbort/FatalError[^star] | yes       | -   | -
/// | Error        | yes      | ErrorGuaranteed              | yes       | -   | yes
/// | DelayedBug   | yes      | ErrorGuaranteed              | yes       | -   | -
/// | ForceWarning | -        | ()                           | yes       | -   | lint-only
/// | Warning      | -        | ()                           | yes       | yes | yes
/// | Note         | -        | ()                           | rare      | yes | -
/// | OnceNote     | -        | ()                           | -         | yes | lint-only
/// | Help         | -        | ()                           | rare      | yes | -
/// | OnceHelp     | -        | ()                           | -         | yes | lint-only
/// | FailureNote  | -        | ()                           | rare      | -   | -
/// | Allow        | -        | ()                           | yes       | -   | lint-only
/// | Expect       | -        | ()                           | yes       | -   | lint-only
///
/// [^star]: `FatalAbort` normally, `FatalError` in the non-aborting "almost fatal" case that is
///     occasionally used.
///
#[derive(Copy, PartialEq, Eq, Clone, Hash, Debug, Encodable, Decodable)]
pub enum Level {
    /// For bugs in the compiler. Manifests as an ICE (internal compiler error) panic.
    Bug,

    /// An error that causes an immediate abort. Used for things like configuration errors,
    /// internal overflows, some file operation errors.
    Fatal,

    /// An error in the code being compiled, which prevents compilation from finishing. This is the
    /// most common case.
    Error,

    /// This is a strange one: lets you register an error without emitting it. If compilation ends
    /// without any other errors occurring, this will be emitted as a bug. Otherwise, it will be
    /// silently dropped. I.e. "expect other errors are emitted" semantics. Useful on code paths
    /// that should only be reached when compiling erroneous code.
    DelayedBug,

    /// A `force-warn` lint warning about the code being compiled. Does not prevent compilation
    /// from finishing.
    ///
    /// Requires a [`LintExpectationId`] for expected lint diagnostics. In all other cases this
    /// should be `None`.
    ForceWarning,

    /// A warning about the code being compiled. Does not prevent compilation from finishing.
    /// Will be skipped if `can_emit_warnings` is false.
    Warning,

    /// A message giving additional context.
    Note,

    /// A note that is only emitted once.
    OnceNote,

    /// A message suggesting how to fix something.
    Help,

    /// A help that is only emitted once.
    OnceHelp,

    /// Similar to `Note`, but used in cases where compilation has failed. When printed for human
    /// consumption, it doesn't have any kind of `note:` label.
    FailureNote,

    /// Only used for lints.
    Allow,

    /// Only used for lints. Requires a [`LintExpectationId`] for silencing the lints.
    Expect,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_str().fmt(f)
    }
}

impl Level {
    // `Level::color` chose an ANSI colour per severity, for the snippet renderer that is gone.

    pub fn to_str(self) -> &'static str {
        match self {
            Bug | DelayedBug => "error: internal compiler error",
            Fatal | Error => "error",
            ForceWarning | Warning => "warning",
            Note | OnceNote => "note",
            Help | OnceHelp => "help",
            FailureNote => "failure-note",
            Allow | Expect => unreachable!(),
        }
    }

    pub fn is_failure_note(&self) -> bool {
        matches!(*self, FailureNote)
    }
}

impl IntoDiagArg for Level {
    fn into_diag_arg(self, _: &mut crate::rustc_error_messages::LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::from(self.to_string()))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Encodable, Decodable)]
pub enum Style {
    MainHeaderMsg,
    HeaderMsg,
    LineAndColumn,
    LineNumber,
    Quotation,
    UnderlinePrimary,
    UnderlineSecondary,
    LabelPrimary,
    LabelSecondary,
    NoStyle,
    Level(Level),
    Highlight,
    Addition,
    Removal,
}

// FIXME(eddyb) this doesn't belong here AFAICT, should be moved to callsite.
pub fn elided_lifetime_in_path_suggestion(
    source_map: &SourceMap,
    n: usize,
    path_span: Span,
    incl_angl_brckt: bool,
    insertion_span: Span,
) -> ElidedLifetimeInPathSubdiag {
    let expected = ExpectedLifetimeParameter { span: path_span, count: n };
    // Do not try to suggest anything if generated by a proc-macro.
    let indicate = source_map.is_span_accessible(insertion_span).then(|| {
        let anon_lts = vec!["'_"; n].join(", ");
        let suggestion =
            if incl_angl_brckt { format!("<{anon_lts}>") } else { format!("{anon_lts}, ") };

        IndicateAnonymousLifetime { span: insertion_span.shrink_to_hi(), count: n, suggestion }
    });

    ElidedLifetimeInPathSubdiag { expected, indicate }
}

/// Grammatical tool for displaying messages to end users in a nice form.
///
/// Returns "an" if the given string starts with a vowel, and "a" otherwise.
pub fn a_or_an(s: &str) -> &'static str {
    let mut chars = s.chars();
    let Some(mut first_alpha_char) = chars.next() else {
        return "a";
    };
    if first_alpha_char == '`' {
        let Some(next) = chars.next() else {
            return "a";
        };
        first_alpha_char = next;
    }
    if ["a", "e", "i", "o", "u", "&"].contains(&&first_alpha_char.to_lowercase().to_string()[..]) {
        "an"
    } else {
        "a"
    }
}

#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum TerminalUrl {
    No,
    Yes,
    Auto,
}
pub use crate::struct_span_code_err;
