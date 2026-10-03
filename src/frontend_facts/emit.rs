use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_errors::emitter::Emitter;
use crate::rustc_errors::formatting::{diag_message_source, try_format_diag_message};
use crate::rustc_errors::{Applicability as RustcApplicability, DiagInner, Level};
use crate::rustc_span::source_map::SourceMap;
use crate::rustc_span::{BytePos, Span};

use super::{Applicability, ByteSpan, Severity, Suggestion, SuggestionPart, TypeDiagnostic};

#[derive(Default)]
pub struct CapturedDiagnostics {
    pub typed: Vec<TypeDiagnostic>,
    pub(super) rendered: Vec<String>,
}

pub(super) struct FactEmitter {
    sm: Arc<SourceMap>,
    captured: Arc<eko::thread::Mutex<CapturedDiagnostics>>,
}

impl FactEmitter {
    pub(super) fn new(
        sm: Arc<SourceMap>,
        captured: Arc<eko::thread::Mutex<CapturedDiagnostics>>,
    ) -> Self {
        FactEmitter { sm, captured }
    }
}

impl Emitter for FactEmitter {
    fn emit_diagnostic(&mut self, diag: DiagInner) {
        let level = diag.level();
        let Some(severity) = severity(level) else { return };
        let message = message(&diag);
        let code = diag
            .code
            .map(|code| code.to_string())
            .or_else(|| diag.is_lint.as_ref().map(|lint| lint.name.clone()));
        // Keep the enclosing source expression when a diagnostic marks inner types too.
        let primary = diag
            .span
            .primary_spans()
            .iter()
            .copied()
            .max_by_key(|span| span.hi().0.saturating_sub(span.lo().0));
        let primary = primary.map(|span| {
            if code.as_deref() == Some("E0061")
                && let Some(extra) = self
                    .sm
                    .span_to_next_source(span)
                    .ok()
                    .and_then(|source| call_expression_end(&source))
            {
                return span.with_hi(span.hi() + BytePos(extra));
            }
            span
        });
        let display_code = diag.code.is_some();
        if primary.is_none()
            && diag.code.is_none()
            && diag.is_lint.is_none()
            && is_summary(level, &message)
        {
            return;
        }

        let (span, span_known) = byte_span(primary, Some(&self.sm));
        let suggestions = suggestions(&diag, &self.sm);
        let typed = TypeDiagnostic { code, severity, message, span, span_known, suggestions };
        let rendered = render(&typed, level, primary, Some(&self.sm), display_code);
        let mut captured = self.captured.lock();
        captured.typed.push(typed);
        captured.rendered.push(rendered);
    }

    fn source_map(&self) -> Option<&SourceMap> {
        Some(&self.sm)
    }
}

fn call_expression_end(source: &str) -> Option<u32> {
    use crate::rustc_lexer::{FrontmatterAllowed, TokenKind};

    let mut consumed = 0;
    let mut depth = 0;
    let mut started = false;
    for token in crate::rustc_lexer::tokenize(source, FrontmatterAllowed::No) {
        consumed += token.len;
        match token.kind {
            TokenKind::Whitespace
            | TokenKind::LineComment { .. }
            | TokenKind::BlockComment { .. }
                if !started => {}
            TokenKind::OpenParen => {
                started = true;
                depth += 1;
            }
            TokenKind::CloseParen if started => {
                depth -= 1;
                if depth == 0 {
                    return Some(consumed);
                }
            }
            _ if !started => return None,
            _ => {}
        }
    }
    None
}

fn suggestions(diag: &DiagInner, sm: &SourceMap) -> Vec<Suggestion> {
    diag.suggestions
        .clone()
        .unwrap_tag()
        .into_iter()
        .flat_map(|suggestion| {
            let message = try_format_diag_message(&suggestion.msg, &diag.args)
                .unwrap_or_else(|| Cow::Borrowed(diag_message_source(&suggestion.msg)))
                .into_owned();
            let applicability = match suggestion.applicability {
                RustcApplicability::MachineApplicable => Applicability::MachineApplicable,
                RustcApplicability::MaybeIncorrect => Applicability::MaybeIncorrect,
                RustcApplicability::HasPlaceholders => Applicability::HasPlaceholders,
                RustcApplicability::Unspecified => Applicability::Unspecified,
            };
            suggestion.substitutions.into_iter().map(move |substitution| Suggestion {
                applicability,
                message: message.clone(),
                parts: substitution
                    .parts
                    .into_iter()
                    .map(|part| {
                        let (span, span_known) = byte_span(Some(part.span), Some(sm));
                        SuggestionPart { span, span_known, replacement: part.snippet }
                    })
                    .collect(),
            })
        })
        .collect()
}

