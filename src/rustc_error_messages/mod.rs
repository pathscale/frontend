// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
// No real std remains in this crate. The types diagnostics render are now
// `eko::file::Error` and `eko::path::Path`; `Backtrace` and `ExitStatus`
// were dropped rather than replaced, so `extern crate std;` is gone with them.

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
use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use alloc::borrow::Cow;

use rustc_macros::{Decodable, Encodable, StableHash};
use crate::rustc_span::Span;

mod diagnostic_impls;
pub use diagnostic_impls::DiagArgFromDisplay;
use crate::rustc_data_structures::fx::FxIndexMap;

/// Abstraction over a message in a diagnostic: one that still has arguments to interpolate, and
/// one that is already the final text.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encodable, Decodable, StableHash)]
pub enum DiagMessage {
    /// A message with nothing left to substitute, or one that was rendered eagerly.
    ///
    /// Some diagnostics have repeated subdiagnostics where the same interpolated variables would
    /// be instantiated multiple times with different values. These subdiagnostics' messages
    /// are rendered when they are added to the parent diagnostic. This is one of the ways
    /// this variant of `DiagMessage` is produced.
    Str(Cow<'static, str>),
    /// An unrendered template, in the syntax `frontend_diag_template` parses: the literal from a
    /// `#[diag("...")]` attribute or a `msg!("...")`, with its `{$args}` still in place.
    Inline(Cow<'static, str>),
}

impl DiagMessage {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            DiagMessage::Str(s) => Some(s),
            DiagMessage::Inline(_) => None,
        }
    }
}

impl From<String> for DiagMessage {
    fn from(s: String) -> Self {
        DiagMessage::Str(Cow::Owned(s))
    }
}
impl From<&'static str> for DiagMessage {
    fn from(s: &'static str) -> Self {
        DiagMessage::Str(Cow::Borrowed(s))
    }
}
impl From<Cow<'static, str>> for DiagMessage {
    fn from(s: Cow<'static, str>) -> Self {
        DiagMessage::Str(s)
    }
}

/// A span together with some additional data.
#[derive(Clone, Debug)]
pub struct SpanLabel {
    /// The span we are going to include in the final snippet.
    pub span: Span,

    /// Is this a primary span? This is the "locus" of the message,
    /// and is indicated with a `^^^^` underline, versus `----`.
    pub is_primary: bool,

    /// What label should we attach to this span (if any)?
    pub label: Option<DiagMessage>,
}

/// A collection of `Span`s.
///
/// Spans have two orthogonal attributes:
///
/// - They can be *primary spans*. In this case they are the locus of
///   the error, and would be rendered with `^^^`.
/// - They can have a *label*. In this case, the label is written next
///   to the mark in the snippet when we render.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Encodable, Decodable, StableHash)]
pub struct MultiSpan {
    primary_spans: Vec<Span>,
    span_labels: Vec<(Span, DiagMessage)>,
}

impl MultiSpan {
    #[inline]
    pub fn new() -> MultiSpan {
        MultiSpan { primary_spans: vec![], span_labels: vec![] }
    }

    pub fn from_span(primary_span: Span) -> MultiSpan {
        MultiSpan { primary_spans: vec![primary_span], span_labels: vec![] }
    }

    pub fn from_spans(mut vec: Vec<Span>) -> MultiSpan {
        vec.sort();
        MultiSpan { primary_spans: vec, span_labels: vec![] }
    }

    pub fn push_primary_span(&mut self, primary_span: Span) {
        self.primary_spans.push(primary_span);
    }

    pub fn push_span_label(&mut self, span: Span, label: impl Into<DiagMessage>) {
        self.span_labels.push((span, label.into()));
    }

    pub fn push_span_diag(&mut self, span: Span, diag: DiagMessage) {
        self.span_labels.push((span, diag));
    }

    /// Selects the first primary span (if any).
    pub fn primary_span(&self) -> Option<Span> {
        self.primary_spans.first().cloned()
    }

    /// Returns all primary spans.
    pub fn primary_spans(&self) -> &[Span] {
        &self.primary_spans
    }

    /// Returns `true` if any of the primary spans are displayable.
    pub fn has_primary_spans(&self) -> bool {
        !self.is_dummy()
    }

    /// Returns `true` if this contains only a dummy primary span with any hygienic context.
    pub fn is_dummy(&self) -> bool {
        self.primary_spans.iter().all(|sp| sp.is_dummy())
    }

    /// Replaces all occurrences of one Span with another. Used to move `Span`s in areas that don't
    /// display well (like std macros). Returns whether replacements occurred.
    pub fn replace(&mut self, before: Span, after: Span) -> bool {
        let mut replacements_occurred = false;
        for primary_span in &mut self.primary_spans {
            if *primary_span == before {
                *primary_span = after;
                replacements_occurred = true;
            }
        }
        for span_label in &mut self.span_labels {
            if span_label.0 == before {
                span_label.0 = after;
                replacements_occurred = true;
            }
        }
        replacements_occurred
    }

