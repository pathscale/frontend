use std::cell::RefCell;

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::spanned::Spanned;
use syn::{Attribute, Error as SynError, Meta};

// Errors reported by the derives go here rather than straight to the compiler. The direct route
// is `proc_macro::Diagnostic`, which is nightly-only (`proc_macro_diagnostic`), and this crate
// builds on stable. The stable route is a `compile_error!` in the expansion, which needs the
// errors in hand when the output is assembled - so `Diagnostic::emit` records here, and
// `with_emitted_errors` turns the record into tokens at each macro's entry point.
//
// A thread-local, not a parameter: `emit()` is called from about eighty sites several frames
// below the entry point, and threading a sink through all of them would touch every signature
// for no gain. One expansion runs on one thread from start to finish.
thread_local! {
    static EMITTED: RefCell<Vec<SynError>> = const { RefCell::new(Vec::new()) };
}

/// Runs one macro expansion and appends every error it emitted, as `compile_error!` tokens.
///
/// The record is cleared first as well as drained after, because an expansion that panicked
/// never reached the drain, and its errors would otherwise surface in the next, unrelated one.
pub(crate) fn with_emitted_errors(expand: impl FnOnce() -> TokenStream) -> TokenStream {
    EMITTED.with(|e| e.borrow_mut().clear());
    let mut tokens = expand();
    let emitted = EMITTED.with(|e| std::mem::take(&mut *e.borrow_mut()));
    // Combined into one `syn::Error` so each keeps its own span; `to_compile_error` then yields
    // one `compile_error!` per message.
    let mut iter = emitted.into_iter();
    if let Some(mut first) = iter.next() {
        for err in iter {
            first.combine(err);
        }
        tokens.extend(first.to_compile_error());
    }
    tokens
}

/// A stable stand-in for the builder `proc_macro::Diagnostic` used to provide.
///
/// It keeps that type's method names so the reporting sites read as they did. What it cannot
/// keep is the sub-diagnostic structure: `compile_error!` carries one string and one span, so a
/// `help` or `note` is folded into the error's text as a `= help:` / `= note:` line, the way
/// rustc would print it. A `span_note` becomes an error of its own at the note's span, because
/// the location is the point of such a note and folding it into text would lose it.
#[must_use]
pub(crate) struct Diagnostic {
    span: Span,
    message: String,
    span_notes: Vec<(Span, String)>,
}

impl Diagnostic {
    fn spanned(span: impl Into<Span>, message: String) -> Self {
        Diagnostic { span: span.into(), message, span_notes: Vec::new() }
    }

    pub(crate) fn help<T: Into<String>>(mut self, msg: T) -> Self {
        self.message.push_str("\n= help: ");
        self.message.push_str(&msg.into());
        self
    }

    pub(crate) fn note<T: Into<String>>(mut self, msg: T) -> Self {
        self.message.push_str("\n= note: ");
        self.message.push_str(&msg.into());
        self
    }

    pub(crate) fn span_note<T: Into<String>>(mut self, span: impl Into<Span>, msg: T) -> Self {
        self.span_notes.push((span.into(), format!("note: {}", msg.into())));
        self
    }

    /// Records the error for `with_emitted_errors` to put in the expansion.
    pub(crate) fn emit(self) {
        let mut err = SynError::new(self.span, self.message);
        for (span, note) in self.span_notes {
            err.combine(SynError::new(span, note));
        }
        EMITTED.with(|e| e.borrow_mut().push(err));
    }
}

#[derive(Debug)]
pub(crate) enum DiagnosticDeriveError {
    SynError(SynError),
    ErrorHandled,
}

impl DiagnosticDeriveError {
    pub(crate) fn to_compile_error(self) -> TokenStream {
        match self {
            DiagnosticDeriveError::SynError(e) => e.to_compile_error(),
            DiagnosticDeriveError::ErrorHandled => {
                // Return ! to avoid having to create a blank Diag to return when an
                // error has already been emitted to the compiler.
                quote! {
                    { unreachable!(); }
                }
            }
        }
    }
}

impl From<SynError> for DiagnosticDeriveError {
    fn from(e: SynError) -> Self {
        DiagnosticDeriveError::SynError(e)
    }
}

/// Helper function for use with `throw_*` macros - constraints `$f` to an `impl FnOnce`.
pub(crate) fn _throw_err(
    diag: Diagnostic,
    f: impl FnOnce(Diagnostic) -> Diagnostic,
) -> DiagnosticDeriveError {
    f(diag).emit();
    DiagnosticDeriveError::ErrorHandled
}

/// Helper function for printing `syn::Path` - doesn't handle arguments in paths and these are
/// unlikely to come up much in use of the macro.
fn path_to_string(path: &syn::Path) -> String {
    let mut out = String::new();
    for (i, segment) in path.segments.iter().enumerate() {
        if i > 0 || path.leading_colon.is_some() {
            out.push_str("::");
        }
        out.push_str(&segment.ident.to_string());
    }
    out
}

/// Returns an error diagnostic on span `span` with msg `msg`.
#[must_use]
pub(crate) fn span_err<T: Into<String>>(span: impl Into<Span>, msg: T) -> Diagnostic {
    Diagnostic::spanned(span, format!("derive(Diagnostic): {}", msg.into()))
}

/// Emit a diagnostic on span `$span` with msg `$msg` (optionally performing additional decoration
/// using the `FnOnce` passed in `diag`) and return `Err(ErrorHandled)`.
///
/// For methods that return a `Result<_, DiagnosticDeriveError>`:
macro_rules! throw_span_err {
    ($span:expr, $msg:expr) => {{ throw_span_err!($span, $msg, |diag| diag) }};
    ($span:expr, $msg:expr, $f:expr) => {{
        let diag = span_err($span, $msg);
        return Err(crate::diagnostics::error::_throw_err(diag, $f));
    }};
}

pub(crate) use throw_span_err;

/// Returns an error diagnostic for an invalid attribute.
pub(crate) fn invalid_attr(attr: &Attribute) -> Diagnostic {
    let span = attr.span().unwrap();
    let path = path_to_string(attr.path());
    match attr.meta {
        Meta::Path(_) => span_err(span, format!("`#[{path}]` is not a valid attribute")),
        Meta::NameValue(_) => span_err(span, format!("`#[{path} = ...]` is not a valid attribute")),
        Meta::List(_) => span_err(span, format!("`#[{path}(...)]` is not a valid attribute")),
    }
}

/// Emit an error diagnostic for an invalid attribute (optionally performing additional decoration
/// using the `FnOnce` passed in `diag`) and return `Err(ErrorHandled)`.
///
/// For methods that return a `Result<_, DiagnosticDeriveError>`:
macro_rules! throw_invalid_attr {
    ($attr:expr) => {{ throw_invalid_attr!($attr, |diag| diag) }};
    ($attr:expr, $f:expr) => {{
        let diag = crate::diagnostics::error::invalid_attr($attr);
        return Err(crate::diagnostics::error::_throw_err(diag, $f));
    }};
}

pub(crate) use throw_invalid_attr;
