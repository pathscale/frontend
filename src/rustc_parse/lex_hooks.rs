//! Hooks for the lexer examples (`examples/lex_bench.rs`, `examples/lex_oracle.rs`):
//! `frontend_lex` and the eager diagnostic lexer, outside a parser.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_ast::token;
use crate::rustc_ast::tokenstream::lexed_tape::LexedTape;
use crate::rustc_ast::tokenstream::{Spacing, TokenCursor};
use crate::rustc_lexer::FrontmatterAllowed;
use crate::rustc_session::parse::ParseSess;
use crate::rustc_span::edition::Edition;
use crate::rustc_span::{BytePos, FileName, Span, Symbol};

use super::lexer::{StripTokens, lex_flat_tokens};

/// `frontend_lex` over `src`: its token count, or the error it stopped at. Needs session
/// globals: it interns.
pub fn lex(src: &Arc<String>, edition: Edition) -> Result<usize, frontend_lex::LexError> {
    let options = frontend_lex::Options { edition, strip_shebang: false, frontmatter: false };
    let file = crate::rustc_span::with_session_interner(|interner| {
        frontend_lex::lex(Arc::clone(src), options, interner)
    });
    match file.error {
        Some(error) => Err(error),
        None => Ok(file.tokens.len()),
    }
}

/// The eager diagnostic lexer over `src`: whether it lexed it without an error. Needs session
/// globals.
pub fn eager(src: &Arc<String>) -> bool {
    let psess = ParseSess::new();
    let (src, base) = registered(&psess, src);
    match lex_flat_tokens(&psess, &src, Some(&src), base, None, StripTokens::Nothing) {
        Ok(_) => true,
        Err(errors) => {
            errors.into_iter().for_each(|diag| diag.cancel());
            false
        }
    }
}

/// `src` registered as a file of `psess`'s source map, as the parser lexes it: its text (a BOM
/// and CRLF line ends normalized) and where it starts. The eager lexer looks up the spans it
/// reports in the source map.
fn registered(psess: &ParseSess, src: &Arc<String>) -> (Arc<String>, BytePos) {
    let name = FileName::Custom(String::from("lex_hooks"));
    let file = psess.source_map().new_source_file(name, String::clone(src));
    (file.src.clone().expect("a new file keeps its text"), file.start_pos)
}

/// How the eager diagnostic lexer and `frontend_lex` compare on one source.
#[derive(Debug)]
pub enum Compared {
    /// Both accept it and the parser sees the same thing: this many tokens.
    Same(usize),
    /// Both report an error.
    BothRefuse,
    /// `frontend_lex` stops at this error and the eager lexer accepts it: the parser falls back
    /// to the eager lexer, which also lints (reserved prefixes before 2021, for one).
    OnlyEagerAccepts(frontend_lex::LexError),
    /// `frontend_lex` accepts what the eager lexer reports an error in.
    OnlyNewAccepts,
    /// Both accept it and differ: the first difference.
    Differ(String),
}

/// Lexes `src` both ways and compares what the parser reads: every token with its span and
/// spacing through the parser's own cursor, the raw identifier spans, and the symbol gallery.
/// Needs session globals.
pub fn compare(src: &Arc<String>, edition: Edition) -> Compared {
    let psess = ParseSess::new();
    let (src, base) = registered(&psess, src);
    let eager = lex_flat_tokens(&psess, &src, Some(&src), base, None, StripTokens::Nothing);
    let new = LexedTape::build(Arc::clone(&src), base, 0, FrontmatterAllowed::No, edition);
    let (eager, new) = match (eager, new) {
        (Err(errors), new) => {
            errors.into_iter().for_each(|diag| diag.cancel());
            return if new.is_some() { Compared::OnlyNewAccepts } else { Compared::BothRefuse };
        }
        (Ok(_), None) => {
            let error = lex(&src, edition).expect_err("the tape refuses only a lexing error");
            return Compared::OnlyEagerAccepts(error);
        }
        (Ok(eager), Some(new)) => (eager, new),
    };
    let raw: Vec<Span> = (0..).map_while(|i| psess.raw_identifier_spans.get(i)).collect();
    if raw != new.raw_identifier_spans() {
        return Compared::Differ(format!(
            "raw identifiers {raw:?} vs {:?}",
            new.raw_identifier_spans()
        ));
    }
    let gallery = |psess: &ParseSess| -> Vec<(Symbol, Span)> {
        psess.symbol_gallery.symbols.lock().iter().map(|(&s, &span)| (s, span)).collect()
    };
    let new_psess = ParseSess::new();
    new.record_symbol_gallery(&new_psess.symbol_gallery);
    let (a, b) = (gallery(&psess), gallery(&new_psess));
    if a != b {
        let at = a.iter().zip(&b).position(|(x, y)| x != y);
        return Compared::Differ(format!(
            "gallery ({} vs {} entries) differs at {at:?}",
            a.len(),
            b.len()
        ));
    }
    let mut old = TokenCursor::from_flat(eager);
    let mut cursor = TokenCursor::from_lexed_tape(new);
    let mut count = 0;
    loop {
        let (a, b): ((token::Token, Spacing), (token::Token, Spacing)) =
            (old.next_and_bump(), cursor.next_and_bump());
        if a.0.kind == token::Eof || b.0.kind == token::Eof {
            // The parser places EOF itself, so only its kind is compared.
            if a.0.kind != b.0.kind {
                return Compared::Differ(format!("token {count}: {a:?} vs {b:?}"));
            }
            return Compared::Same(count);
        }
        if a != b {
            return Compared::Differ(format!("token {count}: {a:?} vs {b:?}"));
        }
        count += 1;
    }
}