    /// Returns the strings to highlight. We always ensure that there
    /// is an entry for each of the primary spans -- for each primary
    /// span `P`, if there is at least one label with span `P`, we return
    /// those labels (marked as primary). But otherwise we return
    /// `SpanLabel` instances with empty labels.
    pub fn span_labels(&self) -> Vec<SpanLabel> {
        let is_primary = |span| self.primary_spans.contains(&span);

        let mut span_labels = self
            .span_labels
            .iter()
            .map(|&(span, ref label)| SpanLabel {
                span,
                is_primary: is_primary(span),
                label: Some(label.clone()),
            })
            .collect::<Vec<_>>();

        for &span in &self.primary_spans {
            if !span_labels.iter().any(|sl| sl.span == span) {
                span_labels.push(SpanLabel { span, is_primary: true, label: None });
            }
        }

        span_labels
    }

    /// Returns the span labels as contained by `MultiSpan`.
    pub fn span_labels_raw(&self) -> &[(Span, DiagMessage)] {
        &self.span_labels
    }

    /// Returns `true` if any of the span labels is displayable.
    pub fn has_span_labels(&self) -> bool {
        self.span_labels.iter().any(|(sp, _)| !sp.is_dummy())
    }

    /// Clone this `MultiSpan` without keeping any of the span labels - sometimes a `MultiSpan` is
    /// to be re-used in another diagnostic, but includes `span_labels` which have translated
    /// messages. These translated messages would fail to translate without their diagnostic
    /// arguments which are unlikely to be cloned alongside the `Span`.
    pub fn clone_ignoring_labels(&self) -> Self {
        Self { primary_spans: self.primary_spans.clone(), ..MultiSpan::new() }
    }
}

impl From<Span> for MultiSpan {
    fn from(span: Span) -> MultiSpan {
        MultiSpan::from_span(span)
    }
}

impl From<Vec<Span>> for MultiSpan {
    fn from(spans: Vec<Span>) -> MultiSpan {
        MultiSpan::from_spans(spans)
    }
}

/// One entry of a diagnostic's argument map.
pub type DiagArg<'iter> = (&'iter DiagArgName, &'iter DiagArgValue);

/// Name of a diagnostic argument.
pub type DiagArgName = Cow<'static, str>;