fn severity(level: Level) -> Option<Severity> {
    match level {
        Level::Bug | Level::Fatal | Level::Error | Level::DelayedBug => Some(Severity::Error),
        Level::ForceWarning | Level::Warning => Some(Severity::Warning),
        _ => None,
    }
}

fn message(diag: &DiagInner) -> String {
    diag.messages
        .iter()
        .map(|(message, _)| {
            let text = match try_format_diag_message(message, &diag.args) {
                Some(text) => text,
                None => Cow::Borrowed(diag_message_source(message)),
            };
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn byte_span(span: Option<Span>, sm: Option<&SourceMap>) -> (ByteSpan, bool) {
    if let Some(span) = span.filter(|span| !span.is_dummy())
        && let Some(sm) = sm
    {
        let file = sm.lookup_source_file(span.lo());
        let start = file.original_relative_byte_pos(span.lo()).0;
        let end = file.original_relative_byte_pos(span.hi()).0;
        let name = file.name.prefer_local_unconditionally().to_string();
        return (ByteSpan { file: Arc::from(name), start, end }, true);
    }

    let file = sm
        .and_then(|sm| {
            let files = sm.files();
            files
                .iter()
                .next()
                .map(|file| file.name.prefer_local_unconditionally().to_string())
        })
        .unwrap_or_default();
    (ByteSpan { file: Arc::from(file), start: 0, end: 0 }, false)
}

fn render(
    diagnostic: &TypeDiagnostic,
    level: Level,
    primary: Option<Span>,
    sm: Option<&SourceMap>,
    display_code: bool,
) -> String {
    let mut rendered = match level {
        Level::Bug | Level::DelayedBug => String::from("internal compiler error"),
        Level::Fatal | Level::Error => String::from("error"),
        Level::ForceWarning | Level::Warning => String::from("warning"),
        _ => unreachable!(),
    };
    if display_code && let Some(code) = &diagnostic.code {
        rendered.push('[');
        rendered.push_str(code);
        rendered.push(']');
    }
    rendered.push_str(": ");
    rendered.push_str(&diagnostic.message);
    if let (Some(span), Some(sm)) = (primary.filter(|span| !span.is_dummy()), sm) {
        rendered.push_str("\n  --> ");
        rendered.push_str(&sm.span_to_diagnostic_string(span));
    }
    rendered
}

fn is_summary(level: Level, message: &str) -> bool {
    match level {
        Level::Error | Level::Fatal | Level::Bug | Level::DelayedBug => {
            message.starts_with("aborting due to ")
        }
        Level::Warning | Level::ForceWarning => {
            message.ends_with(" warning emitted") || message.ends_with(" warnings emitted")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dummy_spans_are_unknown() {
        let mut diagnostic = DiagInner::new(Level::Error, "an error");
        diagnostic.span = crate::rustc_errors::MultiSpan::from_span(crate::rustc_span::DUMMY_SP);
        let (span, span_known) = byte_span(diagnostic.span.primary_span(), None);
        let typed = TypeDiagnostic {
            code: None,
            severity: Severity::Error,
            message: message(&diagnostic),
            span,
            span_known,
            suggestions: Vec::new(),
        };
        assert!(!typed.span_known);
        assert_eq!(typed.span.start, 0);
        assert_eq!(typed.span.end, 0);
    }

    #[test]
    fn e0061_call_span_includes_nested_arguments() {
        let source = " /* gap */ (nested(1), \")\") trailing";
        assert_eq!(call_expression_end(source), source.find(" trailing").map(|end| end as u32));
    }
}
