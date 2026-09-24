//! Does this text parse as one kind of Rust syntax?
//!
//! The question a formatter asks before touching a selection, or an editor asks before offering
//! "extract to variable": is this exactly one expression, one type, one item? It is answered here
//! by rustc's own parser, so the answer is the one the compiler would give, not an approximation
//! of it.
//!
//! **Only the parser runs.** There is no `Session`, no name resolution, no macro expansion, no
//! type checking, and nothing is read from disk. A bare [`ParseSess`] and a [`Parser`] over the
//! text are the entire machine, so no sysroot and no library are involved at any point (rule zero
//! in `AGENTS.md`). That also makes a call cheap enough to repeat per keystroke.
//!
//! **Parse, not validity.** `fn f() -> u8 { "x" }` parses as an item; whether it type checks is
//! [`super::check_source`]'s question. A macro invocation parses as its call syntax, and its
//! body is never looked at, because looking at it is expansion.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::token;
use crate::rustc_errors::plain_emitter::PlainEmitter;
use crate::rustc_errors::{DiagCtxt, PResult};
use crate::rustc_parse::lexer::StripTokens;
use crate::rustc_parse::new_parser_from_source_str;
use crate::rustc_parse::parser::{
    AllowConstBlockItems, AttemptLocalParseRecovery, CommaRecoveryMode, ForceCollect, Parser,
    RecoverColon, RecoverComma,
};
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::fatal_error::catch_fatal_errors;
use crate::rustc_span::source_map::{FilePathMapping, SourceMap};
use crate::rustc_span::{FileName, create_session_if_not_set_then};
use serde::{Deserialize, Serialize};

/// The nonterminal kind a piece of text is checked against.
///
/// Each maps to the parser entry point rustc itself uses for that position, so "parses as a
/// `Type`" means "would parse where rustc expects a type".
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fragment {
    /// One expression, as on the right of `let x =`. A block expression such as `{ 1 }` counts.
    Expr,
    /// One braced block, `{ ... }`, as a function body.
    Block,
    /// One statement, including the semicolon it needs: `let x = 1;` is a statement and
    /// `let x = 1` is not, because a `let` cannot end without one.
    Stmt,
    /// Exactly one item: a `fn`, `struct`, `impl`, `use`, `mod`, and so on.
    Item,
    /// An entire file or module body: any number of items, inner attributes first. Empty text
    /// is a valid module, so it parses. A leading `#!` shebang line is skipped, as it is for a
    /// file on disk.
    Items,
    /// One type, as after `let x:`.
    Type,
    /// One pattern, as in a `match` arm or after `let`. Top-level alternatives (`A | B`) are
    /// accepted, as they are in both of those positions; a trailing `if` guard is not, because
    /// a guard belongs to the arm, not to the pattern.
    Pat,
}

/// Whether `source` is exactly one `kind`, with nothing left over.
///
/// `Ok(())` means the parser read all of `source` as that kind and said nothing against it.
/// `Err` carries every error the lexer and parser emitted, one string each, in emission order;
/// it is never empty. Text that is a valid `kind` followed by more text (`1 + 2 3`) is `Err`,
/// since the point is that the entire selection is the thing, not that it starts with one.
/// Warnings do not refuse the text and are not reported.
///
/// Parsed as edition 2024, so `async`, `dyn` and `gen` are keywords. When the calling thread is
/// already inside a compiler session, that session's globals, and so its edition, are used
/// instead: a thread holds one set of session globals at a time.
///
/// Callable any number of times on one thread; each call builds its own parse session and
/// shares nothing with the last. Needs a catcher installed through
/// [`crate::unwind_janky::install_catcher`], like [`super::analyze_source`]: some parser paths
/// end in `FatalError::raise`, which unwinds, and without a catcher that ends the process
/// instead of coming back as `Err`.
pub fn parses_as(source: &str, kind: Fragment) -> Result<(), Vec<String>> {
    assert!(
        crate::unwind_janky::unwinding_is_enabled(),
        "parses_as needs panic=unwind and a catcher installed through unwind_janky::install_catcher"
    );
    create_session_if_not_set_then(Edition::Edition2024, |_| parse_in_session(source, kind))
}

/// Whether `source` is an entire file or module body. Shorthand for [`Fragment::Items`].
pub fn parses(source: &str) -> Result<(), Vec<String>> {
    parses_as(source, Fragment::Items)
}

