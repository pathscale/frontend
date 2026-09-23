//! Syntax-level diagnostics, ported from rust-analyzer's `ide-diagnostics`.
//!
//! Every check here is decided from the parsed AST alone: no macro expansion, no name
//! resolution, no type checking, and no sysroot. That makes them cheap enough to run on every
//! keystroke in an editor and usable by tooling that has a source string and nothing else.
//!
//! [`diagnose`] parses the source once with `rustc_parse`, inside a bare `ParseSess`, and then
//! runs a small constant number of linear passes over the AST. A check that is filtered out by
//! [`Options`] does not run at all.
//!
//! **Fatal parse errors are contained, not fatal to the process.** A few parser paths refuse a
//! program by unwinding (`FatalError::raise`), so [`diagnose`] runs inside
//! [`crate::rustc_span::fatal_error::catch_fatal_errors`]. That needs `panic = "unwind"` and a
//! catcher installed through [`crate::unwind_janky::install_catcher`], exactly as
//! [`super::analyze_source`] does, and it is asserted the same way.
//!
//! The diagnostic codes are rust-analyzer's diagnostic names, the ones its manual lists, so a
//! caller that already knows rust-analyzer's configuration can use the same strings here.
//! `structure.rs` lists which handler each code came from.

// `#![no_std]`: these arrive with the standard prelude and name no path.
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::rustc_ast::ast::Crate;
use crate::rustc_errors::DiagCtxt;
use crate::rustc_errors::plain_emitter::PlainEmitter;
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::new_parser_from_source_str;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::fatal_error::catch_fatal_errors;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{FileName, Span, create_session_if_not_set_then};

mod names;
mod structure;

/// How serious a diagnostic is. The same three levels rust-analyzer reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    WeakWarning,
}

impl Severity {
    fn rank(self) -> u8 {
        match self {
            Severity::WeakWarning => 0,
            Severity::Warning => 1,
            Severity::Error => 2,
        }
    }

    /// `self` is at least as serious as `min`.
    pub fn at_least(self, min: Severity) -> bool {
        self.rank() >= min.rank()
    }
}

/// One finding, with a byte range into the source [`diagnose`] was handed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// rust-analyzer's name for the diagnostic, e.g. `"break-outside-of-loop"`.
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub start: u32,
    pub end: u32,
}

/// Which checks run, per call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Options {
    /// Codes to run. `None` runs every check. A check whose code is not listed is skipped
    /// entirely, not run and filtered.
    pub codes: Option<Vec<String>>,
    /// Drop anything less serious than this. A check that can never reach it is skipped.
    pub min_severity: Severity,
}

impl Default for Options {
    fn default() -> Self {
        Options { codes: None, min_severity: Severity::WeakWarning }
    }
}

impl Options {
    /// Whether a check with this code, which can report at most `max` severity, should run.
    pub fn wants(&self, code: &str, max: Severity) -> bool {
        max.at_least(self.min_severity)
            && self.codes.as_ref().is_none_or(|codes| codes.iter().any(|c| c == code))
    }

    fn keeps(&self, d: &Diagnostic) -> bool {
        d.severity.at_least(self.min_severity)
            && self.codes.as_ref().is_none_or(|codes| codes.iter().any(|c| *c == d.code))
    }
}

/// What a check needs besides the AST: the source text, a way to turn a span into a byte
/// range of that text, and the caller's options.
pub(crate) struct Cx<'a> {
    pub source: &'a str,
    pub span: &'a dyn Fn(Span) -> (u32, u32),
    pub opts: &'a Options,
}

impl<'a> Cx<'a> {
    /// Shorthand for [`Options::wants`].
    pub fn wants(&self, code: &str, max: Severity) -> bool {
        self.opts.wants(code, max)
    }

    /// The source text a span covers. Empty for a span outside the source.
    pub fn text(&self, span: Span) -> &'a str {
        let (start, end) = (self.span)(span);
        self.source.get(start as usize..end as usize).unwrap_or("")
    }
}

/// Parse `source` as a crate and run every check. See [`diagnose_with`].
pub fn diagnose(source: &str) -> Result<Vec<Diagnostic>, Vec<String>> {
    diagnose_with(source, &Options::default())
}