/// The value of a diagnostic argument. It is deliberately a small closed set rather than an open
/// `Display`: it has to implement `Encodable` and `Decodable`, because a diagnostic crosses the
/// wire between the compiler and whatever is showing it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encodable, Decodable)]
pub enum DiagArgValue {
    Str(Cow<'static, str>),
    // An `i32` is what the select rules are written against: `{$count -> [1] ... *[other] ...}`.
    // Anything bigger is converted to a string in `into_diag_arg` and stored in `Str`.
    Number(i32),
    StrListSepByAnd(Vec<Cow<'static, str>>),
}

/// A mapping from diagnostic argument names to their values.
/// This contains all the arguments necessary to format a diagnostic message.
pub type DiagArgMap = FxIndexMap<DiagArgName, DiagArgValue>;

/// Converts a value of a type into a `DiagArg` (typically a field of an `Diag` struct).
/// Implemented as a custom trait rather than `From` so that it is implemented on the type being
/// converted rather than on `DiagArgValue`, which enables types from other `rustc_*` crates to
/// implement this.
pub trait IntoDiagArg {
    /// Convert `Self` into a `DiagArgValue` suitable for rendering in a diagnostic.
    ///
    /// It takes a `path` where "long values" could be written to, if the `DiagArgValue` is too big
    /// for displaying on the terminal. This path comes from the `Diag` itself. When rendering
    /// values that come from `TyCtxt`, like `Ty<'_>`, they can use `TyCtxt::short_string`. If a
    /// value has no shortening logic that could be used, the argument can be safely ignored.
    fn into_diag_arg(self, path: &mut LongTyPath) -> DiagArgValue;
}

/// The full text of an over-long rendered value, threaded through every `into_diag_arg`.
///
/// **This was `Option<PathBuf>` and it named a file.** `short_string_namespace` wrote the long
/// type to `long-type-<hash>.txt`, read the file back to avoid duplicate lines, appended, and
/// handed back the path so a diagnostic could say where it went. That names a file on the
/// compiler process's own disk, which is the wrong place whenever whoever reads the diagnostic is
/// not sitting on that disk: another process, another machine, or a test harness comparing output.
///
/// So the value travels instead of a path to it. A caller that wants a file can still write one,
/// and now it is the caller's file in the caller's directory.
///
/// The name is kept. It is `_` at thirty of its thirty-one sites, and renaming it would be
/// thirty edits to files that do nothing with it - which is the same reason it was an alias in
/// the first place.
pub type LongTyPath = Option<String>;

/// Re-exported for `into_diag_arg_using_display!`, whose body calls `to_string`. The trait is in
/// std's prelude but not core's, so a `no_std` caller of that macro has no such method in scope,
/// and it cannot name `alloc::string::ToString` unless it declared `extern crate alloc`.
pub use alloc::string::ToString;

impl IntoDiagArg for DiagArgValue {
    fn into_diag_arg(self, _: &mut LongTyPath) -> DiagArgValue {
        self
    }
}

/// Render an inline message template against a diagnostic's arguments.
///
/// This is the whole of what `fluent-bundle` was doing here. The old path built a
/// `FluentBundle`, parsed the template into a one-message `FluentResource`, registered the
/// `STREQ` function, converted the argument map into `FluentArgs` and formatted the pattern -
/// once per message rendered. `frontend_diag_template` does the same parse and the same
/// substitution with no locale negotiation, no ICU data and no `std`.
///
/// Both failure modes were panics before and are panics now. A template that does not parse
/// cannot reach here: the derive and `msg!` parse every one at compile time. A missing argument
/// is a diagnostic that was built wrong, and rendering `{$name}` into a user's terminal is worse
/// than stopping.
pub fn render_message(template: &str, args: &DiagArgMap) -> Cow<'static, str> {
    let pattern = frontend_diag_template::parse(template).unwrap_or_else(|err| {
        panic!(
            "diagnostic message failed to parse at byte {}: {}\nmessage: {template:?}",
            err.offset, err.message
        )
    });
    let rendered = frontend_diag_template::render(&pattern, &TemplateArgs(args));
    if !rendered.unresolved.is_empty() {
        panic!(
            "diagnostic message refers to arguments that were not set: {:?}\nmessage: {template:?}",
            rendered.unresolved
        );
    }
    Cow::Owned(rendered.text)
}

/// The same substitution, for a caller that has to keep going when it cannot be done.
///
/// `render_message` panics on both failure modes because its callers are the eager formatter
/// and the JSON emitter, where a diagnostic built wrong is a compiler bug worth stopping for.
/// An emitter whose output is consumed programmatically is not in that position: it is often holding the *only*
/// record of what went wrong, and taking the process down loses it along with every other
/// diagnostic of the compile.
///
/// So this returns `None` for the two conditions the other one panics on, and the caller
/// decides. A consumer-supplied sink and `PlainEmitter` both fall back to printing the template
/// beside its arguments, which is what they used to print unconditionally.
pub fn try_render_message(template: &str, args: &DiagArgMap) -> Option<String> {
    let pattern = frontend_diag_template::parse(template).ok()?;
    let rendered = frontend_diag_template::render(&pattern, &TemplateArgs(args));
    if rendered.unresolved.is_empty() { Some(rendered.text) } else { None }
}

/// The bridge from a diagnostic's argument map to the renderer's value type.
struct TemplateArgs<'a>(&'a DiagArgMap);

impl frontend_diag_template::Args for TemplateArgs<'_> {
    fn get(&self, name: &str) -> Option<frontend_diag_template::Value<'_>> {
        Some(match self.0.get(name)? {
            DiagArgValue::Str(s) => frontend_diag_template::Value::Str(Cow::Borrowed(s)),
            DiagArgValue::Number(n) => frontend_diag_template::Value::Number(*n),
            // A list arrives at the renderer already joined. Fluent carried it as a custom value
            // that only stringified when written, which had one visible consequence: a `select`
            // on a list argument matched no variant key and always took the default branch.
            // Here it is a string, so such a select could now match a key. No message in the
            // tree selects on a list argument, and matching is the less surprising of the two.
            DiagArgValue::StrListSepByAnd(list) => {
                frontend_diag_template::Value::Str(Cow::Owned(join_with_and(list)))
            }
        })
    }
}

/// Join a list the way `icu_list`'s English "wide" and-list did, which is the only locale the
/// bundle ever held: `a`, `a and b`, `a, b, and c`. The serial comma is CLDR's for English, and
/// dropping it would change existing diagnostic output.
fn join_with_and(list: &[Cow<'static, str>]) -> String {
    let mut out = String::new();
    for (i, item) in list.iter().enumerate() {
        if i > 0 {
            if list.len() == 2 {
                out.push_str(" and ");
            } else if i + 1 == list.len() {
                out.push_str(", and ");
            } else {
                out.push_str(", ");
            }
        }
        out.push_str(item);
    }
    out
}
pub use crate::into_diag_arg_using_display;