fn parse_in_session(source: &str, kind: Fragment) -> Result<(), Vec<String>> {
    // The diagnostics are the answer, so they go to a buffer rather than stderr. Same sink and
    // same emitter settings as `check_source`, so one line format serves both.
    let text = Arc::new(eko::thread::Mutex::new(String::new()));
    let sm = Arc::new(SourceMap::new(FilePathMapping::empty()));
    let emitter = PlainEmitter::new()
        .sm(Some(Arc::clone(&sm)))
        .short_message(true)
        .dst(Box::new(super::Sink(Arc::clone(&text))));
    let psess = ParseSess::with_dcx(DiagCtxt::new(Box::new(emitter)), sm);

    let finished = catch_fatal_errors(|| parse_all(&psess, source, kind));
    // A stashed diagnostic is otherwise emitted when the session drops, after the buffer has
    // been read, and would be lost from the answer.
    let _ = psess.dcx().emit_stashed_diagnostics();
    // The parser recovers from many mistakes: it emits the error and returns `Ok` with an
    // error node in the tree. So `Ok` from the entry point is not enough, and the error count
    // is what decides.
    let refused = finished.is_err() || psess.dcx().has_errors().is_some();

    // Read in place: only the errors kept become strings.
    let mut errors = error_entries(&text.lock());
    if !refused && errors.is_empty() {
        return Ok(());
    }
    if errors.is_empty() {
        // A fatal stop can come with nothing emitted; the caller still gets a reason.
        errors.push("error: the parser stopped without a diagnostic".to_string());
    }
    Err(errors)
}

/// Lex `source`, run the entry point for `kind`, and insist on end of input. Every error ends
/// up emitted into the session: a `Diag` dropped unemitted is a panic, not a silent loss.
fn parse_all(psess: &ParseSess, source: &str, kind: Fragment) {
    // Only a file can start with a shebang. In a fragment `#!` is the start of an inner
    // attribute, and stripping it would hide exactly the text being asked about.
    let strip = if kind == Fragment::Items { StripTokens::Shebang } else { StripTokens::Nothing };
    let name = FileName::anon_source_code(source);
    let mut parser = match new_parser_from_source_str(psess, name, source.to_string(), strip) {
        Ok(parser) => parser,
        // Unbalanced delimiters and bad tokens are refused while lexing, before a parser exists.
        Err(diags) => {
            for diag in diags {
                let _ = diag.emit();
            }
            return;
        }
    };
    let result = match parse_fragment(&mut parser, kind) {
        Ok(()) if parser.token == token::Eof => Ok(()),
        // The fragment ended early. `unexpected` names the token it stopped at, which is the
        // diagnostic rustc gives for trailing text anywhere else.
        Ok(()) => parser.unexpected(),
        Err(diag) => Err(diag),
    };
    if let Err(diag) = result {
        let _ = diag.emit();
    }
}

fn parse_fragment<'a>(parser: &mut Parser<'a>, kind: Fragment) -> PResult<'a, ()> {
    match kind {
        Fragment::Expr => parser.parse_expr().map(drop),
        Fragment::Block => parser.parse_block().map(drop),
        Fragment::Stmt => parser.parse_full_stmt(AttemptLocalParseRecovery::No).map(drop),
        Fragment::Item => match parser.parse_item(ForceCollect::No, AllowConstBlockItems::Yes)? {
            Some(_) => Ok(()),
            // Nothing that starts an item was found. Say so in rustc's words rather than ours.
            None => parser.unexpected(),
        },
        Fragment::Items => parser.parse_crate_mod().map(drop),
        Fragment::Type => parser.parse_ty().map(drop),
        // The same call rustc uses for a `$p:pat` fragment on edition 2021 and later, which is
        // the pattern syntax of `let` and `match` arms minus the guard.
        Fragment::Pat => parser
            .parse_pat_no_top_guard(
                None,
                RecoverComma::No,
                RecoverColon::No,
                CommaRecoveryMode::EitherTupleOrPipe,
            )
            .map(drop),
    }
}

/// Split the emitter's output into one string per error. An entry starts at a line with no
/// leading space and runs through the indented `-->` location line that follows it. Internal
/// compiler errors count: text the compiler could not handle has not been shown to parse.
fn error_entries(captured: &str) -> Vec<String> {
    super::split_diagnostics(captured).0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Parsing itself is exercised from `tests/syntax.rs`: it needs a catcher, and a catcher
    // needs `std`, which this crate does not name. What is left here needs neither.

    #[test]
    fn entries_keep_location_lines_and_drop_warnings() {
        let captured = "error: expected expression, found `<eof>`\n  --> a.rs:1:4\n\
                        warning: unused\n  --> a.rs:1:1\nerror: second\n";
        assert_eq!(
            error_entries(captured),
            vec![
                "error: expected expression, found `<eof>`\n  --> a.rs:1:4".to_string(),
                "error: second".to_string(),
            ]
        );
    }
}
