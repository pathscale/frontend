// `#![no_std]`: these arrive with the standard prelude and name no path, so a `std::`
// search cannot see them - and a `#[derive]` can use them without the name appearing
// in this file at all, which is why they are not trimmed by inspection.
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use alloc::borrow::Cow;

use crate::rustc_error_messages::{DiagArgValue, IntoDiagArg};
use rustc_macros::Subdiagnostic;
use crate::rustc_span::{Span, Symbol};

use crate::rustc_errors::diagnostic::DiagLocation;
use crate::rustc_errors::{Diag, EmissionGuarantee, Subdiagnostic};

impl IntoDiagArg for DiagLocation {
    fn into_diag_arg(self, _: &mut crate::rustc_error_messages::LongTyPath) -> DiagArgValue {
        DiagArgValue::Str(Cow::from(self.to_string()))
    }
}

#[derive(Clone)]
pub struct DiagSymbolList<S = Symbol>(Vec<S>);

impl<S> From<Vec<S>> for DiagSymbolList<S> {
    fn from(v: Vec<S>) -> Self {
        DiagSymbolList(v)
    }
}

impl<S> FromIterator<S> for DiagSymbolList<S> {
    fn from_iter<T: IntoIterator<Item = S>>(iter: T) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

impl<S: core::fmt::Display> IntoDiagArg for DiagSymbolList<S> {
    fn into_diag_arg(self, _: &mut crate::rustc_error_messages::LongTyPath) -> DiagArgValue {
        DiagArgValue::StrListSepByAnd(
            self.0.into_iter().map(|sym| Cow::Owned(format!("`{sym}`"))).collect(),
        )
    }
}

/// Utility struct used to apply a single label while highlighting multiple spans
pub struct SingleLabelManySpans {
    pub spans: Vec<Span>,
    pub label: &'static str,
}
impl Subdiagnostic for SingleLabelManySpans {
    fn add_to_diag<G: EmissionGuarantee>(self, diag: &mut Diag<'_, G>) {
        diag.span_labels(self.spans, self.label);
    }
}

#[derive(Subdiagnostic)]
#[label(
    "expected lifetime {$count ->
        [1] parameter
        *[other] parameters
    }"
)]
pub struct ExpectedLifetimeParameter {
    #[primary_span]
    pub span: Span,
    pub count: usize,
}

#[derive(Subdiagnostic)]
#[suggestion(
    "indicate the anonymous {$count ->
        [1] lifetime
        *[other] lifetimes
    }",
    code = "{suggestion}",
    style = "verbose"
)]
pub struct IndicateAnonymousLifetime {
    #[primary_span]
    pub span: Span,
    pub count: usize,
    pub suggestion: String,
}

#[derive(Subdiagnostic)]
pub struct ElidedLifetimeInPathSubdiag {
    #[subdiagnostic]
    pub expected: ExpectedLifetimeParameter,
    #[subdiagnostic]
    pub indicate: Option<IndicateAnonymousLifetime>,
}