/// Parse `source` as a crate and run the checks `opts` selects.
///
/// `Err` carries the parser's own diagnostics, one string each, when the source does not
/// parse; nothing is checked then, because the checks assume a well-formed tree. `Ok` is sorted
/// by start offset.
///
/// Needs a catcher installed through [`crate::unwind_janky::install_catcher`], because a few
/// parser paths refuse a program by unwinding.
pub fn diagnose_with(source: &str, opts: &Options) -> Result<Vec<Diagnostic>, Vec<String>> {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "diagnose needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    run(source, opts, |cx, krate, out| {
        structure::check(cx, krate, out);
        names::check(cx, krate, out);
    })
}

/// One parse, then `passes`. Split out so the unit tests can run one pass on its own.
fn run(
    source: &str,
    opts: &Options,
    passes: impl FnOnce(&Cx<'_>, &Crate, &mut Vec<Diagnostic>),
) -> Result<Vec<Diagnostic>, Vec<String>> {
    let text = Arc::new(eko::thread::Mutex::new(String::new()));
    let sink = text.clone();
    let finished = catch_fatal_errors(|| {
        // Reuses the caller's globals when there are some; otherwise builds the lightest set
        // there is, with no source map attached, because the parse session brings its own.
        create_session_if_not_set_then(Edition::Edition2024, |_| {
            let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
            let emitter = PlainEmitter::new()
                .sm(Some(sm.clone()))
                .short_message(true)
                .dst(Box::new(super::Sink(sink)));
            let psess = ParseSess::with_dcx(DiagCtxt::new(Box::new(emitter)), sm);
            let mut parser = match new_parser_from_source_str(
                &psess,
                FileName::anon_source_code(source),
                source.to_string(),
                StripTokens::ShebangAndFrontmatter,
            ) {
                Ok(parser) => parser,
                Err(errors) => {
                    for error in errors {
                        let _ = error.emit();
                    }
                    return None;
                }
            };
            let krate = match parser.parse_crate_mod() {
                Ok(krate) => krate,
                Err(error) => {
                    let _ = error.emit();
                    return None;
                }
            };
            drop(parser);
            // The parser recovers and keeps going, so a clean `Ok` can still carry errors.
            psess.dcx().emit_stashed_diagnostics();
            if psess.dcx().has_errors().is_some() {
                return None;
            }

            // One file in a fresh map: every span is that file's start plus an offset, so the
            // conversion is a subtraction rather than a source-map lookup.
            let base = psess.source_map().files().last().map_or(0, |sf| sf.start_pos.0);
            let len = source.len() as u32;
            let span = move |span: Span| {
                let lo = span.lo().0.saturating_sub(base).min(len);
                let hi = span.hi().0.saturating_sub(base).min(len);
                (lo, hi.max(lo))
            };
            let cx = Cx { source, span: &span, opts };
            let mut out = Vec::new();
            passes(&cx, &krate, &mut out);
            Some(out)
        })
    });

    match finished {
        Ok(Some(mut out)) => {
            out.retain(|d| opts.keeps(d));
            out.sort_by_key(|d| (d.start, d.end));
            Ok(out)
        }
        Ok(None) | Err(_) => {
            let errors = parse_errors(&text.lock());
            if errors.is_empty() {
                Err(vec!["error: the parser stopped without saying why".to_string()])
            } else {
                Err(errors)
            }
        }
    }
}

/// Split captured emitter output into one string per error. A diagnostic starts on a line with
/// no leading space; its location lines follow, indented. Warnings are left out: they do not
/// stop a parse.
fn parse_errors(captured: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let mut current: Option<String> = None;
    let mut flush = |entry: Option<String>, errors: &mut Vec<String>| {
        if let Some(entry) = entry
            && entry.starts_with("error")
        {
            errors.push(entry);
        }
    };
    for line in captured.lines() {
        if line.starts_with(' ') {
            if let Some(entry) = current.as_mut() {
                entry.push('\n');
                entry.push_str(line);
            }
        } else {
            flush(current.take(), &mut errors);
            current = Some(line.to_string());
        }
    }
    flush(current.take(), &mut errors);
    errors
}
